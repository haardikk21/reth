//! Transaction Pool Traits and Types
//!
//! This module defines the core abstractions for transaction pool implementations,
//! handling the complexity of different transaction representations across the
//! network, mempool, and the chain itself.
//!
//! ## Key Concepts
//!
//! ### Transaction Representations
//!
//! Transactions exist in different formats throughout their lifecycle:
//!
//! 1. **Consensus Format** ([`PoolTransaction::Consensus`])
//!    - The canonical format stored in blocks
//!    - Minimal size for efficient storage
//!    - Example: EIP-4844 transactions store only blob hashes: ([`TransactionSigned::Eip4844`])
//!
//! 2. **Pooled Format** ([`PoolTransaction::Pooled`])
//!    - Extended format for network propagation
//!    - Includes additional validation data
//!    - Example: EIP-4844 transactions include full blob sidecars: ([`PooledTransactionVariant`])
//!
//! ### Type Relationships
//!
//! ```text
//! NodePrimitives::SignedTx  ←──   NetworkPrimitives::BroadcastedTransaction
//!        │                              │
//!        │ (consensus format)           │ (announced to peers)
//!        │                              │
//!        └──────────┐  ┌────────────────┘
//!                   ▼  ▼
//!            PoolTransaction::Consensus
//!                   │ ▲
//!                   │ │ from pooled (always succeeds)
//!                   │ │
//!                   ▼ │ try_from consensus (may fail)
//!            PoolTransaction::Pooled  ←──→  NetworkPrimitives::PooledTransaction
//!                                             (sent on request)
//! ```
//!
//! ### Special Cases
//!
//! #### EIP-4844 Blob Transactions
//! - Consensus format: Only blob hashes (32 bytes each)
//! - Pooled format: Full blobs + commitments + proofs (large data per blob)
//! - Network behavior: Not broadcast automatically, only sent on explicit request
//!
//! #### Optimism Deposit Transactions
//! - Only exist in consensus format
//! - Never enter the mempool (system transactions)
//! - Conversion from consensus to pooled always fails

use crate::{
    blobstore::BlobStoreError,
    error::{InvalidPoolTransactionError, PoolError, PoolResult},
    pool::{
        state::SubPool, BestTransactionFilter, NewTransactionEvent, TransactionEvents,
        TransactionListenerKind,
    },
    validate::ValidPoolTransaction,
    AddedTransactionOutcome, AllTransactionsEvents,
};
use alloy_consensus::{error::ValueError, BlockHeader, Signed, Typed2718};
use alloy_eips::{
    eip2718::{Encodable2718, WithEncoded},
    eip2930::AccessList,
    eip4844::{
        env_settings::KzgSettings, BlobAndProofV1, BlobAndProofV2, BlobTransactionValidationError,
    },
    eip7594::BlobTransactionSidecarVariant,
    eip7702::SignedAuthorization,
};
use alloy_primitives::{Address, Bytes, TxHash, TxKind, B256, U256};
use futures_util::{ready, Stream};
use reth_eth_wire_types::HandleMempoolData;
use reth_ethereum_primitives::{PooledTransactionVariant, TransactionSigned};
use reth_execution_types::ChangedAccount;
use reth_primitives_traits::{Block, InMemorySize, Recovered, SealedBlock, SignedTransaction};
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fmt,
    fmt::Debug,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::mpsc::Receiver;

/// The `PeerId` type.
pub type PeerId = alloy_primitives::B512;

/// Helper type alias to access [`PoolTransaction`] for a given [`TransactionPool`].
pub type PoolTx<P> = <P as TransactionPool>::Transaction;
/// Helper type alias to access [`PoolTransaction::Consensus`] for a given [`TransactionPool`].
pub type PoolConsensusTx<P> = <<P as TransactionPool>::Transaction as PoolTransaction>::Consensus;

/// Helper type alias to access [`PoolTransaction::Pooled`] for a given [`TransactionPool`].
pub type PoolPooledTx<P> = <<P as TransactionPool>::Transaction as PoolTransaction>::Pooled;

/// General purpose abstraction of a transaction-pool.
///
/// This is intended to be used by API-consumers such as RPC that need inject new incoming,
/// unverified transactions. And by block production that needs to get transactions to execute in a
/// new block.
///
/// Note: This requires `Clone` for convenience, since it is assumed that this will be implemented
/// for a wrapped `Arc` type, see also [`Pool`](crate::Pool).
#[auto_impl::auto_impl(&, Arc)]
pub trait TransactionPool: Clone + Debug + Send + Sync {
    /// The transaction type of the pool
    type Transaction: EthPoolTransaction;

    /// Returns stats about the pool and all sub-pools.
    fn pool_size(&self) -> PoolSize;

    /// Returns the block the pool is currently tracking.
    ///
    /// This tracks the block that the pool has last seen.
    fn block_info(&self) -> BlockInfo;

    /// Imports an _external_ transaction.
    ///
    /// This is intended to be used by the network to insert incoming transactions received over the
    /// p2p network.
    ///
    /// Consumer: P2P
    fn add_external_transaction(
        &self,
        transaction: Self::Transaction,
    ) -> impl Future<Output = PoolResult<AddedTransactionOutcome>> + Send {
        self.add_transaction(TransactionOrigin::External, transaction)
    }

    /// Imports all _external_ transactions
    ///
    /// Consumer: Utility
    fn add_external_transactions(
        &self,
        transactions: Vec<Self::Transaction>,
    ) -> impl Future<Output = Vec<PoolResult<AddedTransactionOutcome>>> + Send {
        self.add_transactions(TransactionOrigin::External, transactions)
    }

