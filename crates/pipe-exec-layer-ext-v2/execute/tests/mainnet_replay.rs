//! Replay an arbitrary range of **real Ethereum mainnet blocks** through the Gravity pipeline
//! execution layer to verify the correctness of the nested trie ([`NestedStateRoot`]), the
//! persist block cache ([`PersistBlockCache`]) and on-disk persistence — driven by realistic,
//! real-world state transitions (creates, self-destructs, wipe+recreate, storage churn, EIP-7702
//! delegations, …) instead of a fixed bundled dataset.
//!
//! # How it works
//!
//! Unlike `pipe_test.rs` (which depends on a pre-synced datadir and replays whatever blocks it
//! happens to contain), this harness reconstructs everything an arbitrary range needs straight
//! from a JSON-RPC archive node, so it can start at **any** block:
//!
//! 1. **Fetch** (once, cached to disk): for each block in `[start, start+count)` we pull the raw
//!    block (`debug_getRawBlock`) and its opening read-set (`debug_traceBlockByNumber` +
//!    `prestateTracer`, merged "first-seen wins" into one union prestate). EIP-7702 delegation
//!    targets — whose code `prestateTracer` omits — are supplemented via `eth_getCode`.
//! 2. **Seed**: the union prestate becomes the `alloc` of a synthetic genesis (block 0). The chain
//!    spec activates the time-based forks at their real mainnet timestamps (see "Hardforks" below),
//!    so each block executes under the fork set that was live for it on mainnet.
//! 3. **Replay**: the real blocks are **renumbered** to `1..=count` (genesis is block 0) and pushed
//!    through the real pipe execution layer. Renumbering is what lets persistence append `1,2,3,…`
//!    contiguously from genesis without fabricating millions of ancestor headers. The pipe
//!    overwrites each block's `state_root`/`parent_hash` with its own computation, so the resulting
//!    chain is self-consistent and block hashes simply don't (need to) match mainnet.
//!
//! # Execution divergence and sender headroom
//!
//! Renumbering changes the `NUMBER` opcode, and without a real ancestor chain `BLOCKHASH` /
//! EIP-2935 cannot return mainnet hashes (grevm avoids this by *not* renumbering — it executes
//! each block at its true height against an in-memory DB; we cannot, because reth persistence is
//! anchored at genesis). A contract whose gas or control flow depends on those opcodes therefore
//! executes a hair differently than on mainnet. Usually that only perturbs gas/state-root by a
//! little, but it can leave a transaction *sender* a few gwei short of its mainnet balance — enough
//! to turn an otherwise-valid tx into a fatal `LackOfFundForMaxFee` (a pre-execution validation
//! error), which `execute_history_block` treats as unrecoverable and aborts the whole run.
//!
//! This does not matter for what we are testing: the correctness gate (below) is a
//! *self-consistent* differential — the nested trie vs reth's standard MPT over whatever state
//! replay produces — not a comparison against mainnet, so bit-exact execution fidelity is
//! unnecessary. To stop a stray divergence from aborting the run we simply grant every transaction
//! sender generous balance headroom in the synthetic genesis ([`grant_sender_headroom`]). That
//! touches only EOA balances; all trie-relevant churn (creates, self-destructs, storage writes,
//! wipe+recreate, 7702 delegations) is driven by contract execution and is left completely intact.
//!
//! # Oracle
//!
//! The hard correctness gate is a **cross-implementation differential**: after draining persist,
//! the nested-trie state root the pipeline produced for the tip is compared against the root of
//! reth's completely independent standard-MPT implementation ([`state_root_prehashed`]) over the
//! same post-persist state. A bug in the incremental nested-trie / cache / persist path (e.g. the
//! wipe+recreate node-drop of Galxe/gravity-audit#715) makes the two diverge. We additionally
//! assert no real transaction was unexpectedly discarded.
//!
//! # Running
//!
//! ```text
//! PIPE_TEST_RPC=<archive-rpc-url> \
//! PIPE_TEST_START_BLOCK=22500000 \
//! PIPE_TEST_COUNT=20 \
//! cargo test --release -p reth-pipe-exec-layer-ext-v2 --features pipe_test --test mainnet_replay -- --nocapture
//! ```
//!
//! - The `pipe_test` feature is required (it makes the on-chain epoch fetch return `0`, so no
//!   Gravity system contracts are needed).
//! - **`--release` is required.** In debug builds the pipe runs a `#[cfg(debug_assertions)]`
//!   *faithfulness* check (`validate_execution_output`) that asserts the executed gas equals the
//!   block header's `gas_used`. Partial-state replay (no ancestor chain ⇒ `BLOCKHASH`/EIP-2935
//!   diverge, and renumbering changes `NUMBER`) intentionally diverges from the exact mainnet block
//!   by a tiny amount, so that check does not apply here. The hard correctness gate is the
//!   cross-implementation trie differential below, which is independent of execution fidelity: any
//!   wipe+recreate / self-destruct / storage churn the real txs produce still has to root
//!   correctly, whatever the exact gas.
//! - When `PIPE_TEST_RPC` is unset the test skips.
//!
//! ## Environment variables
//!
//! | var | meaning |
//! |-----|---------|
//! | `PIPE_TEST_RPC` | archive JSON-RPC URL (HTTP); unset ⇒ skip |
//! | `PIPE_TEST_START_BLOCK` | first mainnet block to replay |
//! | `PIPE_TEST_COUNT` | number of consecutive blocks (default 1) |
//! | `PIPE_TEST_DATA_DIR` | fetched-fixtures dir (default `$TMPDIR/gravity_mainnet_fixtures`) |
//! | `PIPE_TEST_CACHE_CAPACITY` | `--gravity.cache.capacity` (min 1000) — shrink to exercise cache eviction |
//! | `PIPE_TEST_PERSIST_GAP` | `--gravity.cache.max-persist-gap` — widen to let the cache shadow the lagging DB longer |
//! | `GRETH_CACHE_METRICS_INTERVAL_MS` | cache eviction-daemon cadence in ms (default 15000); shrink so eviction fires within a short run |
//!
//! ## Testing the `cache.rs` refactor
//!
//! The cache *read* paths (tombstone / wipe-marker shadowing of a not-yet-persisted delete — the
//! Galxe/gravity-audit#715 class) are exercised on **every** block automatically: persistence is
//! async, so a block reads its predecessors' writes from the cache before they reach the DB, and
//! any stale read corrupts a trie node and is caught by the differential oracle.
//!
//! The cache *eviction* path is gated by a daemon that ticks every `CACHE_METRICS_INTERVAL`
//! (15s) and reads `cache_capacity` once at startup. To make eviction fire within a short run,
//! shrink the cadence with `GRETH_CACHE_METRICS_INTERVAL_MS` (milliseconds) alongside a small
//! `PIPE_TEST_CACHE_CAPACITY`, e.g.:
//!
//! ```text
//! GRETH_CACHE_METRICS_INTERVAL_MS=200 PIPE_TEST_CACHE_CAPACITY=1000 PIPE_TEST_PERSIST_GAP=2 …
//! ```
//!
//! (Without the cadence override a sub-15s run never evicts regardless of capacity. The eviction
//! watermark logic itself is also covered directly by `cache::tests` unit tests.)
//!
//! ## Hardforks and the wipe+recreate (#715) path
//!
//! The synthetic genesis activates the time-based forks at their **real mainnet timestamps**, so
//! each replayed block — which keeps its real timestamp even though its number is rewritten —
//! executes under exactly the forks that were live for it on mainnet. Mainnet activation heights:
//!
//! | fork (consensus name) | first mainnet block | ~date | EVM change of note |
//! |-----------------------|---------------------|-------|--------------------|
//! | The Merge (Paris)     | `15537394`          | 2022-09-15 | PoS; `DIFFICULTY`→`PREVRANDAO` |
//! | Shanghai (Shapella)   | `17034870`          | 2023-04-12 | withdrawals; `PUSH0` |
//! | Cancun (Dencun)       | `19426587`          | 2024-03-13 | blobs; **EIP-6780 neuters `SELFDESTRUCT`** |
//! | Prague (Pectra)       | `22431084`          | 2025-05-07 | **EIP-7702** set-code txs; EIP-2935 |
//!
//! This matters for self-destruct: pre-Cancun, `SELFDESTRUCT` deletes the account and wipes its
//! storage (classic semantics); EIP-6780 (Cancun) restricts that to contracts created in the same
//! transaction. So the self-destruct + recreate path fixed by `5eb45ba990` only occurs in a
//! **pre-Cancun** range (start below block `19426587`) — post-Cancun ranges legitimately never
//! produce a `wiped && non-empty` storage update. A deterministic, offline version of that path
//! also lives in the dedicated `wipe_recreate_e2e` test.
//!
//! ### Straddling an upgrade
//!
//! Because forks are keyed off each block's real timestamp, a range **centred on an activation
//! block** replays the last N pre-fork blocks and the first N post-fork blocks under their
//! respective rules in a single run — the cleanest way to exercise both regimes (and the fork's
//! one-time transition) together. Centre `start = activation - count/2`:
//!
//! ```text
//! # Cancun boundary — classic vs EIP-6780 SELFDESTRUCT (relevant to wipe+recreate):
//! PIPE_TEST_START_BLOCK=19426562 PIPE_TEST_COUNT=50   # 25 pre-Cancun + 25 Cancun
//! # Shanghai boundary — pre/post withdrawals & PUSH0:
//! PIPE_TEST_START_BLOCK=17034845 PIPE_TEST_COUNT=50
//! # Prague boundary — pre/post EIP-7702 & EIP-2935:
//! PIPE_TEST_START_BLOCK=22431059 PIPE_TEST_COUNT=50
//! ```
//!
//! Note: this straddles **one** boundary per run. Spanning *two distinct* activations in a single
//! range is impractical here — adjacent forks are ~2.4–3M blocks apart and every block's prestate
//! is fetched, so `count` would have to be in the millions.

