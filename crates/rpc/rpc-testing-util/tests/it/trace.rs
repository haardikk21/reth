//! Integration tests for the trace API.

use alloy_primitives::map::HashSet;
use alloy_rpc_types_eth::{Block, Header, Transaction, TransactionRequest};
use alloy_rpc_types_trace::{
    filter::TraceFilter, parity::TraceType, tracerequest::TraceCallRequest,
};
use futures::StreamExt;
use jsonrpsee::http_client::HttpClientBuilder;
use jsonrpsee_http_client::HttpClient;
use reth_ethereum_primitives::Receipt;
use reth_rpc_api_testing_util::{debug::DebugApiExt, trace::TraceApiExt, utils::parse_env_url};
use reth_rpc_eth_api::EthApiClient;
use std::time::Instant;

/// This is intended to be run locally against a running node.
///
/// This is a noop of env var `RETH_RPC_TEST_NODE_URL` is not set.
#[tokio::test(flavor = "multi_thread")]
async fn trace_many_blocks() {
    let url = parse_env_url("RETH_RPC_TEST_NODE_URL");
    if url.is_err() {
        return
    }
    let url = url.unwrap();

    let client = HttpClientBuilder::default().build(url).unwrap();
    let mut stream = client.trace_block_buffered_unordered(15_000_000..=16_000_100, 20);
    let now = Instant::now();
    while let Some((err, block)) = stream.next_err().await {
        eprintln!("Error tracing block {block:?}: {err}");
    }
    println!("Traced all blocks in {:?}", now.elapsed());
}

/// Tests the replaying of transactions on a local Ethereum node.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn replay_transactions() {
    let url = parse_env_url("RETH_RPC_TEST_NODE_URL").unwrap();
    let client = HttpClientBuilder::default().build(url).unwrap();

    let tx_hashes = vec![
        "0x4e08fe36db723a338e852f89f613e606b0c9a17e649b18b01251f86236a2cef3".parse().unwrap(),
        "0xea2817f1aeeb587b82f4ab87a6dbd3560fc35ed28de1be280cb40b2a24ab48bb".parse().unwrap(),
    ];

    let trace_types = HashSet::from_iter([TraceType::StateDiff, TraceType::VmTrace]);

    let mut stream = client.replay_transactions(tx_hashes, trace_types);
    let now = Instant::now();
    while let Some(replay_txs) = stream.next().await {
        println!("Transaction: {replay_txs:?}");
        println!("Replayed transactions in {:?}", now.elapsed());
    }
}

/// Tests the tracers filters on a local Ethereum node
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn trace_filters() {
    // Parse the node URL from environment variable and create an HTTP client.
    let url = parse_env_url("RETH_RPC_TEST_NODE_URL").unwrap();
    let client = HttpClientBuilder::default().build(url).unwrap();

    // Set up trace filters.
    let filter = TraceFilter::default();
    let filters = vec![filter];

    // Initialize a stream for the trace filters.
    let mut stream = client.trace_filter_stream(filters);
    let start_time = Instant::now();
    while let Some(trace) = stream.next().await {
        println!("Transaction Trace: {trace:?}");
        println!("Duration since test start: {:?}", start_time.elapsed());
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn trace_call() {
    let url = parse_env_url("RETH_RPC_TEST_NODE_URL").unwrap();
    let client = HttpClientBuilder::default().build(url).unwrap();
    let trace_call_request = TraceCallRequest::default();
    let mut stream = client.trace_call_stream(trace_call_request);
    let start_time = Instant::now();

    while let Some(result) = stream.next().await {
        match result {
            Ok(trace_result) => {
                println!("Trace Result: {trace_result:?}");
            }
            Err((error, request)) => {
                eprintln!("Error for request {request:?}: {error:?}");
            }
        }
    }

    println!("Completed in {:?}", start_time.elapsed());
}

/// This is intended to be run locally against a running node. This traces all blocks for a given
/// chain.
///
/// This is a noop of env var `RETH_RPC_TEST_NODE_URL` is not set.
#[tokio::test(flavor = "multi_thread")]
async fn debug_trace_block_entire_chain() {
    let url = parse_env_url("RETH_RPC_TEST_NODE_URL");
    if url.is_err() {
        return
    }
    let url = url.unwrap();

    let client = HttpClientBuilder::default().build(url).unwrap();
    let current_block: u64 = <HttpClient as EthApiClient<
        TransactionRequest,
        Transaction,
        Block,
        Receipt,
        Header,
    >>::block_number(&client)
    .await
    .unwrap()
    .try_into()
    .unwrap();
    let range = 0..=current_block;
    let mut stream = client.debug_trace_block_buffered_unordered(range, None, 20);
    let now = Instant::now();
    while let Some((err, block)) = stream.next_err().await {
        eprintln!("Error tracing block {block:?}: {err}");
    }
    println!("Traced all blocks in {:?}", now.elapsed());
}

/// This is intended to be run locally against a running node. This traces all blocks for a given
/// chain.
///
/// This is a noop of env var `RETH_RPC_TEST_NODE_URL` is not set.
#[tokio::test(flavor = "multi_thread")]
async fn debug_trace_block_opcodes_entire_chain() {
    let opcodes7702 = ["EXTCODESIZE", "EXTCODECOPY", "EXTCODEHASH"];
    let url = parse_env_url("RETH_RPC_TEST_NODE_URL");
    if url.is_err() {
        return
    }
    let url = url.unwrap();

    let client = HttpClientBuilder::default().build(url).unwrap();
    let current_block: u64 = <HttpClient as EthApiClient<
        TransactionRequest,
        Transaction,
        Block,
        Receipt,
        Header,
    >>::block_number(&client)
    .await
    .unwrap()
    .try_into()
    .unwrap();
    let range = 0..=current_block;
    println!("Tracing blocks {range:?} for opcodes");
    let mut stream = client.trace_block_opcode_gas_unordered(range, 2).enumerate();
    let now = Instant::now();
    while let Some((num, next)) = stream.next().await {
        match next {
            Ok((block_opcodes, block)) => {
                for opcode in opcodes7702 {
                    if block_opcodes.contains(opcode) {
                        eprintln!("Found opcode {opcode}: in {block}");
                    }
                }
            }
            Err((err, block)) => {
                eprintln!("Error tracing block {block:?}: {err}");
            }
        };
        if num % 10000 == 0 {
            println!("Traced {num} blocks");
        }
    }
    println!("Traced all blocks in {:?}", now.elapsed());
}