    /// Adds an _unvalidated_ transaction into the pool and subscribe to state changes.
    ///
    /// This is the same as [`TransactionPool::add_transaction`] but returns an event stream for the
    /// given transaction.
    ///
    /// Consumer: Custom
    fn add_transaction_and_subscribe(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> impl Future<Output = PoolResult<TransactionEvents>> + Send;

    /// Adds an _unvalidated_ transaction into the pool.
    ///
    /// Consumer: RPC
    fn add_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> impl Future<Output = PoolResult<AddedTransactionOutcome>> + Send;

    /// Adds the given _unvalidated_ transaction into the pool.
    ///
    /// Returns a list of results.
    ///
    /// Consumer: RPC
    fn add_transactions(
        &self,
        origin: TransactionOrigin,
        transactions: Vec<Self::Transaction>,
    ) -> impl Future<Output = Vec<PoolResult<AddedTransactionOutcome>>> + Send;

    /// Submit a consensus transaction directly to the pool
    fn add_consensus_transaction(
        &self,
        tx: Recovered<<Self::Transaction as PoolTransaction>::Consensus>,
        origin: TransactionOrigin,
    ) -> impl Future<Output = PoolResult<AddedTransactionOutcome>> + Send {
        async move {
            let tx_hash = *tx.tx_hash();

            let pool_transaction = match Self::Transaction::try_from_consensus(tx) {
                Ok(tx) => tx,
                Err(e) => return Err(PoolError::other(tx_hash, e.to_string())),
            };

            self.add_transaction(origin, pool_transaction).await
        }
    }

    /// Submit a consensus transaction and subscribe to event stream
    fn add_consensus_transaction_and_subscribe(
        &self,
        tx: Recovered<<Self::Transaction as PoolTransaction>::Consensus>,
        origin: TransactionOrigin,
    ) -> impl Future<Output = PoolResult<TransactionEvents>> + Send {
        async move {
            let tx_hash = *tx.tx_hash();

            let pool_transaction = match Self::Transaction::try_from_consensus(tx) {
                Ok(tx) => tx,
                Err(e) => return Err(PoolError::other(tx_hash, e.to_string())),
            };

            self.add_transaction_and_subscribe(origin, pool_transaction).await
        }
    }

    /// Returns a new transaction change event stream for the given transaction.
    ///
    /// Returns `None` if the transaction is not in the pool.
    fn transaction_event_listener(&self, tx_hash: TxHash) -> Option<TransactionEvents>;

    /// Returns a new transaction change event stream for _all_ transactions in the pool.
    fn all_transactions_event_listener(&self) -> AllTransactionsEvents<Self::Transaction>;

    /// Returns a new Stream that yields transactions hashes for new __pending__ transactions
    /// inserted into the pool that are allowed to be propagated.
    ///
    /// Note: This is intended for networking and will __only__ yield transactions that are allowed
    /// to be propagated over the network, see also [`TransactionListenerKind`].
    ///
    /// Consumer: RPC/P2P
    fn pending_transactions_listener(&self) -> Receiver<TxHash> {
        self.pending_transactions_listener_for(TransactionListenerKind::PropagateOnly)
    }

    /// Returns a new [Receiver] that yields transactions hashes for new __pending__ transactions
    /// inserted into the pending pool depending on the given [`TransactionListenerKind`] argument.
    fn pending_transactions_listener_for(&self, kind: TransactionListenerKind) -> Receiver<TxHash>;

    /// Returns a new stream that yields new valid transactions added to the pool.
    fn new_transactions_listener(&self) -> Receiver<NewTransactionEvent<Self::Transaction>> {
        self.new_transactions_listener_for(TransactionListenerKind::PropagateOnly)
    }

    /// Returns a new [Receiver] that yields blob "sidecars" (blobs w/ assoc. kzg
    /// commitments/proofs) for eip-4844 transactions inserted into the pool
    fn blob_transaction_sidecars_listener(&self) -> Receiver<NewBlobSidecar>;

    /// Returns a new stream that yields new valid transactions added to the pool
    /// depending on the given [`TransactionListenerKind`] argument.
    fn new_transactions_listener_for(
        &self,
        kind: TransactionListenerKind,
    ) -> Receiver<NewTransactionEvent<Self::Transaction>>;

    /// Returns a new Stream that yields new transactions added to the pending sub-pool.
    ///
    /// This is a convenience wrapper around [`Self::new_transactions_listener`] that filters for
    /// [`SubPool::Pending`](crate::SubPool).
    fn new_pending_pool_transactions_listener(
        &self,
    ) -> NewSubpoolTransactionStream<Self::Transaction> {
        NewSubpoolTransactionStream::new(
            self.new_transactions_listener_for(TransactionListenerKind::PropagateOnly),
            SubPool::Pending,
        )
    }

    /// Returns a new Stream that yields new transactions added to the basefee sub-pool.
    ///
    /// This is a convenience wrapper around [`Self::new_transactions_listener`] that filters for
    /// [`SubPool::BaseFee`](crate::SubPool).
    fn new_basefee_pool_transactions_listener(
        &self,
    ) -> NewSubpoolTransactionStream<Self::Transaction> {
        NewSubpoolTransactionStream::new(self.new_transactions_listener(), SubPool::BaseFee)
    }

    /// Returns a new Stream that yields new transactions added to the queued-pool.
    ///
    /// This is a convenience wrapper around [`Self::new_transactions_listener`] that filters for
    /// [`SubPool::Queued`](crate::SubPool).
    fn new_queued_transactions_listener(&self) -> NewSubpoolTransactionStream<Self::Transaction> {
        NewSubpoolTransactionStream::new(self.new_transactions_listener(), SubPool::Queued)
    }

    /// Returns the _hashes_ of all transactions in the pool.
    ///
    /// Note: This returns a `Vec` but should guarantee that all hashes are unique.
    ///
    /// Consumer: P2P
    fn pooled_transaction_hashes(&self) -> Vec<TxHash>;

    /// Returns only the first `max` hashes of transactions in the pool.
    ///
    /// Consumer: P2P
    fn pooled_transaction_hashes_max(&self, max: usize) -> Vec<TxHash>;

    /// Returns the _full_ transaction objects all transactions in the pool.
    ///
    /// This is intended to be used by the network for the initial exchange of pooled transaction
    /// _hashes_
    ///
    /// Note: This returns a `Vec` but should guarantee that all transactions are unique.
    ///
    /// Caution: In case of blob transactions, this does not include the sidecar.
    ///
    /// Consumer: P2P
    fn pooled_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>>;

    /// Returns only the first `max` transactions in the pool.
    ///
    /// Consumer: P2P
    fn pooled_transactions_max(
        &self,
        max: usize,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>>;

    /// Returns converted [`PooledTransactionVariant`] for the given transaction hashes.
    ///
    /// This adheres to the expected behavior of
    /// [`GetPooledTransactions`](https://github.com/ethereum/devp2p/blob/master/caps/eth.md#getpooledtransactions-0x09):
    ///
    /// The transactions must be in same order as in the request, but it is OK to skip transactions
    /// which are not available.
    ///
    /// If the transaction is a blob transaction, the sidecar will be included.
    ///
    /// Consumer: P2P
    fn get_pooled_transaction_elements(
        &self,
        tx_hashes: Vec<TxHash>,
        limit: GetPooledTransactionLimit,
    ) -> Vec<<Self::Transaction as PoolTransaction>::Pooled>;

    /// Returns the pooled transaction variant for the given transaction hash.
    ///
    /// This adheres to the expected behavior of
    /// [`GetPooledTransactions`](https://github.com/ethereum/devp2p/blob/master/caps/eth.md#getpooledtransactions-0x09):
    ///
    /// If the transaction is a blob transaction, the sidecar will be included.
    ///
    /// It is expected that this variant represents the valid p2p format for full transactions.
    /// E.g. for EIP-4844 transactions this is the consensus transaction format with the blob
    /// sidecar.
    ///
    /// Consumer: P2P
    fn get_pooled_transaction_element(
        &self,
        tx_hash: TxHash,
    ) -> Option<Recovered<<Self::Transaction as PoolTransaction>::Pooled>>;

    /// Returns an iterator that yields transactions that are ready for block production.
    ///
    /// Consumer: Block production
    fn best_transactions(
        &self,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<Self::Transaction>>>>;

    /// Returns an iterator that yields transactions that are ready for block production with the
    /// given base fee and optional blob fee attributes.
    ///
    /// Consumer: Block production
    fn best_transactions_with_attributes(
        &self,
        best_transactions_attributes: BestTransactionsAttributes,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<Self::Transaction>>>>;

    /// Returns all transactions that can be included in the next block.
    ///
    /// This is primarily used for the `txpool_` RPC namespace:
    /// <https://geth.ethereum.org/docs/interacting-with-geth/rpc/ns-txpool> which distinguishes
    /// between `pending` and `queued` transactions, where `pending` are transactions ready for
    /// inclusion in the next block and `queued` are transactions that are ready for inclusion in
    /// future blocks.
    ///
    /// Consumer: RPC
    fn pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>>;

    /// Returns first `max` transactions that can be included in the next block.
    /// See <https://github.com/paradigmxyz/reth/issues/12767#issuecomment-2493223579>
    ///
    /// Consumer: Block production
    fn pending_transactions_max(
        &self,
        max: usize,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>>;

    /// Returns all transactions that can be included in _future_ blocks.
    ///
    /// This and [`Self::pending_transactions`] are mutually exclusive.
    ///
    /// Consumer: RPC
    fn queued_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>>;

    /// Returns the number of transactions that are ready for inclusion in the next block and the
    /// number of transactions that are ready for inclusion in future blocks: `(pending, queued)`.
    fn pending_and_queued_txn_count(&self) -> (usize, usize);

    /// Returns all transactions that are currently in the pool grouped by whether they are ready
    /// for inclusion in the next block or not.
    ///
    /// This is primarily used for the `txpool_` namespace: <https://geth.ethereum.org/docs/interacting-with-geth/rpc/ns-txpool>
    ///
    /// Consumer: RPC
    fn all_transactions(&self) -> AllPoolTransactions<Self::Transaction>;

    /// Removes all transactions corresponding to the given hashes.
    ///
    /// Note: This removes the transactions as if they got discarded (_not_ mined).
    ///
    /// Consumer: Utility
    fn remove_transactions(
        &self,
        hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>>;

    /// Removes all transactions corresponding to the given hashes.
    ///
    /// Also removes all _dependent_ transactions.
    ///
    /// Consumer: Utility
    fn remove_transactions_and_descendants(
        &self,
        hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>>;

    /// Removes all transactions from the given sender
    ///
    /// Consumer: Utility
    fn remove_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>>;

    /// Retains only those hashes that are unknown to the pool.
    /// In other words, removes all transactions from the given set that are currently present in
    /// the pool. Returns hashes already known to the pool.
    ///
    /// Consumer: P2P
    fn retain_unknown<A>(&self, announcement: &mut A)
    where
        A: HandleMempoolData;

    /// Returns if the transaction for the given hash is already included in this pool.
    fn contains(&self, tx_hash: &TxHash) -> bool {
        self.get(tx_hash).is_some()
    }

    /// Returns the transaction for the given hash.
    fn get(&self, tx_hash: &TxHash) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>>;

    /// Returns all transactions objects for the given hashes.
    ///
    /// Caution: This in case of blob transactions, this does not include the sidecar.
    fn get_all(&self, txs: Vec<TxHash>) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>>;

    /// Notify the pool about transactions that are propagated to peers.
    ///
    /// Consumer: P2P
    fn on_propagated(&self, txs: PropagatedTransactions);

    /// Returns all transactions sent by a given user
    fn get_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>>;

    /// Returns all pending transactions filtered by predicate
    fn get_pending_transactions_with_predicate(
        &self,
        predicate: impl FnMut(&ValidPoolTransaction<Self::Transaction>) -> bool,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>>;

    /// Returns all pending transactions sent by a given user
    fn get_pending_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>>;

    /// Returns all queued transactions sent by a given user
    fn get_queued_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>>;

    /// Returns the highest transaction sent by a given user
    fn get_highest_transaction_by_sender(
        &self,
        sender: Address,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>>;

    /// Returns the transaction with the highest nonce that is executable given the on chain nonce.
    /// In other words the highest non nonce gapped transaction.
    ///
    /// Note: The next pending pooled transaction must have the on chain nonce.
    ///
    /// For example, for a given on chain nonce of `5`, the next transaction must have that nonce.
    /// If the pool contains txs `[5,6,7]` this returns tx `7`.
    /// If the pool contains txs `[6,7]` this returns `None` because the next valid nonce (5) is
    /// missing, which means txs `[6,7]` are nonce gapped.
    fn get_highest_consecutive_transaction_by_sender(
        &self,
        sender: Address,
        on_chain_nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>>;

    /// Returns a transaction sent by a given user and a nonce
    fn get_transaction_by_sender_and_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>>;

    /// Returns all transactions that where submitted with the given [`TransactionOrigin`]
    fn get_transactions_by_origin(
        &self,
        origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>>;

    /// Returns all pending transactions filtered by [`TransactionOrigin`]
    fn get_pending_transactions_by_origin(
        &self,
        origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>>;

    /// Returns all transactions that where submitted as [`TransactionOrigin::Local`]
    fn get_local_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.get_transactions_by_origin(TransactionOrigin::Local)
    }

    /// Returns all transactions that where submitted as [`TransactionOrigin::Private`]
    fn get_private_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.get_transactions_by_origin(TransactionOrigin::Private)
    }

    /// Returns all transactions that where submitted as [`TransactionOrigin::External`]
    fn get_external_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.get_transactions_by_origin(TransactionOrigin::External)
    }

    /// Returns all pending transactions that where submitted as [`TransactionOrigin::Local`]
    fn get_local_pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.get_pending_transactions_by_origin(TransactionOrigin::Local)
    }

    /// Returns all pending transactions that where submitted as [`TransactionOrigin::Private`]
    fn get_private_pending_transactions(
        &self,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.get_pending_transactions_by_origin(TransactionOrigin::Private)
    }

    /// Returns all pending transactions that where submitted as [`TransactionOrigin::External`]
    fn get_external_pending_transactions(
        &self,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.get_pending_transactions_by_origin(TransactionOrigin::External)
    }

    /// Returns a set of all senders of transactions in the pool
    fn unique_senders(&self) -> HashSet<Address>;

    /// Returns the [`BlobTransactionSidecarVariant`] for the given transaction hash if it exists in
    /// the blob store.
    fn get_blob(
        &self,
        tx_hash: TxHash,
    ) -> Result<Option<Arc<BlobTransactionSidecarVariant>>, BlobStoreError>;

    /// Returns all [`BlobTransactionSidecarVariant`] for the given transaction hashes if they
    /// exists in the blob store.
    ///
    /// This only returns the blobs that were found in the store.
    /// If there's no blob it will not be returned.
    fn get_all_blobs(
        &self,
        tx_hashes: Vec<TxHash>,
    ) -> Result<Vec<(TxHash, Arc<BlobTransactionSidecarVariant>)>, BlobStoreError>;

    /// Returns the exact [`BlobTransactionSidecarVariant`] for the given transaction hashes in the
    /// order they were requested.
    ///
    /// Returns an error if any of the blobs are not found in the blob store.
    fn get_all_blobs_exact(
        &self,
        tx_hashes: Vec<TxHash>,
    ) -> Result<Vec<Arc<BlobTransactionSidecarVariant>>, BlobStoreError>;

    /// Return the [`BlobAndProofV1`]s for a list of blob versioned hashes.
    fn get_blobs_for_versioned_hashes_v1(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Vec<Option<BlobAndProofV1>>, BlobStoreError>;

    /// Return the [`BlobAndProofV2`]s for a list of blob versioned hashes.
    /// Blobs and proofs are returned only if they are present for _all_ of the requested versioned
    /// hashes.
    fn get_blobs_for_versioned_hashes_v2(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Option<Vec<BlobAndProofV2>>, BlobStoreError>;
}

/// Extension for [`TransactionPool`] trait that allows to set the current block info.
#[auto_impl::auto_impl(&, Arc)]
pub trait TransactionPoolExt: TransactionPool {
    /// Sets the current block info for the pool.
    fn set_block_info(&self, info: BlockInfo);

    /// Event listener for when the pool needs to be updated.
    ///
    /// Implementers need to update the pool accordingly:
    ///
    /// ## Fee changes
    ///
    /// The [`CanonicalStateUpdate`] includes the base and blob fee of the pending block, which
    /// affects the dynamic fee requirement of pending transactions in the pool.
    ///
    /// ## EIP-4844 Blob transactions
    ///
    /// Mined blob transactions need to be removed from the pool, but from the pool only. The blob
    /// sidecar must not be removed from the blob store. Only after a blob transaction is
    /// finalized, its sidecar is removed from the blob store. This ensures that in case of a reorg,
    /// the sidecar is still available.
    fn on_canonical_state_change<B>(&self, update: CanonicalStateUpdate<'_, B>)
    where
        B: Block;

    /// Updates the accounts in the pool
    fn update_accounts(&self, accounts: Vec<ChangedAccount>);

    /// Deletes the blob sidecar for the given transaction from the blob store
    fn delete_blob(&self, tx: B256);

    /// Deletes multiple blob sidecars from the blob store
    fn delete_blobs(&self, txs: Vec<B256>);

    /// Maintenance function to cleanup blobs that are no longer needed.
    fn cleanup_blobs(&self);
}

/// A Helper type that bundles all transactions in the pool.
#[derive(Debug, Clone)]
pub struct AllPoolTransactions<T: PoolTransaction> {
    /// Transactions that are ready for inclusion in the next block.
    pub pending: Vec<Arc<ValidPoolTransaction<T>>>,
    /// Transactions that are ready for inclusion in _future_ blocks, but are currently parked,
    /// because they depend on other transactions that are not yet included in the pool (nonce gap)
    /// or otherwise blocked.
    pub queued: Vec<Arc<ValidPoolTransaction<T>>>,
}

// === impl AllPoolTransactions ===

impl<T: PoolTransaction> AllPoolTransactions<T> {
    /// Returns an iterator over all pending [`Recovered`] transactions.
    pub fn pending_recovered(&self) -> impl Iterator<Item = Recovered<T::Consensus>> + '_ {
        self.pending.iter().map(|tx| tx.transaction.clone().into_consensus())
    }

    /// Returns an iterator over all queued [`Recovered`] transactions.
    pub fn queued_recovered(&self) -> impl Iterator<Item = Recovered<T::Consensus>> + '_ {
        self.queued.iter().map(|tx| tx.transaction.clone().into_consensus())
    }

    /// Returns an iterator over all transactions, both pending and queued.
    pub fn all(&self) -> impl Iterator<Item = Recovered<T::Consensus>> + '_ {
        self.pending
            .iter()
            .chain(self.queued.iter())
            .map(|tx| tx.transaction.clone().into_consensus())
    }
}

impl<T: PoolTransaction> Default for AllPoolTransactions<T> {
    fn default() -> Self {
        Self { pending: Default::default(), queued: Default::default() }
    }
}

/// Represents transactions that were propagated over the network.
#[derive(Debug, Clone, Eq, PartialEq, Default)]
pub struct PropagatedTransactions(pub HashMap<TxHash, Vec<PropagateKind>>);

/// Represents how a transaction was propagated over the network.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum PropagateKind {
    /// The full transaction object was sent to the peer.
    ///
    /// This is equivalent to the `Transaction` message
    Full(PeerId),
    /// Only the Hash was propagated to the peer.
    Hash(PeerId),
}

// === impl PropagateKind ===

impl PropagateKind {
    /// Returns the peer the transaction was sent to
    pub const fn peer(&self) -> &PeerId {
        match self {
            Self::Full(peer) | Self::Hash(peer) => peer,
        }
    }

    /// Returns true if the transaction was sent as a full transaction
    pub const fn is_full(&self) -> bool {
        matches!(self, Self::Full(_))
    }

    /// Returns true if the transaction was sent as a hash
    pub const fn is_hash(&self) -> bool {
        matches!(self, Self::Hash(_))
    }
}

impl From<PropagateKind> for PeerId {
    fn from(value: PropagateKind) -> Self {
        match value {
            PropagateKind::Full(peer) | PropagateKind::Hash(peer) => peer,
        }
    }
}

/// This type represents a new blob sidecar that has been stored in the transaction pool's
/// blobstore; it includes the `TransactionHash` of the blob transaction along with the assoc.
/// sidecar (blobs, commitments, proofs)
#[derive(Debug, Clone)]
pub struct NewBlobSidecar {
    /// hash of the EIP-4844 transaction.
    pub tx_hash: TxHash,
    /// the blob transaction sidecar.
    pub sidecar: Arc<BlobTransactionSidecarVariant>,
}

/// Where the transaction originates from.
///
/// Depending on where the transaction was picked up, it affects how the transaction is handled
/// internally, e.g. limits for simultaneous transaction of one sender.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub enum TransactionOrigin {
    /// Transaction is coming from a local source.
    #[default]
    Local,
    /// Transaction has been received externally.
    ///
    /// This is usually considered an "untrusted" source, for example received from another in the
    /// network.
    External,
    /// Transaction is originated locally and is intended to remain private.
    ///
    /// This type of transaction should not be propagated to the network. It's meant for
    /// private usage within the local node only.
    Private,
}

// === impl TransactionOrigin ===

impl TransactionOrigin {
    /// Whether the transaction originates from a local source.
    pub const fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }

    /// Whether the transaction originates from an external source.
    pub const fn is_external(&self) -> bool {
        matches!(self, Self::External)
    }
    /// Whether the transaction originates from a private source.
    pub const fn is_private(&self) -> bool {
        matches!(self, Self::Private)
    }
}

/// Represents the kind of update to the canonical state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolUpdateKind {
    /// The update was due to a block commit.
    Commit,
    /// The update was due to a reorganization.
    Reorg,
}

/// Represents changes after a new canonical block or range of canonical blocks was added to the
/// chain.
///
/// It is expected that this is only used if the added blocks are canonical to the pool's last known
/// block hash. In other words, the first added block of the range must be the child of the last
/// known block hash.
///
/// This is used to update the pool state accordingly.
#[derive(Clone, Debug)]
pub struct CanonicalStateUpdate<'a, B: Block> {
    /// Hash of the tip block.
    pub new_tip: &'a SealedBlock<B>,
    /// EIP-1559 Base fee of the _next_ (pending) block
    ///
    /// The base fee of a block depends on the utilization of the last block and its base fee.
    pub pending_block_base_fee: u64,
    /// EIP-4844 blob fee of the _next_ (pending) block
    ///
    /// Only after Cancun
    pub pending_block_blob_fee: Option<u128>,
    /// A set of changed accounts across a range of blocks.
    pub changed_accounts: Vec<ChangedAccount>,
    /// All mined transactions in the block range.
    pub mined_transactions: Vec<B256>,
    /// The kind of update to the canonical state.
    pub update_kind: PoolUpdateKind,
}

impl<B> CanonicalStateUpdate<'_, B>
where
    B: Block,
{
    /// Returns the number of the tip block.
    pub fn number(&self) -> u64 {
        self.new_tip.number()
    }

    /// Returns the hash of the tip block.
    pub fn hash(&self) -> B256 {
        self.new_tip.hash()
    }

    /// Timestamp of the latest chain update
    pub fn timestamp(&self) -> u64 {
        self.new_tip.timestamp()
    }

    /// Returns the block info for the tip block.
    pub fn block_info(&self) -> BlockInfo {
        BlockInfo {
            block_gas_limit: self.new_tip.gas_limit(),
            last_seen_block_hash: self.hash(),
            last_seen_block_number: self.number(),
            pending_basefee: self.pending_block_base_fee,
            pending_blob_fee: self.pending_block_blob_fee,
        }
    }
}

impl<B> fmt::Display for CanonicalStateUpdate<'_, B>
where
    B: Block,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CanonicalStateUpdate")
            .field("hash", &self.hash())
            .field("number", &self.number())
            .field("pending_block_base_fee", &self.pending_block_base_fee)
            .field("pending_block_blob_fee", &self.pending_block_blob_fee)
            .field("changed_accounts", &self.changed_accounts.len())
            .field("mined_transactions", &self.mined_transactions.len())
            .finish()
    }
}

/// Alias to restrict the [`BestTransactions`] items to the pool's transaction type.
pub type BestTransactionsFor<Pool> = Box<
    dyn BestTransactions<Item = Arc<ValidPoolTransaction<<Pool as TransactionPool>::Transaction>>>,
>;

/// An `Iterator` that only returns transactions that are ready to be executed.
///
/// This makes no assumptions about the order of the transactions, but expects that _all_
/// transactions are valid (no nonce gaps.) for the tracked state of the pool.
///
/// Note: this iterator will always return the best transaction that it currently knows.
/// There is no guarantee transactions will be returned sequentially in decreasing
/// priority order.
pub trait BestTransactions: Iterator + Send {
    /// Mark the transaction as invalid.
    ///
    /// Implementers must ensure all subsequent transaction _don't_ depend on this transaction.
    /// In other words, this must remove the given transaction _and_ drain all transaction that
    /// depend on it.
    fn mark_invalid(&mut self, transaction: &Self::Item, kind: InvalidPoolTransactionError);