#![allow(clippy::type_complexity, unreachable_pub)]

use alloy_primitives::{Address, B256, U256};
use alloy_rlp::Decodable;
use reth_chainspec::ChainSpec;
use reth_cli_commands::{launcher::FnLauncher, NodeCommand};
use reth_cli_runner::CliRunner;
use reth_db::DatabaseEnv;
use reth_ethereum_cli::chainspec::EthereumChainSpecParser;
use reth_ethereum_primitives::Block;
use reth_node_api::NodeTypesWithDBAdapter;
use reth_node_builder::{EngineNodeLauncher, NodeBuilder, WithLaunchContext};
use reth_node_ethereum::{node::EthereumAddOns, EthereumNode};
use reth_pipe_exec_layer_ext_v2::{new_pipe_exec_layer_api, ExecutionArgs};
use reth_primitives_traits::RecoveredBlock;
use reth_provider::{
    providers::BlockchainProvider, writer::UnifiedStorageWriter, BlockHashReader, BlockNumReader,
    DatabaseProviderFactory, HeaderProvider, TrieWriterV2,
};
use reth_tracing::{
    tracing_subscriber::filter::LevelFilter, LayerInfo, LogFormat, RethTracer, Tracer,
};
use reth_trie_parallel::nested_hash::NestedStateRoot;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use tracing::info;

