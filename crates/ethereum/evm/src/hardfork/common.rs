//! Common types and traits for Gravity hardfork state changes.
//!
//! Each hardfork module (alpha, beta, gamma, ...) defines a set of system
//! contract bytecode upgrades. This module provides the [`HardforkUpgrades`]
//! trait that standardizes how upgrade tables are exposed, plus a single,
//! executor-agnostic entry point ([`apply_gravity_hardfork_upgrades`]) that
//! both the parallel (grevm) and the reth-native serial (`disable-grevm`)
//! execution paths share, so the two backends can never diverge on hardfork
//! application (gravity-audit#711, F-A2-3).

use alloc::format;
use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use reth_chainspec::{ChainSpec, EthChainSpec, GravityHardfork};
use reth_ethereum_primitives::EthPrimitives;
use reth_evm::{execute::BlockExecutionError, parallel_execute::ParallelExecutor};
use revm::{
    bytecode::Bytecode,
    state::{Account, AccountStatus, EvmState, EvmStorageSlot},
    DatabaseRef,
};

/// A single bytecode replacement: target address → new runtime bytecode.
pub type BytecodeUpgrade = (Address, &'static [u8]);

/// A single storage patch: target address, slot, value.
pub type StoragePatch = (Address, B256, U256);

/// A batch storage patch: same (slot, value) applied to multiple addresses.
/// Used for Gamma's `ReentrancyGuard` initialization across all `StakePool` instances.
pub type BatchStoragePatch = (&'static [Address], B256, U256);

/// Trait implemented by each hardfork module to describe its upgrades.
///
/// This enables generic test helpers that work across any hardfork:
/// ```ignore
/// fn verify_hardfork_applied<H: HardforkUpgrades>(provider: &P, block: u64) { ... }
/// ```
pub trait HardforkUpgrades {
    /// The human-readable name of this hardfork (e.g. "Gamma").
    fn name(&self) -> &'static str;

    /// System contract bytecode upgrades: `(address, new_bytecode)` pairs.
    fn system_upgrades(&self) -> &'static [BytecodeUpgrade];

    /// Additional non-system contract bytecode upgrades (e.g. `StakePool` instances).
    /// Returns `(address, new_bytecode)` pairs.
    fn extra_upgrades(&self) -> &'static [BytecodeUpgrade] {
        &[]
    }

    /// Storage patches to apply alongside bytecode replacements
    /// (e.g. `ReentrancyGuard` initialization, Governance owner).
    fn storage_patches(&self) -> &'static [StoragePatch] {
        &[]
    }

    /// Batch storage patches: apply the same (slot, value) to multiple addresses.
    /// Used for Gamma's `ReentrancyGuard` initialization across all `StakePool` instances.
    fn batch_storage_patches(&self) -> &'static [BatchStoragePatch] {
        &[]
    }
}

/// Build the [`EvmState`] diff for a single hardfork's upgrades.
///
/// Reads the *current* account info and storage values through `db` (any
/// [`DatabaseRef`]) so existing balances/nonces are preserved and only the
/// bytecode and patched slots change. This is pure — it commits nothing — so
/// the same diff can be applied through any executor's `apply_state_change`.
pub fn hardfork_upgrade_diff<H, DB>(hardfork: &H, db: &DB) -> Result<EvmState, BlockExecutionError>
where
    H: HardforkUpgrades,
    DB: DatabaseRef,
{
    let mut changes = EvmState::default();

    // 1. Bytecode upgrades (system + extra): only patch accounts that already exist, preserving
    //    their balance and nonce.
    for (addr, bytecode_bytes) in
        hardfork.system_upgrades().iter().chain(hardfork.extra_upgrades().iter())
    {
        let Some(info) = db.basic_ref(*addr).map_err(|e| {
            BlockExecutionError::msg(format!("hardfork upgrade read {addr}: {e:?}"))
        })?
        else {
            continue;
        };

        let mut new_info = info;
        new_info.code_hash = keccak256(bytecode_bytes);
        new_info.code = Some(Bytecode::new_raw(Bytes::from_static(bytecode_bytes)));
        changes.insert(
            *addr,
            Account {
                info: new_info,
                storage: Default::default(),
                status: AccountStatus::Touched,
                transaction_id: 0,
            },
        );
    }

    // 2. Storage patches.
    for (addr, slot, value) in hardfork.storage_patches() {
        patch_storage(&mut changes, db, *addr, U256::from_be_bytes(slot.0), *value)?;
    }

    // 3. Batch storage patches (same slot+value across many addresses).
    for (addresses, slot, value) in hardfork.batch_storage_patches() {
        let slot_key = U256::from_be_bytes(slot.0);
        for addr in *addresses {
            patch_storage(&mut changes, db, *addr, slot_key, *value)?;
        }
    }

    Ok(changes)
}