    /// An iterator may be able to receive additional pending transactions that weren't present it
    /// the pool when it was created.
    ///
    /// This ensures that iterator will return the best transaction that it currently knows and not
    /// listen to pool updates.
    fn no_updates(&mut self);

    /// Convenience function for [`Self::no_updates`] that returns the iterator again.
    fn without_updates(mut self) -> Self
    where
        Self: Sized,
    {
        self.no_updates();
        self
    }

    /// Skip all blob transactions.
    ///
    /// There's only limited blob space available in a block, once exhausted, EIP-4844 transactions
    /// can no longer be included.
    ///
    /// If called then the iterator will no longer yield blob transactions.
    ///
    /// Note: this will also exclude any transactions that depend on blob transactions.
    fn skip_blobs(&mut self) {
        self.set_skip_blobs(true);
    }

    /// Controls whether the iterator skips blob transactions or not.
    ///
    /// If set to true, no blob transactions will be returned.
    fn set_skip_blobs(&mut self, skip_blobs: bool);

    /// Convenience function for [`Self::skip_blobs`] that returns the iterator again.
    fn without_blobs(mut self) -> Self
    where
        Self: Sized,
    {
        self.skip_blobs();
        self
    }

    /// Creates an iterator which uses a closure to determine whether a transaction should be
    /// returned by the iterator.
    ///
    /// All items the closure returns false for are marked as invalid via [`Self::mark_invalid`] and
    /// descendant transactions will be skipped.
    fn filter_transactions<P>(self, predicate: P) -> BestTransactionFilter<Self, P>
    where
        P: FnMut(&Self::Item) -> bool,
        Self: Sized,
    {
        BestTransactionFilter::new(self, predicate)
    }
}

impl<T> BestTransactions for Box<T>
where
    T: BestTransactions + ?Sized,
{
    fn mark_invalid(&mut self, transaction: &Self::Item, kind: InvalidPoolTransactionError) {
        (**self).mark_invalid(transaction, kind)
    }

    fn no_updates(&mut self) {
        (**self).no_updates();
    }

    fn skip_blobs(&mut self) {
        (**self).skip_blobs();
    }

    fn set_skip_blobs(&mut self, skip_blobs: bool) {
        (**self).set_skip_blobs(skip_blobs);
    }
}

/// A no-op implementation that yields no transactions.
impl<T> BestTransactions for std::iter::Empty<T> {
    fn mark_invalid(&mut self, _tx: &T, _kind: InvalidPoolTransactionError) {}