use gravity_storage::block_view_storage::BlockViewStorage;

type Provider = BlockchainProvider<NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>>;
type Error = Box<dyn std::error::Error + Send + Sync>;

// ===========================================================================================
// Minimal blocking JSON-RPC client
// ===========================================================================================

mod rpc {
    use super::{fixtures, Error};
    use alloy_primitives::{Address, U256};
    use serde_json::{json, Value};
    use std::{collections::BTreeMap, time::Duration};

    /// A tiny JSON-RPC-over-HTTP client (blocking).
    pub struct Rpc {
        agent: ureq::Agent,
        url: String,
    }

    impl Rpc {
        pub fn new(url: String) -> Self {
            let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(300)).build();
            Self { agent, url }
        }

        /// POST a JSON body, retrying on HTTP 429 / 5xx with exponential backoff.
        fn send(&self, body: &Value) -> Result<Value, Error> {
            let mut delay = Duration::from_millis(400);
            for attempt in 0..7 {
                match self.agent.post(&self.url).send_json(body) {
                    Ok(resp) => return resp.into_json().map_err(Into::into),
                    Err(ureq::Error::Status(code, _)) if code == 429 || code >= 500 => {
                        if attempt == 6 {
                            return Err(format!("HTTP {code} after {} retries", attempt + 1).into());
                        }
                        std::thread::sleep(delay);
                        delay = (delay * 2).min(Duration::from_secs(10));
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            unreachable!()
        }

        /// Issue a JSON-RPC call and return its `result`.
        pub fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
            let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
            let resp = self.send(&body)?;
            if let Some(err) = resp.get("error") {
                return Err(format!("{method} failed: {err}").into());
            }
            Ok(resp.get("result").cloned().unwrap_or(Value::Null))
        }

        pub fn chain_id(&self) -> Result<u64, Error> {
            let v = self.call("eth_chainId", json!([]))?;
            let s = v.as_str().unwrap_or("0x1");
            Ok(u64::from_str_radix(s.trim_start_matches("0x"), 16).unwrap_or(1))
        }

        /// Raw consensus RLP of a block (header + body), via `debug_getRawBlock`.
        pub fn raw_block(&self, number: u64) -> Result<Vec<u8>, Error> {
            let hex = format!("0x{number:x}");
            let v = self.call("debug_getRawBlock", json!([hex]))?;
            let s = v.as_str().ok_or("debug_getRawBlock returned no result")?;
            Ok(hex::decode(s.trim_start_matches("0x"))?)
        }

        /// Full block (with tx objects) — used only to read the transaction list for EIP-7702
        /// delegation-target discovery.
        pub fn block_with_txs(&self, number: u64) -> Result<Value, Error> {
            let hex = format!("0x{number:x}");
            self.call("eth_getBlockByNumber", json!([hex, true]))
        }

        /// `prestateTracer` read-set for a block, accumulated into `merged` (first-seen wins).
        pub fn accumulate_prestate(
            &self,
            number: u64,
            merged: &mut fixtures::PreState,
        ) -> Result<(), Error> {
            let hex = format!("0x{number:x}");
            let trace = self
                .call("debug_traceBlockByNumber", json!([hex, { "tracer": "prestateTracer" }]))?;
            fixtures::accumulate_prestate(merged, &trace).map_err(Into::into)
        }

        /// Supplement the EIP-7702 delegation **targets** of a block's txs (code omitted by
        /// `prestateTracer`) into `merged`, fetched at the parent block. Returns count added.
        pub fn supplement_delegations(
            &self,
            number: u64,
            txs: &Value,
            merged: &mut fixtures::PreState,
            cache: &mut BTreeMap<Address, Option<fixtures::AccountFixture>>,
        ) -> Result<usize, Error> {
            let at = format!("0x{:x}", number.saturating_sub(1));
            let mut added = 0;
            for target in fixtures::delegation_targets(txs, merged) {
                if merged
                    .get(&target)
                    .is_some_and(|a| a.code.as_ref().is_some_and(|c| !c.is_empty()))
                {
                    continue;
                }
                let account = match cache.get(&target) {
                    Some(cached) => cached.clone(),
                    None => {
                        let fetched = self.fetch_account_with_code(target, &at)?;
                        cache.insert(target, fetched.clone());
                        fetched
                    }
                };
                if let Some(account) = account {
                    merged.insert(target, account);
                    added += 1;
                }
            }
            Ok(added)
        }

        fn fetch_account_with_code(
            &self,
            addr: Address,
            at: &str,
        ) -> Result<Option<fixtures::AccountFixture>, Error> {
            let a = addr.to_string();
            let code = self.call("eth_getCode", json!([a, at]))?;
            let code = code.as_str().unwrap_or("0x");
            if code == "0x" || code.is_empty() {
                return Ok(None);
            }
            let balance = self.call("eth_getBalance", json!([a, at]))?;
            let nonce = self.call("eth_getTransactionCount", json!([a, at]))?;
            let balance: U256 =
                balance.as_str().unwrap_or("0x0").parse().map_err(|e| format!("{e:?}"))?;
            let nonce =
                u64::from_str_radix(nonce.as_str().unwrap_or("0x0").trim_start_matches("0x"), 16)
                    .unwrap_or(0);
            let code: alloy_primitives::Bytes = code.parse().map_err(|e| format!("{e:?}"))?;
            Ok(Some(fixtures::AccountFixture {
                balance,
                nonce,
                code: Some(code),
                storage: BTreeMap::new(),
            }))
        }
    }
}

// ===========================================================================================
// On-disk fixture schema (revm/reth-version independent)
// ===========================================================================================

mod fixtures {
    use alloy_primitives::{Address, Bytes, U256};
    use serde::{Deserialize, Serialize};
    use serde_json::Value;
    use std::collections::{BTreeMap, BTreeSet};