/// Insert a `(slot → value)` storage patch into `changes` for `addr`, reading
/// the original slot value (and the account info, if not already staged) from
/// `db` so the resulting transition carries a correct changeset.
fn patch_storage<DB: DatabaseRef>(
    changes: &mut EvmState,
    db: &DB,
    addr: Address,
    slot_key: U256,
    value: U256,
) -> Result<(), BlockExecutionError> {
    let original = db
        .storage_ref(addr, slot_key)
        .map_err(|e| BlockExecutionError::msg(format!("hardfork patch read {addr}: {e:?}")))?;

    if !changes.contains_key(&addr) {
        let info = db
            .basic_ref(addr)
            .map_err(|e| BlockExecutionError::msg(format!("hardfork patch read {addr}: {e:?}")))?
            .unwrap_or_default();
        changes.insert(
            addr,
            Account {
                info,
                storage: Default::default(),
                status: AccountStatus::Touched,
                transaction_id: 0,
            },
        );
    }
    if let Some(account) = changes.get_mut(&addr) {
        account.storage.insert(slot_key, EvmStorageSlot::new_changed(original, value, 0));
    }
    Ok(())
}

/// Build the merged [`EvmState`] diff for every Gravity hardfork that activates
/// at the given block, using the exact same activation gating as the executor:
/// Alpha is active for every block past its timestamp; Beta/Gamma/Delta fire
/// only on their transition block.
pub fn gravity_hardfork_state_for_block<DB>(
    chain_spec: &ChainSpec,
    block_number: u64,
    timestamp: u64,
    db: &DB,
) -> Result<EvmState, BlockExecutionError>
where
    DB: DatabaseRef,
{
    use super::{
        alpha::AlphaHardfork, beta::BetaHardfork, delta::DeltaHardfork, gamma::GammaHardfork,
    };

    let hf = chain_spec.gravity_hardforks();
    let mut changes = EvmState::default();

    if hf.fork(GravityHardfork::Alpha).active_at_timestamp(timestamp) {
        merge_state(&mut changes, hardfork_upgrade_diff(&AlphaHardfork, db)?);
    }
    if hf.fork(GravityHardfork::Beta).transitions_at_block(block_number) {
        merge_state(&mut changes, hardfork_upgrade_diff(&BetaHardfork, db)?);
    }
    if hf.fork(GravityHardfork::Gamma).transitions_at_block(block_number) {
        merge_state(&mut changes, hardfork_upgrade_diff(&GammaHardfork, db)?);
    }
    if hf.fork(GravityHardfork::Delta).transitions_at_block(block_number) {
        merge_state(&mut changes, hardfork_upgrade_diff(&DeltaHardfork, db)?);
    }

    Ok(changes)
}

/// Merge `from` into `into`: later forks override account info and union their
/// storage slots (different forks patch disjoint system contracts in practice).
fn merge_state(into: &mut EvmState, from: EvmState) {
    for (addr, account) in from {
        if let Some(existing) = into.get_mut(&addr) {
            existing.info = account.info;
            existing.status |= account.status;
            existing.storage.extend(account.storage);
        } else {
            into.insert(addr, account);
        }
    }
}

/// Apply every Gravity hardfork upgrade active at `block_number` to `executor`
/// through the shared [`ParallelExecutor::apply_state_change`] channel.
///
/// This is the **single source of truth** for hardfork application: both the
/// parallel (grevm) and the reth-native serial (`disable-grevm`) executors are
/// driven through here from the pipe layer, so they apply byte-identical
/// changes (gravity-audit#711, F-A2-3). A no-op when no fork activates.
pub fn apply_gravity_hardfork_upgrades<DB>(
    executor: &mut dyn ParallelExecutor<Primitives = EthPrimitives, Error = BlockExecutionError>,
    chain_spec: &ChainSpec,
    block_number: u64,
    timestamp: u64,
    db: &DB,
) -> Result<(), BlockExecutionError>
where
    DB: DatabaseRef,
{
    let changes = gravity_hardfork_state_for_block(chain_spec, block_number, timestamp, db)?;
    if !changes.is_empty() {
        executor.apply_state_change(changes)?;
    }
    Ok(())
}