    fn no_updates(&mut self) {}

    fn skip_blobs(&mut self) {}

    fn set_skip_blobs(&mut self, _skip_blobs: bool) {}
}

/// A filter that allows to check if a transaction satisfies a set of conditions
pub trait TransactionFilter {
    /// The type of the transaction to check.
    type Transaction;

    /// Returns true if the transaction satisfies the conditions.
    fn is_valid(&self, transaction: &Self::Transaction) -> bool;
}

/// A no-op implementation of [`TransactionFilter`] which
/// marks all transactions as valid.
#[derive(Debug, Clone)]
pub struct NoopTransactionFilter<T>(std::marker::PhantomData<T>);

// We can't derive Default because this forces T to be
// Default as well, which isn't necessary.
impl<T> Default for NoopTransactionFilter<T> {
    fn default() -> Self {
        Self(std::marker::PhantomData)
    }
}

impl<T> TransactionFilter for NoopTransactionFilter<T> {
    type Transaction = T;

    fn is_valid(&self, _transaction: &Self::Transaction) -> bool {
        true
    }
}

/// A Helper type that bundles the best transactions attributes together.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct BestTransactionsAttributes {
    /// The base fee attribute for best transactions.
    pub basefee: u64,
    /// The blob fee attribute for best transactions.
    pub blob_fee: Option<u64>,
}

