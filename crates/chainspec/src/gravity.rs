//! Gravity-specific chain identity and hardforks

use reth_ethereum_forks::hardfork;

/// Gravity mainnet chain id.
pub const GRAVITY_MAINNET_CHAIN_ID: u64 = 127001;

/// Gravity testnet chain id.
pub const GRAVITY_TESTNET_CHAIN_ID: u64 = 7771113;

/// Returns `true` if `chain_id` identifies a Gravity network (mainnet or testnet).
pub const fn is_gravity_chain_id(chain_id: u64) -> bool {
    matches!(chain_id, GRAVITY_MAINNET_CHAIN_ID | GRAVITY_TESTNET_CHAIN_ID)
}

hardfork!(
    /// Gravity hardforks.
    GravityHardfork {
        /// Alpha hardfork: upgrade Staking/StakePool contracts and disable PoW rewards
        Alpha,
        /// Beta hardfork: upgrade StakePool contracts with correct FACTORY immutable
        Beta,
        /// Gamma hardfork: audit fixes, precompile changes, 12 contract bytecode upgrades
        Gamma,
        /// Delta hardfork: activate Governance contract by setting Ownable._owner
        Delta,
    }
);
