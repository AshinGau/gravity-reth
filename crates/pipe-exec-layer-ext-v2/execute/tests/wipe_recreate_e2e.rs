//! End-to-end coverage of the self-destruct / **wipe + recreate** path fixed by commit
//! `5eb45ba990` (`fix(state_root): nested-trie wipe+recreate drops recreated storage`,
//! Galxe/gravity-audit#715) — driven through the *real* Gravity pipe execution layer (execute →
//! cache → merklize → persist), not a unit harness.
//!
//! # Why this test has to be synthetic
//!
//! After EIP-6780 (Cancun) a cross-transaction `SELFDESTRUCT` no longer deletes the account or
//! wipes its storage, so genuine "account with **persisted** storage gets wiped and recreated with
//! new storage" does not occur in post-Cancun traffic. The `mainnet_replay` test can reach it by
//! replaying a pre-Cancun range (it activates forks at their real mainnet timestamps), but that
//! needs network access and a hand-picked range. This test is the deterministic, offline version:
//! a tiny **post-Merge / pre-Shanghai ("Paris")** chain, where `SELFDESTRUCT` still has its classic
//! wipe-the-whole-account semantics, that exercises the path by construction on every run.
//!
//! # Scenario
//!
//! Genesis seeds a funded EOA and a CREATE2 **factory** `F` whose code deterministically deploys a
//! fixed **child** initcode at a fixed address `C`. The child, on construction, writes storage
//! slots `0,1,2 = NUMBER`; its runtime is a dispatcher — empty calldata ⇒ `SELFDESTRUCT(caller)`,
//! non-empty calldata ⇒ `SSTORE(3, 5)`.
//!
//! | block | transactions | effect on `C` |
//! |-------|--------------|----------------|
//! | 1 | `EOA → F` | `C` created, storage `{0,1,2}=1` — **persisted** to disk |
//! | 2 | `EOA → C` (empty), `EOA → F` | `C` self-destructed (storage wiped) **then** recreated, storage `{0,1,2}=2` — the wipe+recreate |
//! | 3 | `EOA → C` (`0x01`) | `SSTORE(3,5)` — re-touches `C`, forcing its storage trie to be read back |
//!
//! Block 2 is the wipe+recreate: `C` had persisted storage, is destroyed, and is recreated with a
//! fresh, non-empty storage set in the same block. That is exactly the `wiped == true` &&
//! non-empty-`storage_nodes` case the fix restored.
//!
//! # What is asserted
//!
//! 1. **Correctness** — the cross-implementation differential the rest of the suite relies on: the
//!    nested-trie tip root the pipeline produced equals reth's independent standard-MPT root over
//!    the same post-persist state. With the pre-fix code (recreated nodes dropped) block 3's
//!    read-back of `C`'s storage trie is incomplete and the two roots disagree.
//! 2. **State** — `C`'s recreated storage is exactly `{0:2, 1:2, 2:2, 3:5}` at the tip.
//!
//! Run with (no RPC needed — fully self-contained):
//! ```text
//! cargo test --release -p reth-pipe-exec-layer-ext-v2 --features pipe_test --test wipe_recreate_e2e -- --nocapture
//! ```
//! `--release` is required for the same reason as `mainnet_replay`: the debug-only
//! `validate_execution_output` faithfulness check does not apply to synthetic blocks.

#![allow(clippy::type_complexity, unreachable_pub)]