// === impl BestTransactionsAttributes ===

impl BestTransactionsAttributes {
    /// Creates a new `BestTransactionsAttributes` with the given basefee and blob fee.
    pub const fn new(basefee: u64, blob_fee: Option<u64>) -> Self {
        Self { basefee, blob_fee }
    }

    /// Creates a new `BestTransactionsAttributes` with the given basefee.
    pub const fn base_fee(basefee: u64) -> Self {
        Self::new(basefee, None)
    }

    /// Sets the given blob fee.
    pub const fn with_blob_fee(mut self, blob_fee: u64) -> Self {
        self.blob_fee = Some(blob_fee);
        self
    }
}

/// Trait for transaction types stored in the transaction pool.
///
/// This trait represents the actual transaction object stored in the mempool, which includes not
/// only the transaction data itself but also additional metadata needed for efficient pool
/// operations. Implementations typically cache values that are frequently accessed during
/// transaction ordering, validation, and eviction.
///
/// ## Key Responsibilities
///
/// 1. **Metadata Caching**: Store computed values like address, cost and encoded size
/// 2. **Representation Conversion**: Handle conversions between consensus and pooled
///    representations
/// 3. **Validation Support**: Provide methods for pool-specific validation rules
///
/// ## Cached Metadata
///
/// Implementations should cache frequently accessed values to avoid recomputation:
/// - **Address**: Recovered sender address of the transaction
/// - **Cost**: Max amount spendable (gas × price + value + blob costs)
/// - **Size**: RLP encoded length for mempool size limits
///
/// See [`EthPooledTransaction`] for a reference implementation.
///
/// ## Transaction Representations
///
/// This trait abstracts over the different representations a transaction can have:
///
/// 1. **Consensus representation** (`Consensus` associated type): The canonical form included in
///    blocks
///    - Compact representation without networking metadata
///    - For EIP-4844: includes only blob hashes, not the actual blobs
///    - Used for block execution and state transitions
///
/// 2. **Pooled representation** (`Pooled` associated type): The form used for network propagation
///    - May include additional data for validation
///    - For EIP-4844: includes full blob sidecars (blobs, commitments, proofs)
///    - Used for mempool validation and p2p gossiping
///
/// ## Why Two Representations?
///
/// This distinction is necessary because:
///
/// - **EIP-4844 blob transactions**: Require large blob sidecars for validation that would bloat
///   blocks if included. Only blob hashes are stored on-chain.
///
/// - **Network efficiency**: Blob transactions are not broadcast to all peers automatically but
///   must be explicitly requested to reduce bandwidth usage.
///
/// - **Special transactions**: Some transactions (like OP deposit transactions) exist only in
///   consensus format and are never in the mempool.
///
/// ## Conversion Rules
///
/// - `Consensus` → `Pooled`: May fail for transactions that cannot be pooled (e.g., OP deposit
///   transactions, blob transactions without sidecars)
/// - `Pooled` → `Consensus`: Always succeeds (pooled is a superset)
pub trait PoolTransaction:
    alloy_consensus::Transaction + InMemorySize + Debug + Send + Sync + Clone
{
    /// Associated error type for the `try_from_consensus` method.
    type TryFromConsensusError: fmt::Display;

    /// Associated type representing the raw consensus variant of the transaction.
    type Consensus: SignedTransaction + From<Self::Pooled>;

    /// Associated type representing the recovered pooled variant of the transaction.
    type Pooled: TryFrom<Self::Consensus, Error = Self::TryFromConsensusError> + SignedTransaction;

    /// Define a method to convert from the `Consensus` type to `Self`
    ///
    /// This conversion may fail for transactions that are valid for inclusion in blocks
    /// but cannot exist in the transaction pool. Examples include:
    ///
    /// - **OP Deposit transactions**: These are special system transactions that are directly
    ///   included in blocks by the sequencer/validator and never enter the mempool
    /// - **Blob transactions without sidecars**: After being included in a block, the sidecar data
    ///   is pruned, making the consensus transaction unpoolable
    fn try_from_consensus(
        tx: Recovered<Self::Consensus>,
    ) -> Result<Self, Self::TryFromConsensusError> {
        let (tx, signer) = tx.into_parts();
        Ok(Self::from_pooled(Recovered::new_unchecked(tx.try_into()?, signer)))
    }

    /// Clone the transaction into a consensus variant.
    ///
    /// This method is preferred when the [`PoolTransaction`] already wraps the consensus variant.
    fn clone_into_consensus(&self) -> Recovered<Self::Consensus> {
        self.clone().into_consensus()
    }

    /// Define a method to convert from the `Self` type to `Consensus`
    fn into_consensus(self) -> Recovered<Self::Consensus>;

    /// Converts the transaction into consensus format while preserving the EIP-2718 encoded bytes.
    /// This is used to optimize transaction execution by reusing cached encoded bytes instead of
    /// re-encoding the transaction. The cached bytes are particularly useful in payload building
    /// where the same transaction may be executed multiple times.
    fn into_consensus_with2718(self) -> WithEncoded<Recovered<Self::Consensus>> {
        self.into_consensus().into_encoded()
    }

    /// Define a method to convert from the `Pooled` type to `Self`
    fn from_pooled(pooled: Recovered<Self::Pooled>) -> Self;

    /// Tries to convert the `Consensus` type into the `Pooled` type.
    fn try_into_pooled(self) -> Result<Recovered<Self::Pooled>, Self::TryFromConsensusError> {
        let consensus = self.into_consensus();
        let (tx, signer) = consensus.into_parts();
        Ok(Recovered::new_unchecked(tx.try_into()?, signer))
    }

    /// Converts the `Pooled` type into the `Consensus` type.
    fn pooled_into_consensus(tx: Self::Pooled) -> Self::Consensus {
        tx.into()
    }

    /// Hash of the transaction.
    fn hash(&self) -> &TxHash;

    /// The Sender of the transaction.
    fn sender(&self) -> Address;

    /// Reference to the Sender of the transaction.
    fn sender_ref(&self) -> &Address;

    /// Returns the cost that this transaction is allowed to consume:
    ///
    /// For EIP-1559 transactions: `max_fee_per_gas * gas_limit + tx_value`.
    /// For legacy transactions: `gas_price * gas_limit + tx_value`.
    /// For EIP-4844 blob transactions: `max_fee_per_gas * gas_limit + tx_value +
    /// max_blob_fee_per_gas * blob_gas_used`.
    fn cost(&self) -> &U256;

    /// Returns the length of the rlp encoded transaction object
    ///
    /// Note: Implementations should cache this value.
    fn encoded_length(&self) -> usize;

    /// Ensures that the transaction's code size does not exceed the provided `max_init_code_size`.
    ///
    /// This is specifically relevant for contract creation transactions ([`TxKind::Create`]),
    /// where the input data contains the initialization code. If the input code size exceeds
    /// the configured limit, an [`InvalidPoolTransactionError::ExceedsMaxInitCodeSize`] error is
    /// returned.
    fn ensure_max_init_code_size(
        &self,
        max_init_code_size: usize,
    ) -> Result<(), InvalidPoolTransactionError> {
        let input_len = self.input().len();
        if self.is_create() && input_len > max_init_code_size {
            Err(InvalidPoolTransactionError::ExceedsMaxInitCodeSize(input_len, max_init_code_size))
        } else {
            Ok(())
        }
    }
}