    /// Opening state: account address -> snapshot.
    pub type PreState = BTreeMap<Address, AccountFixture>;

    /// A single account's opening snapshot.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct AccountFixture {
        pub balance: U256,
        pub nonce: u64,
        #[serde(default)]
        pub code: Option<Bytes>,
        #[serde(default)]
        pub storage: BTreeMap<U256, U256>,
    }

    /// EIP-7702 delegation designator: `0xef0100 || target` (23 bytes).
    const EIP7702_MAGIC: [u8; 3] = [0xef, 0x01, 0x00];

    /// Accumulate one block's `prestateTracer` response into `merged` ("first-seen wins" at
    /// account and storage-slot granularity).
    pub fn accumulate_prestate(merged: &mut PreState, trace: &Value) -> Result<(), String> {
        let entries = trace.as_array().ok_or("prestate trace is not an array")?;
        for entry in entries {
            let result = entry.get("result").unwrap_or(entry);
            let Some(pre) = result.as_object() else { continue };
            for (addr_str, acc) in pre {
                let addr: Address = addr_str.parse().map_err(|e| format!("{e:?}"))?;
                if let std::collections::btree_map::Entry::Vacant(slot) = merged.entry(addr) {
                    slot.insert(AccountFixture {
                        balance: opt_u256(acc.get("balance"))?.unwrap_or(U256::ZERO),
                        nonce: acc.get("nonce").and_then(Value::as_u64).unwrap_or(0),
                        code: acc
                            .get("code")
                            .and_then(Value::as_str)
                            .filter(|s| *s != "0x")
                            .map(|s| s.parse::<Bytes>())
                            .transpose()
                            .map_err(|e| format!("{e:?}"))?,
                        storage: BTreeMap::new(),
                    });
                }
                if let Some(storage) = acc.get("storage").and_then(Value::as_object) {
                    let account = merged.get_mut(&addr).expect("just inserted");
                    for (slot, val) in storage {
                        let slot = parse_u256(slot)?;
                        let val = parse_u256(val.as_str().unwrap_or("0x0"))?;
                        account.storage.entry(slot).or_insert(val);
                    }
                }
            }
        }
        Ok(())
    }