use alloy_consensus::{Header, SignableTransaction, TxEip1559};
use alloy_primitives::{address, Address, Bytes, TxKind, B256, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use reth_chainspec::ChainSpec;
use reth_cli_commands::{launcher::FnLauncher, NodeCommand};
use reth_cli_runner::CliRunner;
use reth_db::DatabaseEnv;
use reth_ethereum_cli::chainspec::EthereumChainSpecParser;
use reth_ethereum_primitives::{Block, BlockBody, Transaction, TransactionSigned};
use reth_node_api::NodeTypesWithDBAdapter;
use reth_node_builder::{EngineNodeLauncher, NodeBuilder, WithLaunchContext};
use reth_node_ethereum::{node::EthereumAddOns, EthereumNode};
use reth_pipe_exec_layer_ext_v2::{new_pipe_exec_layer_api, ExecutionArgs};
use reth_primitives_traits::RecoveredBlock;
use reth_provider::{
    providers::BlockchainProvider, writer::UnifiedStorageWriter, BlockHashReader, BlockNumReader,
    DatabaseProviderFactory, HeaderProvider, StateProvider, StateProviderFactory, TrieWriterV2,
};
use reth_tracing::{
    tracing_subscriber::filter::LevelFilter, LayerInfo, LogFormat, RethTracer, Tracer,
};
use reth_trie_parallel::nested_hash::NestedStateRoot;
use std::{collections::BTreeMap, sync::Arc};
use tracing::info;

use gravity_storage::block_view_storage::BlockViewStorage;

type Provider = BlockchainProvider<NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>>;

const CHAIN_ID: u64 = 1;

/// Anvil account 0 — the funded tx sender.
const FUNDED_PRIVKEY: [u8; 32] = [
    0xac, 0x09, 0x74, 0xbe, 0xc3, 0x9a, 0x17, 0xe3, 0x6b, 0xa4, 0xa6, 0xb4, 0xd2, 0x38, 0xff, 0x94,
    0x4b, 0xac, 0xb4, 0x78, 0xcb, 0xed, 0x5e, 0xfc, 0xae, 0x78, 0x4d, 0x7b, 0xf4, 0xf2, 0xff, 0x80,
];
const FUNDED_ADDR: Address = address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

/// Fixed address the CREATE2 factory lives at in genesis.
const FACTORY_ADDR: Address = address!("0x000000000000000000000000000000000000fac7");

/// Child contract **initcode**, CREATE2'd by the factory. On construction it does
/// `SSTORE(0,NUMBER); SSTORE(1,NUMBER); SSTORE(2,NUMBER)` then returns the runtime below; the
/// CREATE2 address therefore only depends on these bytes (so every deployment lands at the same
/// address `C`), while the stored *values* differ per block via `NUMBER`.
///
/// Runtime dispatcher (13 bytes): `CALLDATASIZE; PUSH1 dest; JUMPI` — empty calldata falls through
/// to `CALLER; SELFDESTRUCT`; non-empty jumps to `PUSH1 5; PUSH1 3; SSTORE; STOP`.
///
/// ```text
/// 43 60 00 55  NUMBER PUSH1 0 SSTORE     ; storage[0] = NUMBER
/// 43 60 01 55  NUMBER PUSH1 1 SSTORE     ; storage[1] = NUMBER
/// 43 60 02 55  NUMBER PUSH1 2 SSTORE     ; storage[2] = NUMBER
/// 60 0d 60 18 60 00 39   PUSH1 13 PUSH1 24 PUSH1 0 CODECOPY   ; copy 13-byte runtime
/// 60 0d 60 00 f3         PUSH1 13 PUSH1 0 RETURN
/// ; runtime @ offset 24:
/// 36 60 06 57  CALLDATASIZE PUSH1 6 JUMPI
/// 33 ff        CALLER SELFDESTRUCT       ; empty calldata -> destroy
/// 5b           JUMPDEST (@6)
/// 60 05 60 03 55 00   PUSH1 5 PUSH1 3 SSTORE STOP   ; non-empty -> storage[3] = 5
/// ```
const CHILD_INITCODE_HEX: &str =
    "436000554360015543600255600d6018600039600d6000f33660065733ff5b600560035500";

/// Factory **runtime** code (placed directly in genesis at [`FACTORY_ADDR`]): copies the 37-byte
/// child initcode (appended after this 17-byte prefix) into memory and `CREATE2`s it with salt 0.
///
/// ```text
/// 60 25 60 11 60 00 39   PUSH1 37 PUSH1 17 PUSH1 0 CODECOPY   ; mem[0..37] = child initcode
/// 60 00 60 25 60 00 60 00 f5   PUSH1 salt PUSH1 len PUSH1 off PUSH1 val CREATE2
/// 00                     STOP
/// ```
const FACTORY_RUNTIME_HEX: &str = "602560116000396000602560006000f500";

fn child_initcode() -> Bytes {
    Bytes::from(hex::decode(CHILD_INITCODE_HEX).unwrap())
}

/// Full factory code = runtime prefix ++ child initcode (the prefix `CODECOPY`s the appended
/// bytes).
fn factory_code() -> Bytes {
    let mut code = hex::decode(FACTORY_RUNTIME_HEX).unwrap();
    code.extend(hex::decode(CHILD_INITCODE_HEX).unwrap());
    Bytes::from(code)
}

/// Deterministic CREATE2 address of the child contract `C`.
fn child_address() -> Address {
    FACTORY_ADDR.create2_from_code(B256::ZERO, child_initcode())
}

// ===========================================================================================
// Synthetic Paris (post-Merge, pre-Shanghai) genesis
// ===========================================================================================

/// Build a geth-format genesis activating every fork up to the Merge but **not** Shanghai/Cancun,
/// so `SELFDESTRUCT` keeps its classic account-wiping semantics. `alloc` = funded EOA + factory.
fn build_paris_genesis() -> String {
    let genesis = serde_json::json!({
        "config": {
            "chainId": CHAIN_ID,
            "homesteadBlock": 0,
            "eip150Block": 0,
            "eip155Block": 0,
            "eip158Block": 0,
            "byzantiumBlock": 0,
            "constantinopleBlock": 0,
            "petersburgBlock": 0,
            "istanbulBlock": 0,
            "berlinBlock": 0,
            "londonBlock": 0,
            "mergeNetsplitBlock": 0,
            "terminalTotalDifficulty": 0,
            "terminalTotalDifficultyPassed": true
        },
        "nonce": "0x0",
        "timestamp": "0x0",
        "gasLimit": "0x1c9c380",
        "difficulty": "0x0",
        "coinbase": "0x0000000000000000000000000000000000000000",
        "alloc": {
            format!("{FUNDED_ADDR:?}"): { "balance": "0xd3c21bcecceda1000000" /* 1e6 ETH */ },
            format!("{FACTORY_ADDR:?}"): {
                "balance": "0x0",
                "code": factory_code().to_string()
            }
        }
    });
    serde_json::to_string(&genesis).unwrap()
}

// ===========================================================================================
// Transaction / block construction
// ===========================================================================================

fn funded_signer() -> PrivateKeySigner {
    PrivateKeySigner::from_bytes(&B256::from(FUNDED_PRIVKEY)).expect("funded key parses")
}

/// Build and sign an EIP-1559 call from the funded EOA.
fn signed_call(
    signer: &PrivateKeySigner,
    nonce: u64,
    to: Address,
    input: Bytes,
) -> TransactionSigned {
    let tx = TxEip1559 {
        chain_id: CHAIN_ID,
        nonce,
        gas_limit: 1_000_000,
        max_fee_per_gas: 1_000_000_000,
        max_priority_fee_per_gas: 0,
        to: TxKind::Call(to),
        value: U256::ZERO,
        access_list: Default::default(),
        input,
    };
    let sig = signer.sign_hash_sync(&tx.signature_hash()).expect("sign");
    let signed = tx.into_signed(sig);
    let (tx, sig, _hash) = signed.into_parts();
    let signed_tx = TransactionSigned::new_unhashed(Transaction::Eip1559(tx), sig);
    let _ = signed_tx.hash();
    signed_tx
}

/// Assemble a Paris block carrying `txs` at height `number` (`parent_hash`/`state_root`/roots are
/// all overwritten or filled by the pipe, so only the EVM-env fields below need to be valid).
fn build_block(number: u64, txs: Vec<TransactionSigned>) -> RecoveredBlock<Block> {
    let header = Header {
        number,
        gas_limit: 30_000_000,
        timestamp: 1_700_000_000 + number,
        base_fee_per_gas: Some(7),
        ..Default::default()
    };
    let body = BlockBody { transactions: txs, ommers: vec![], withdrawals: None };
    RecoveredBlock::try_recover(Block { header, body }).expect("recover senders")
}

// ===========================================================================================
// Harness
// ===========================================================================================

async fn run(
    builder: WithLaunchContext<NodeBuilder<Arc<DatabaseEnv>, ChainSpec>>,
) -> eyre::Result<()> {
    let handle = builder
        .with_types_and_provider::<EthereumNode, BlockchainProvider<_>>()
        .with_components(EthereumNode::components())
        .with_add_ons(EthereumAddOns::default())
        .launch_with_fn(|builder| {
            let launcher = EngineNodeLauncher::new(
                builder.task_executor().clone(),
                builder.config().datadir(),
                reth_engine_primitives::TreeConfig::default()
                    .with_persistence_threshold(0)
                    .with_memory_block_buffer_target(0),
            );
            builder.launch_with(launcher)
        })
        .await?;

    let chain_spec = handle.node.chain_spec();
    let eth_api = handle.node.rpc_registry.eth_api().clone();
    let provider: Provider = handle.node.provider;

    // Genesis is block 0; seed the nested trie from its (EOA + factory) hashed state.
    let db_provider = provider.database_provider_ro().unwrap();
    assert_eq!(db_provider.best_block_number().unwrap(), 0, "fresh datadir starts at genesis");
    let genesis_hash = db_provider.block_hash(0).unwrap().unwrap();
    let genesis_header = db_provider.header_by_number(0).unwrap().unwrap();
    drop(db_provider);

    let tx = provider.database_provider_ro().unwrap().into_tx();
    let nested = NestedStateRoot::new(&tx, None);
    let hashed = nested.read_hashed_state(None).unwrap();
    let (_root, updates) = nested.calculate(&hashed).unwrap();
    drop(tx);
    let w = provider.database_provider_rw().unwrap();
    w.write_trie_updatesv2(&updates).unwrap();
    UnifiedStorageWriter::commit_unwind(w)?;

    let storage = BlockViewStorage::new(provider.clone());
    let (tx_args, rx_args) = tokio::sync::oneshot::channel();
    let pipeline_api = new_pipe_exec_layer_api(
        chain_spec,
        storage,
        genesis_header,
        genesis_hash,
        rx_args,
        eth_api,
    );
    tx_args.send(ExecutionArgs { block_number_to_block_id: BTreeMap::new() }).unwrap();

    // Build the three-block scenario.
    let signer = funded_signer();
    let factory = FACTORY_ADDR;
    let child = child_address();
    info!(?child, "child (C) CREATE2 address");

    let blocks = vec![
        // Block 1: deploy C (storage {0,1,2}=1) — persisted to disk.
        build_block(1, vec![signed_call(&signer, 0, factory, Bytes::new())]),
        // Block 2: destroy C (empty calldata) then recreate it (storage {0,1,2}=2) — the
        // wipe+recreate: C had persisted storage, is wiped, and is rebuilt with new, non-empty
        // storage in the same block (the exact `wiped && non-empty` case the #715 fix restored).
        build_block(
            2,
            vec![
                signed_call(&signer, 1, child, Bytes::new()),
                signed_call(&signer, 2, factory, Bytes::new()),
            ],
        ),
        // Block 3: re-touch C's storage (non-empty calldata -> SSTORE(3,5)). This forces C's
        // storage trie to be read back from the cache the wipe+recreate just wrote; with the
        // pre-fix code (recreated nodes dropped) the read is incomplete and the oracle below
        // diverges.
        build_block(3, vec![signed_call(&signer, 3, child, Bytes::from(vec![0x01]))]),
    ];
    let count = blocks.len() as u64;

    for block in blocks {
        pipeline_api.push_history_block(block).expect("push history block");
    }
    for expected in 1..=count {
        let result = pipeline_api.pull_executed_block_hash().await.expect("pull executed");
        assert_eq!(result.block_number, expected, "blocks execute in order");
        pipeline_api.commit_executed_block_hash(result.block_id, None).expect("commit");
    }
    pipeline_api.wait_for_block_persistence(count).await.expect("persistence");

    // ---- Correctness gate: nested-trie tip root vs independent standard MPT. -----------------
    let r_incr = provider
        .database_provider_ro()
        .unwrap()
        .header_by_number(count)
        .unwrap()
        .expect("tip header persisted")
        .state_root;
    let tx = provider.database_provider_ro().unwrap().into_tx();
    let full_state = NestedStateRoot::new(&tx, None).read_hashed_state(None).unwrap();
    let r_std = standard_mpt_root(&full_state);
    drop(tx);
    info!(?r_incr, ?r_std, "differential oracle");
    assert_eq!(
        r_incr, r_std,
        "nested-trie tip root disagrees with the standard-MPT root over the same state — the \
         recreated storage nodes were dropped (Galxe/gravity-audit#715 regression)"
    );

    // ---- State gate: C is recreated as exactly {0:2, 1:2, 2:2, 3:5} at the tip. --------------
    let state =
        provider.state_by_block_number_or_tag(alloy_eips::BlockNumberOrTag::Number(count)).unwrap();
    for (slot, want) in [(0u64, 2u64), (1, 2), (2, 2), (3, 5)] {
        let got = state.storage(child, B256::from(U256::from(slot))).unwrap().unwrap_or_default();
        assert_eq!(got, U256::from(want), "C.storage[{slot}] mismatch");
    }
    info!("wipe+recreate e2e OK: C={{0:2,1:2,2:2,3:5}}, tip root {r_incr:?}");
    Ok(())
}

/// Canonical Ethereum state root via reth's standard MPT — independent of the nested trie.
fn standard_mpt_root(state: &reth_trie::HashedPostState) -> B256 {
    let accounts = state.accounts.iter().filter_map(|(hashed_addr, maybe_acct)| {
        let acct = (*maybe_acct)?;
        let storage = state
            .storages
            .get(hashed_addr)
            .map(|s| s.storage.iter().map(|(k, v)| (*k, *v)).collect::<Vec<_>>())
            .unwrap_or_default();
        Some((*hashed_addr, (acct, storage)))
    });
    reth_trie::test_utils::state_root_prehashed(accounts)
}

// ===========================================================================================
// Test entry
// ===========================================================================================

#[test]
fn wipe_recreate_e2e() {
    std::panic::set_hook(Box::new(|panic_info| {
        let backtrace = std::backtrace::Backtrace::capture();
        eprintln!("Panic occurred: {panic_info}\nBacktrace:\n{backtrace}");
        std::process::exit(1);
    }));

    let _ = RethTracer::new()
        .with_stdout(LayerInfo::new(
            LogFormat::Terminal,
            LevelFilter::INFO.to_string(),
            String::new(),
            Some("always".to_string()),
        ))
        .init();

    let work = std::env::temp_dir().join(format!("gravity_wipe_recreate_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).unwrap();
    let genesis_file = work.join("genesis.json");
    std::fs::write(&genesis_file, build_paris_genesis()).unwrap();
    let datadir = work.join("datadir");

    let runner = CliRunner::try_default_runtime().unwrap();
    let args: Vec<String> = vec![
        "reth".into(),
        "--chain".into(),
        genesis_file.to_str().unwrap().into(),
        "--with-unused-ports".into(),
        "--dev".into(),
        "--datadir".into(),
        datadir.to_str().unwrap().into(),
    ];
    let command: NodeCommand<EthereumChainSpecParser> =
        NodeCommand::try_parse_args_from(args).unwrap();

    runner
        .run_command_until_exit(|ctx| {
            command.execute(
                ctx,
                FnLauncher::new::<EthereumChainSpecParser, _>(move |builder, _| async move {
                    run(builder).await
                }),
            )
        })
        .unwrap();

    let _ = std::fs::remove_dir_all(&work);
}