/// Super trait for transactions that can be converted to and from Eth transactions intended for the
/// ethereum style pool.
///
/// This extends the [`PoolTransaction`] trait with additional methods that are specific to the
/// Ethereum pool.
pub trait EthPoolTransaction: PoolTransaction {
    /// Extracts the blob sidecar from the transaction.
    fn take_blob(&mut self) -> EthBlobTransactionSidecar;

    /// A specialization for the EIP-4844 transaction type.
    /// Tries to reattach the blob sidecar to the transaction.
    ///
    /// This returns an option, but callers should ensure that the transaction is an EIP-4844
    /// transaction: [`Typed2718::is_eip4844`].
    fn try_into_pooled_eip4844(
        self,
        sidecar: Arc<BlobTransactionSidecarVariant>,
    ) -> Option<Recovered<Self::Pooled>>;

    /// Tries to convert the `Consensus` type with a blob sidecar into the `Pooled` type.
    ///
    /// Returns `None` if passed transaction is not a blob transaction.
    fn try_from_eip4844(
        tx: Recovered<Self::Consensus>,
        sidecar: BlobTransactionSidecarVariant,
    ) -> Option<Self>;

    /// Validates the blob sidecar of the transaction with the given settings.
    fn validate_blob(
        &self,
        blob: &BlobTransactionSidecarVariant,
        settings: &KzgSettings,
    ) -> Result<(), BlobTransactionValidationError>;
}

/// The default [`PoolTransaction`] for the [Pool](crate::Pool) for Ethereum.
///
/// This type wraps a consensus transaction with additional cached data that's
/// frequently accessed by the pool for transaction ordering and validation:
///
/// - `cost`: Pre-calculated max cost (gas * price + value + blob costs)
/// - `encoded_length`: Cached RLP encoding length for size limits
/// - `blob_sidecar`: Blob data state (None/Missing/Present)
///
/// This avoids recalculating these values repeatedly during pool operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EthPooledTransaction<T = TransactionSigned> {
    /// `EcRecovered` transaction, the consensus format.
    pub transaction: Recovered<T>,

    /// For EIP-1559 transactions: `max_fee_per_gas * gas_limit + tx_value`.
    /// For legacy transactions: `gas_price * gas_limit + tx_value`.
    /// For EIP-4844 blob transactions: `max_fee_per_gas * gas_limit + tx_value +
    /// max_blob_fee_per_gas * blob_gas_used`.
    pub cost: U256,

    /// This is the RLP length of the transaction, computed when the transaction is added to the
    /// pool.
    pub encoded_length: usize,

    /// The blob side car for this transaction
    pub blob_sidecar: EthBlobTransactionSidecar,
}

impl<T: SignedTransaction> EthPooledTransaction<T> {
    /// Create new instance of [Self].
    ///
    /// Caution: In case of blob transactions, this does marks the blob sidecar as
    /// [`EthBlobTransactionSidecar::Missing`]
    pub fn new(transaction: Recovered<T>, encoded_length: usize) -> Self {
        let mut blob_sidecar = EthBlobTransactionSidecar::None;

        let gas_cost = U256::from(transaction.max_fee_per_gas())
            .saturating_mul(U256::from(transaction.gas_limit()));

        let mut cost = gas_cost.saturating_add(transaction.value());

        if let (Some(blob_gas_used), Some(max_fee_per_blob_gas)) =
            (transaction.blob_gas_used(), transaction.max_fee_per_blob_gas())
        {
            // Add max blob cost using saturating math to avoid overflow
            cost = cost.saturating_add(U256::from(
                max_fee_per_blob_gas.saturating_mul(blob_gas_used as u128),
            ));

            // because the blob sidecar is not included in this transaction variant, mark it as
            // missing
            blob_sidecar = EthBlobTransactionSidecar::Missing;
        }

        Self { transaction, cost, encoded_length, blob_sidecar }
    }

    /// Return the reference to the underlying transaction.
    pub const fn transaction(&self) -> &Recovered<T> {
        &self.transaction
    }
}

impl PoolTransaction for EthPooledTransaction {
    type TryFromConsensusError = ValueError<TransactionSigned>;

    type Consensus = TransactionSigned;

    type Pooled = PooledTransactionVariant;

    fn clone_into_consensus(&self) -> Recovered<Self::Consensus> {
        self.transaction().clone()
    }

    fn into_consensus(self) -> Recovered<Self::Consensus> {
        self.transaction
    }

    fn from_pooled(tx: Recovered<Self::Pooled>) -> Self {
        let encoded_length = tx.encode_2718_len();
        let (tx, signer) = tx.into_parts();
        match tx {
            PooledTransactionVariant::Eip4844(tx) => {
                // include the blob sidecar
                let (tx, sig, hash) = tx.into_parts();
                let (tx, blob) = tx.into_parts();
                let tx = Signed::new_unchecked(tx, sig, hash);
                let tx = TransactionSigned::from(tx);
                let tx = Recovered::new_unchecked(tx, signer);
                let mut pooled = Self::new(tx, encoded_length);
                pooled.blob_sidecar = EthBlobTransactionSidecar::Present(blob);
                pooled
            }
            tx => {
                // no blob sidecar
                let tx = Recovered::new_unchecked(tx.into(), signer);
                Self::new(tx, encoded_length)
            }
        }
    }

    /// Returns hash of the transaction.
    fn hash(&self) -> &TxHash {
        self.transaction.tx_hash()
    }

    /// Returns the Sender of the transaction.
    fn sender(&self) -> Address {
        self.transaction.signer()
    }

    /// Returns a reference to the Sender of the transaction.
    fn sender_ref(&self) -> &Address {
        self.transaction.signer_ref()
    }