    /// EIP-7702 delegation targets referenced by a block's transactions plus any prestate account
    /// already holding a `0xef0100||target` designator.
    pub fn delegation_targets(txs: &Value, pre_state: &PreState) -> Vec<Address> {
        let mut targets: BTreeSet<Address> = BTreeSet::new();
        if let Some(arr) = txs.get("transactions").and_then(Value::as_array) {
            for tx in arr {
                if let Some(auths) = tx.get("authorizationList").and_then(Value::as_array) {
                    for auth in auths {
                        if let Some(a) = auth.get("address").and_then(Value::as_str) &&
                            let Ok(addr) = a.parse::<Address>() &&
                            addr != Address::ZERO
                        {
                            targets.insert(addr);
                        }
                    }
                }
            }
        }
        for acc in pre_state.values() {
            if let Some(code) = acc.code.as_ref() {
                let bytes = code.as_ref();
                if bytes.len() == 23 && bytes[..3] == EIP7702_MAGIC {
                    targets.insert(Address::from_slice(&bytes[3..23]));
                }
            }
        }
        targets.into_iter().collect()
    }

    fn opt_u256(v: Option<&Value>) -> Result<Option<U256>, String> {
        v.and_then(Value::as_str).map(parse_u256).transpose()
    }

    fn parse_u256(s: &str) -> Result<U256, String> {
        U256::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| e.to_string())
    }
}

// ===========================================================================================
// Fetch + cache fixtures
// ===========================================================================================

/// Fixtures for one replay range, cached on disk.
struct Fixtures {
    prestate: fixtures::PreState,
    /// Raw block RLP for each real block, in order `start..start+count`.
    raw_blocks: Vec<Vec<u8>>,
}

fn cache_dir(start: u64, count: u64) -> PathBuf {
    let base = std::env::var("PIPE_TEST_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("gravity_mainnet_fixtures"));
    base.join(format!("{start}_{count}"))
}

/// Load fixtures from the cache, or fetch them from `rpc_url` and cache them.
fn load_or_fetch(rpc_url: &str, start: u64, count: u64) -> Result<Fixtures, Error> {
    let dir = cache_dir(start, count);
    let prestate_path = dir.join("prestate.json");
    let blocks_path = dir.join("blocks.json");

    if prestate_path.is_file() && blocks_path.is_file() {
        info!("loading cached fixtures from {}", dir.display());
        let prestate: fixtures::PreState =
            serde_json::from_reader(std::fs::File::open(&prestate_path)?)?;
        let raw_hex: Vec<String> = serde_json::from_reader(std::fs::File::open(&blocks_path)?)?;
        let raw_blocks = raw_hex
            .iter()
            .map(|h| hex::decode(h.trim_start_matches("0x")))
            .collect::<Result<_, _>>()?;
        return Ok(Fixtures { prestate, raw_blocks });
    }

    info!("fetching {count} blocks from {start} via {rpc_url}");
    let rpc = rpc::Rpc::new(rpc_url.to_string());
    let chain_id = rpc.chain_id()?;
    assert_eq!(chain_id, 1, "this harness targets Ethereum mainnet (chainId 1)");

    let mut prestate = fixtures::PreState::new();
    let mut raw_blocks = Vec::with_capacity(count as usize);
    let mut deleg_cache = BTreeMap::new();

    for number in start..start + count {
        let raw = rpc.raw_block(number)?;
        rpc.accumulate_prestate(number, &mut prestate)?;
        let block_with_txs = rpc.block_with_txs(number)?;
        let added =
            rpc.supplement_delegations(number, &block_with_txs, &mut prestate, &mut deleg_cache)?;
        info!(
            "  block {number}: {} bytes, prestate {} accounts (+{added} 7702 targets)",
            raw.len(),
            prestate.len()
        );
        raw_blocks.push(raw);
    }

    // Cache to disk.
    std::fs::create_dir_all(&dir)?;
    serde_json::to_writer(std::fs::File::create(&prestate_path)?, &prestate)?;
    let raw_hex: Vec<String> = raw_blocks.iter().map(|b| format!("0x{}", hex::encode(b))).collect();
    serde_json::to_writer(std::fs::File::create(&blocks_path)?, &raw_hex)?;
    info!("cached fixtures to {}", dir.display());

    Ok(Fixtures { prestate, raw_blocks })
}

/// Headroom (wei) added to every transaction sender's genesis balance: 1e24 ≈ 1,000,000 ETH —
/// astronomically larger than any plausible per-run execution divergence, while still trivial next
/// to `U256::MAX`, so it can never overflow or otherwise perturb the trie machinery under test.
const SENDER_HEADROOM_WEI: u128 = 1_000_000_000_000_000_000_000_000;

/// Add [`SENDER_HEADROOM_WEI`] to the genesis balance of every account that sends a transaction in
/// the range.
///
/// Partial-state replay diverges slightly from mainnet (renumbered `NUMBER`, absent
/// `BLOCKHASH`/EIP-2935 ancestors — see the module docs), which can leave a sender a few gwei short
/// and abort an otherwise-valid tx with `LackOfFundForMaxFee`. The differential oracle compares the
/// nested trie against reth's standard MPT over the *same produced state*, so inflating sender
/// balances is invisible to it; only EOA balances move, and all trie-relevant churn is untouched.
/// Senders are recovered from the raw block bodies (recovery is independent of block number, so we
/// do it on the un-renumbered blocks here).
fn grant_sender_headroom(fx: &mut Fixtures) {
    let mut senders: std::collections::BTreeSet<Address> = std::collections::BTreeSet::new();
    for raw in &fx.raw_blocks {
        let block = Block::decode(&mut raw.as_slice()).expect("decode raw block");
        let recovered = RecoveredBlock::try_recover(block).expect("recover senders");
        senders.extend(recovered.senders().iter().copied());
    }
    let headroom = U256::from(SENDER_HEADROOM_WEI);
    let mut granted = 0usize;
    for sender in &senders {
        // A tx sender is always touched in its block, so `prestateTracer` always captured it.
        if let Some(account) = fx.prestate.get_mut(sender) {
            account.balance = account.balance.saturating_add(headroom);
            granted += 1;
        }
    }
    info!("granted balance headroom to {granted}/{} transaction senders", senders.len());
}

// ===========================================================================================
// Synthetic genesis from prestate
// ===========================================================================================

/// Real mainnet activation timestamps of the time-based forks. Used in the synthetic genesis so a
/// replayed block — which keeps its real timestamp even though its number is rewritten — executes
/// under exactly the forks that were live for it on mainnet (notably, pre-Cancun blocks keep
/// classic wipe-the-account `SELFDESTRUCT`, which EIP-6780 later restricted).
const SHANGHAI_TIME: u64 = 1_681_338_455; // 2023-04-12
const CANCUN_TIME: u64 = 1_710_338_135; // 2024-03-13 (Dencun)
const PRAGUE_TIME: u64 = 1_746_612_311; // 2025-05-07 (Pectra)

/// Build a geth-format genesis JSON whose `alloc` is the union prestate. Block-based forks are all
/// active from genesis; the time-based forks (Shanghai/Cancun/Prague) activate at their real
/// mainnet timestamps so each replayed block runs under its true fork set (see the consts above).
fn build_genesis_json(prestate: &fixtures::PreState) -> String {
    let mut alloc = serde_json::Map::new();
    for (addr, acc) in prestate {
        let mut obj = serde_json::Map::new();
        obj.insert("balance".into(), serde_json::json!(format!("0x{:x}", acc.balance)));
        obj.insert("nonce".into(), serde_json::json!(format!("0x{:x}", acc.nonce)));
        if let Some(code) = &acc.code &&
            !code.is_empty()
        {
            obj.insert("code".into(), serde_json::json!(code.to_string()));
        }
        if !acc.storage.is_empty() {
            let mut st = serde_json::Map::new();
            for (k, v) in &acc.storage {
                st.insert(format!("0x{k:064x}"), serde_json::json!(format!("0x{v:064x}")));
            }
            obj.insert("storage".into(), serde_json::Value::Object(st));
        }
        alloc.insert(format!("{addr:?}"), serde_json::Value::Object(obj));
    }

    let genesis = serde_json::json!({
        "config": {
            "chainId": 1,
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
            "terminalTotalDifficultyPassed": true,
            "shanghaiTime": SHANGHAI_TIME,
            "cancunTime": CANCUN_TIME,
            "pragueTime": PRAGUE_TIME,
            "blobSchedule": {
                "cancun": { "target": 3, "max": 6, "baseFeeUpdateFraction": 3338477 },
                "prague": { "target": 6, "max": 9, "baseFeeUpdateFraction": 5007716 }
            }
        },
        "nonce": "0x0",
        "timestamp": "0x0",
        "gasLimit": "0x1c9c380",
        "difficulty": "0x0",
        "coinbase": "0x0000000000000000000000000000000000000000",
        "alloc": serde_json::Value::Object(alloc),
    });
    serde_json::to_string(&genesis).expect("serialize genesis")
}

// ===========================================================================================
// Replay + oracle
// ===========================================================================================

async fn run_replay(
    builder: WithLaunchContext<NodeBuilder<Arc<DatabaseEnv>, ChainSpec>>,
    raw_blocks: Vec<Vec<u8>>,
) -> eyre::Result<()> {
    let handle = builder
        .with_types_and_provider::<EthereumNode, BlockchainProvider<_>>()
        .with_components(EthereumNode::components())
        .with_add_ons(EthereumAddOns::default())
        .launch_with_fn(|builder| {
            let launcher = EngineNodeLauncher::new(
                builder.task_executor().clone(),
                builder.config().datadir(),
                // Persist every block eagerly (don't keep a tail in memory) so the final
                // `wait_for_block_persistence(count)` always completes and the oracle reads the
                // full state from disk, regardless of range size.
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

    // The seeded chain tip is genesis (block 0) with the union prestate as its state.
    let db_provider = provider.database_provider_ro().unwrap();
    let genesis_number = db_provider.best_block_number().unwrap();
    assert_eq!(genesis_number, 0, "fresh datadir must start at genesis");
    let genesis_hash = db_provider.block_hash(0).unwrap().unwrap();
    let genesis_header = db_provider.header_by_number(0).unwrap().unwrap();
    drop(db_provider);

    // Build the nested trie from the genesis (prestate) hashed state, so block 1's incremental
    // computation has the full base trie to build on.
    let tx = provider.database_provider_ro().unwrap().into_tx();
    let nested_hash = NestedStateRoot::new(&tx, None);
    let hashed_state = nested_hash.read_hashed_state(None).unwrap();
    let (genesis_root, trie_updates) = nested_hash.calculate(&hashed_state).unwrap();
    drop(tx);
    let trie_write = provider.database_provider_rw().unwrap();
    trie_write.write_trie_updatesv2(&trie_updates).unwrap();
    UnifiedStorageWriter::commit_unwind(trie_write)?;
    info!(?genesis_root, header_root=?genesis_header.state_root, "seeded nested trie for genesis");

    let count = raw_blocks.len() as u64;
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

    // Push the real blocks, renumbered to 1..=count (genesis is 0). The pipe overwrites
    // parent_hash/state_root, so the only field that matters from our renumbering is `number`.
    for (i, raw) in raw_blocks.iter().enumerate() {
        let mut block = Block::decode(&mut raw.as_slice()).expect("decode raw block");
        block.header.number = i as u64 + 1;
        let recovered = RecoveredBlock::try_recover(block).expect("recover senders");
        pipeline_api.push_history_block(recovered).expect("push history block");
    }

    // Pull each execution result and commit it (None = no defensive hash check). Committing
    // unblocks the pipe to make the block canonical and proceed to the next one. History blocks
    // execute every transaction (no tx filtering) and `execute_history_block` panics on any
    // execution error, so reaching here for all `count` blocks already implies every block
    // executed cleanly; the only thing left to assert is that they arrive in order.
    for expected in 1..=count {
        let result = pipeline_api.pull_executed_block_hash().await.expect("pull executed block");
        assert_eq!(result.block_number, expected, "blocks must execute in order");
        pipeline_api
            .commit_executed_block_hash(result.block_id, None)
            .expect("commit executed block");
    }

    // Drain persistence so the on-disk MDBX state reflects every executed block.
    pipeline_api.wait_for_block_persistence(count).await.expect("wait for persistence");

    // ---- Oracle: cross-implementation differential ----------------------------------------
    // R_incr: the nested-trie root the pipeline produced for the tip.
    let r_incr = provider
        .database_provider_ro()
        .unwrap()
        .header_by_number(count)
        .unwrap()
        .expect("tip header persisted")
        .state_root;

    // R_std: reth's independent standard-MPT root over the same post-persist hashed state.
    let tx = provider.database_provider_ro().unwrap().into_tx();
    let full_state = NestedStateRoot::new(&tx, None).read_hashed_state(None).unwrap();
    let r_std = standard_mpt_root(&full_state);
    drop(tx);

    info!(?r_incr, ?r_std, count, "differential oracle");
    assert_eq!(
        r_incr, r_std,
        "nested-trie tip root disagrees with reth standard-MPT root over the same state \
         (nested trie / cache / persist bug)"
    );

    info!("mainnet replay OK: {count} blocks, tip state_root {r_incr:?}");
    Ok(())
}

/// Compute the canonical Ethereum state root from a full [`HashedPostState`] using reth's
/// standard MPT implementation — a codebase entirely independent of the nested trie under test.
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
fn mainnet_replay() {
    let rpc_url = match std::env::var("PIPE_TEST_RPC") {
        Ok(u) => u,
        Err(_) => {
            eprintln!(
                "SKIP mainnet_replay: set PIPE_TEST_RPC=<archive-rpc> \
                 PIPE_TEST_START_BLOCK=<n> [PIPE_TEST_COUNT=<n>] and run with --features pipe_test"
            );
            return;
        }
    };
    let start: u64 = std::env::var("PIPE_TEST_START_BLOCK")
        .expect("PIPE_TEST_START_BLOCK required")
        .parse()
        .expect("PIPE_TEST_START_BLOCK must be a number");
    let count: u64 =
        std::env::var("PIPE_TEST_COUNT").ok().and_then(|s| s.parse().ok()).unwrap_or(1);
    assert!(count >= 1, "PIPE_TEST_COUNT must be >= 1");

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

    let mut fx = load_or_fetch(&rpc_url, start, count).expect("load or fetch fixtures");

    // Give transaction senders balance headroom so partial-state replay divergence can't starve
    // one into a fatal funds error mid-run (see the module-level "Execution divergence" note).
    grant_sender_headroom(&mut fx);

    // Write the synthetic genesis to a temp file and point the node at it.
    let genesis_json = build_genesis_json(&fx.prestate);
    let work =
        std::env::temp_dir().join(format!("gravity_replay_{start}_{count}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).unwrap();
    let genesis_file = work.join("genesis.json");
    std::fs::write(&genesis_file, genesis_json).unwrap();
    let datadir = work.join("datadir");

    let raw_blocks = fx.raw_blocks;
    let runner = CliRunner::try_default_runtime().unwrap();
    let mut args: Vec<String> = vec![
        "reth".into(),
        "--chain".into(),
        genesis_file.to_str().unwrap().into(),
        "--with-unused-ports".into(),
        "--dev".into(),
        "--datadir".into(),
        datadir.to_str().unwrap().into(),
    ];
    // Optional cache tuning, forwarded to `init_gravity_config` via the node CLI so it wins the
    // `OnceLock` race (the node initializes the global gravity config before the cache daemon
    // reads `cache_capacity`). Set `PIPE_TEST_CACHE_CAPACITY` small (min 1000) to exercise the
    // eviction path; set `PIPE_TEST_PERSIST_GAP` large to let the cache shadow the lagging DB
    // over a wider window.
    if let Ok(c) = std::env::var("PIPE_TEST_CACHE_CAPACITY") {
        args.push("--gravity.cache.capacity".into());
        args.push(c);
    }
    if let Ok(g) = std::env::var("PIPE_TEST_PERSIST_GAP") {
        args.push("--gravity.cache.max-persist-gap".into());
        args.push(g);
    }
    let command: NodeCommand<EthereumChainSpecParser> =
        NodeCommand::try_parse_args_from(args).unwrap();

    runner
        .run_command_until_exit(|ctx| {
            command.execute(
                ctx,
                FnLauncher::new::<EthereumChainSpecParser, _>(move |builder, _| async move {
                    run_replay(builder, raw_blocks).await
                }),
            )
        })
        .unwrap();

    let _ = std::fs::remove_dir_all(&work);
}