    /// Returns the cost that this transaction is allowed to consume:
    ///
    /// For EIP-1559 transactions: `max_fee_per_gas * gas_limit + tx_value`.
    /// For legacy transactions: `gas_price * gas_limit + tx_value`.
    /// For EIP-4844 blob transactions: `max_fee_per_gas * gas_limit + tx_value +
    /// max_blob_fee_per_gas * blob_gas_used`.
    fn cost(&self) -> &U256 {
        &self.cost
    }

    /// Returns the length of the rlp encoded object
    fn encoded_length(&self) -> usize {
        self.encoded_length
    }
}

impl<T: Typed2718> Typed2718 for EthPooledTransaction<T> {
    fn ty(&self) -> u8 {
        self.transaction.ty()
    }
}

impl<T: InMemorySize> InMemorySize for EthPooledTransaction<T> {
    fn size(&self) -> usize {
        self.transaction.size()
    }
}

impl<T: alloy_consensus::Transaction> alloy_consensus::Transaction for EthPooledTransaction<T> {
    fn chain_id(&self) -> Option<alloy_primitives::ChainId> {
        self.transaction.chain_id()
    }

    fn nonce(&self) -> u64 {
        self.transaction.nonce()
    }

    fn gas_limit(&self) -> u64 {
        self.transaction.gas_limit()
    }

    fn gas_price(&self) -> Option<u128> {
        self.transaction.gas_price()
    }

    fn max_fee_per_gas(&self) -> u128 {
        self.transaction.max_fee_per_gas()
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.transaction.max_priority_fee_per_gas()
    }

    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        self.transaction.max_fee_per_blob_gas()
    }

    fn priority_fee_or_price(&self) -> u128 {
        self.transaction.priority_fee_or_price()
    }

    fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        self.transaction.effective_gas_price(base_fee)
    }

    fn is_dynamic_fee(&self) -> bool {
        self.transaction.is_dynamic_fee()
    }

    fn kind(&self) -> TxKind {
        self.transaction.kind()
    }

    fn is_create(&self) -> bool {
        self.transaction.is_create()
    }

    fn value(&self) -> U256 {
        self.transaction.value()
    }

    fn input(&self) -> &Bytes {
        self.transaction.input()
    }

    fn access_list(&self) -> Option<&AccessList> {
        self.transaction.access_list()
    }

    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        self.transaction.blob_versioned_hashes()
    }

    fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
        self.transaction.authorization_list()
    }
}

impl EthPoolTransaction for EthPooledTransaction {
    fn take_blob(&mut self) -> EthBlobTransactionSidecar {
        if self.is_eip4844() {
            std::mem::replace(&mut self.blob_sidecar, EthBlobTransactionSidecar::Missing)
        } else {
            EthBlobTransactionSidecar::None
        }
    }

    fn try_into_pooled_eip4844(
        self,
        sidecar: Arc<BlobTransactionSidecarVariant>,
    ) -> Option<Recovered<Self::Pooled>> {
        let (signed_transaction, signer) = self.into_consensus().into_parts();
        let pooled_transaction =
            signed_transaction.try_into_pooled_eip4844(Arc::unwrap_or_clone(sidecar)).ok()?;

        Some(Recovered::new_unchecked(pooled_transaction, signer))
    }

    fn try_from_eip4844(
        tx: Recovered<Self::Consensus>,
        sidecar: BlobTransactionSidecarVariant,
    ) -> Option<Self> {
        let (tx, signer) = tx.into_parts();
        tx.try_into_pooled_eip4844(sidecar)
            .ok()
            .map(|tx| tx.with_signer(signer))
            .map(Self::from_pooled)
    }

    fn validate_blob(
        &self,
        sidecar: &BlobTransactionSidecarVariant,
        settings: &KzgSettings,
    ) -> Result<(), BlobTransactionValidationError> {
        match self.transaction.inner().as_eip4844() {
            Some(tx) => tx.tx().validate_blob(sidecar, settings),
            _ => Err(BlobTransactionValidationError::NotBlobTransaction(self.ty())),
        }
    }
}

/// Represents the blob sidecar of the [`EthPooledTransaction`].
///
/// EIP-4844 blob transactions require additional data (blobs, commitments, proofs)
/// for validation that is not included in the consensus format. This enum tracks
/// the sidecar state throughout the transaction's lifecycle in the pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EthBlobTransactionSidecar {
    /// This transaction does not have a blob sidecar
    /// (applies to all non-EIP-4844 transaction types)
    None,
    /// This transaction has a blob sidecar (EIP-4844) but it is missing.
    ///
    /// This can happen when:
    /// - The sidecar was extracted after the transaction was added to the pool
    /// - The transaction was re-injected after a reorg without its sidecar
    /// - The transaction was recovered from the consensus format (e.g., from a block)
    Missing,
    /// The EIP-4844 transaction was received from the network with its complete sidecar.
    ///
    /// This sidecar contains:
    /// - The actual blob data (large data per blob)
    /// - KZG commitments for each blob
    /// - KZG proofs for validation
    ///
    /// The sidecar is required for validating the transaction but is not included
    /// in blocks (only the blob hashes are included in the consensus format).
    Present(BlobTransactionSidecarVariant),
}

impl EthBlobTransactionSidecar {
    /// Returns the blob sidecar if it is present
    pub const fn maybe_sidecar(&self) -> Option<&BlobTransactionSidecarVariant> {
        match self {
            Self::Present(sidecar) => Some(sidecar),
            _ => None,
        }
    }
}

/// Represents the current status of the pool.
#[derive(Debug, Clone, Copy, Default)]
pub struct PoolSize {
    /// Number of transactions in the _pending_ sub-pool.
    pub pending: usize,
    /// Reported size of transactions in the _pending_ sub-pool.
    pub pending_size: usize,
    /// Number of transactions in the _blob_ pool.
    pub blob: usize,
    /// Reported size of transactions in the _blob_ pool.
    pub blob_size: usize,
    /// Number of transactions in the _basefee_ pool.
    pub basefee: usize,
    /// Reported size of transactions in the _basefee_ sub-pool.
    pub basefee_size: usize,
    /// Number of transactions in the _queued_ sub-pool.
    pub queued: usize,
    /// Reported size of transactions in the _queued_ sub-pool.
    pub queued_size: usize,
    /// Number of all transactions of all sub-pools
    ///
    /// Note: this is the sum of ```pending + basefee + queued```
    pub total: usize,
}

// === impl PoolSize ===

impl PoolSize {
    /// Asserts that the invariants of the pool size are met.
    #[cfg(test)]
    pub(crate) fn assert_invariants(&self) {
        assert_eq!(self.total, self.pending + self.basefee + self.queued + self.blob);
    }
}

/// Represents the current status of the pool.
#[derive(Default, Debug, Clone, Copy, Eq, PartialEq)]
pub struct BlockInfo {
    /// Hash for the currently tracked block.
    pub last_seen_block_hash: B256,
    /// Currently tracked block.
    pub last_seen_block_number: u64,
    /// Current block gas limit for the latest block.
    pub block_gas_limit: u64,
    /// Currently enforced base fee: the threshold for the basefee sub-pool.
    ///
    /// Note: this is the derived base fee of the _next_ block that builds on the block the pool is
    /// currently tracking.
    pub pending_basefee: u64,
    /// Currently enforced blob fee: the threshold for eip-4844 blob transactions.
    ///
    /// Note: this is the derived blob fee of the _next_ block that builds on the block the pool is
    /// currently tracking
    pub pending_blob_fee: Option<u128>,
}

/// The limit to enforce for [`TransactionPool::get_pooled_transaction_elements`].
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum GetPooledTransactionLimit {
    /// No limit, return all transactions.
    None,
    /// Enforce a size limit on the returned transactions, for example 2MB
    ResponseSizeSoftLimit(usize),
}

impl GetPooledTransactionLimit {
    /// Returns true if the given size exceeds the limit.
    #[inline]
    pub const fn exceeds(&self, size: usize) -> bool {
        match self {
            Self::None => false,
            Self::ResponseSizeSoftLimit(limit) => size > *limit,
        }
    }
}

/// A Stream that yields full transactions the subpool
#[must_use = "streams do nothing unless polled"]
#[derive(Debug)]
pub struct NewSubpoolTransactionStream<Tx: PoolTransaction> {
    st: Receiver<NewTransactionEvent<Tx>>,
    subpool: SubPool,
}

// === impl NewSubpoolTransactionStream ===

impl<Tx: PoolTransaction> NewSubpoolTransactionStream<Tx> {
    /// Create a new stream that yields full transactions from the subpool
    pub const fn new(st: Receiver<NewTransactionEvent<Tx>>, subpool: SubPool) -> Self {
        Self { st, subpool }
    }

    /// Tries to receive the next value for this stream.
    pub fn try_recv(
        &mut self,
    ) -> Result<NewTransactionEvent<Tx>, tokio::sync::mpsc::error::TryRecvError> {
        loop {
            match self.st.try_recv() {
                Ok(event) => {
                    if event.subpool == self.subpool {
                        return Ok(event)
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl<Tx: PoolTransaction> Stream for NewSubpoolTransactionStream<Tx> {
    type Item = NewTransactionEvent<Tx>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match ready!(self.st.poll_recv(cx)) {
                Some(event) => {
                    if event.subpool == self.subpool {
                        return Poll::Ready(Some(event))
                    }
                }
                None => return Poll::Ready(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{
        EthereumTxEnvelope, SignableTransaction, TxEip1559, TxEip2930, TxEip4844, TxEip7702,
        TxEnvelope, TxLegacy,
    };
    use alloy_eips::eip4844::DATA_GAS_PER_BLOB;
    use alloy_primitives::Signature;

    #[test]
    fn test_pool_size_invariants() {
        let pool_size = PoolSize {
            pending: 10,
            pending_size: 1000,
            blob: 5,
            blob_size: 500,
            basefee: 8,
            basefee_size: 800,
            queued: 7,
            queued_size: 700,
            total: 10 + 5 + 8 + 7, // Correct total
        };

        // Call the assert_invariants method to check if the invariants are correct
        pool_size.assert_invariants();
    }

    #[test]
    #[should_panic]
    fn test_pool_size_invariants_fail() {
        let pool_size = PoolSize {
            pending: 10,
            pending_size: 1000,
            blob: 5,
            blob_size: 500,
            basefee: 8,
            basefee_size: 800,
            queued: 7,
            queued_size: 700,
            total: 10 + 5 + 8, // Incorrect total
        };

        // Call the assert_invariants method, which should panic
        pool_size.assert_invariants();
    }

    #[test]
    fn test_eth_pooled_transaction_new_legacy() {
        // Create a legacy transaction with specific parameters
        let tx = TxEnvelope::Legacy(
            TxLegacy {
                gas_price: 10,
                gas_limit: 1000,
                value: U256::from(100),
                ..Default::default()
            }
            .into_signed(Signature::test_signature()),
        );
        let transaction = Recovered::new_unchecked(tx, Default::default());
        let pooled_tx = EthPooledTransaction::new(transaction.clone(), 200);

        // Check that the pooled transaction is created correctly
        assert_eq!(pooled_tx.transaction, transaction);
        assert_eq!(pooled_tx.encoded_length, 200);
        assert_eq!(pooled_tx.blob_sidecar, EthBlobTransactionSidecar::None);
        assert_eq!(pooled_tx.cost, U256::from(100) + U256::from(10 * 1000));
    }

    #[test]
    fn test_eth_pooled_transaction_new_eip2930() {
        // Create an EIP-2930 transaction with specific parameters
        let tx = TxEnvelope::Eip2930(
            TxEip2930 {
                gas_price: 10,
                gas_limit: 1000,
                value: U256::from(100),
                ..Default::default()
            }
            .into_signed(Signature::test_signature()),
        );
        let transaction = Recovered::new_unchecked(tx, Default::default());
        let pooled_tx = EthPooledTransaction::new(transaction.clone(), 200);
        let expected_cost = U256::from(100) + (U256::from(10 * 1000));

        assert_eq!(pooled_tx.transaction, transaction);
        assert_eq!(pooled_tx.encoded_length, 200);
        assert_eq!(pooled_tx.blob_sidecar, EthBlobTransactionSidecar::None);
        assert_eq!(pooled_tx.cost, expected_cost);
    }

    #[test]
    fn test_eth_pooled_transaction_new_eip1559() {
        // Create an EIP-1559 transaction with specific parameters
        let tx = TxEnvelope::Eip1559(
            TxEip1559 {
                max_fee_per_gas: 10,
                gas_limit: 1000,
                value: U256::from(100),
                ..Default::default()
            }
            .into_signed(Signature::test_signature()),
        );
        let transaction = Recovered::new_unchecked(tx, Default::default());
        let pooled_tx = EthPooledTransaction::new(transaction.clone(), 200);

        // Check that the pooled transaction is created correctly
        assert_eq!(pooled_tx.transaction, transaction);
        assert_eq!(pooled_tx.encoded_length, 200);
        assert_eq!(pooled_tx.blob_sidecar, EthBlobTransactionSidecar::None);
        assert_eq!(pooled_tx.cost, U256::from(100) + U256::from(10 * 1000));
    }

    #[test]
    fn test_eth_pooled_transaction_new_eip4844() {
        // Create an EIP-4844 transaction with specific parameters
        let tx = EthereumTxEnvelope::Eip4844(
            TxEip4844 {
                max_fee_per_gas: 10,
                gas_limit: 1000,
                value: U256::from(100),
                max_fee_per_blob_gas: 5,
                blob_versioned_hashes: vec![B256::default()],
                ..Default::default()
            }
            .into_signed(Signature::test_signature()),
        );
        let transaction = Recovered::new_unchecked(tx, Default::default());
        let pooled_tx = EthPooledTransaction::new(transaction.clone(), 300);

        // Check that the pooled transaction is created correctly
        assert_eq!(pooled_tx.transaction, transaction);
        assert_eq!(pooled_tx.encoded_length, 300);
        assert_eq!(pooled_tx.blob_sidecar, EthBlobTransactionSidecar::Missing);
        let expected_cost =
            U256::from(100) + U256::from(10 * 1000) + U256::from(5 * DATA_GAS_PER_BLOB);
        assert_eq!(pooled_tx.cost, expected_cost);
    }

    #[test]
    fn test_eth_pooled_transaction_new_eip7702() {
        // Init an EIP-7702 transaction with specific parameters
        let tx = EthereumTxEnvelope::<TxEip4844>::Eip7702(
            TxEip7702 {
                max_fee_per_gas: 10,
                gas_limit: 1000,
                value: U256::from(100),
                ..Default::default()
            }
            .into_signed(Signature::test_signature()),
        );
        let transaction = Recovered::new_unchecked(tx, Default::default());
        let pooled_tx = EthPooledTransaction::new(transaction.clone(), 200);

        // Check that the pooled transaction is created correctly
        assert_eq!(pooled_tx.transaction, transaction);
        assert_eq!(pooled_tx.encoded_length, 200);
        assert_eq!(pooled_tx.blob_sidecar, EthBlobTransactionSidecar::None);
        assert_eq!(pooled_tx.cost, U256::from(100) + U256::from(10 * 1000));
    }

    #[test]
    fn test_pooled_transaction_limit() {
        // No limit should never exceed
        let limit_none = GetPooledTransactionLimit::None;
        // Any size should return false
        assert!(!limit_none.exceeds(1000));

        // Size limit of 2MB (2 * 1024 * 1024 bytes)
        let size_limit_2mb = GetPooledTransactionLimit::ResponseSizeSoftLimit(2 * 1024 * 1024);

        // Test with size below the limit
        // 1MB is below 2MB, should return false
        assert!(!size_limit_2mb.exceeds(1024 * 1024));

        // Test with size exactly at the limit
        // 2MB equals the limit, should return false
        assert!(!size_limit_2mb.exceeds(2 * 1024 * 1024));

        // Test with size exceeding the limit
        // 3MB is above the 2MB limit, should return true
        assert!(size_limit_2mb.exceeds(3 * 1024 * 1024));
    }
}
