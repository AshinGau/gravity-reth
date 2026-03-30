//! An integration of `reth-trie` with `reth-db`.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

<<<<<<< HEAD
mod commitment;
=======
mod changesets;
pub use changesets::*;
>>>>>>> v1.11.3
mod hashed_cursor;
mod prefix_set;
mod proof;
mod state;
mod storage;
mod trie_cursor;
mod witness;

pub use commitment::{MerklePatriciaTrie, StateCommitment};
pub use hashed_cursor::{
    DatabaseHashedAccountCursor, DatabaseHashedCursorFactory, DatabaseHashedStorageCursor,
};
<<<<<<< HEAD
pub use prefix_set::PrefixSetLoader;
pub use proof::{DatabaseProof, DatabaseStorageProof};
pub use state::{DatabaseHashedPostState, DatabaseStateRoot};
pub use storage::{DatabaseHashedStorage, DatabaseStorageRoot};
=======
pub use prefix_set::load_prefix_sets_with_provider;
pub use proof::{DatabaseProof, DatabaseStorageProof};
pub use state::{from_reverts_auto, DatabaseHashedPostState, DatabaseStateRoot};
pub use storage::{hashed_storage_from_reverts_with_provider, DatabaseStorageRoot};
>>>>>>> v1.11.3
pub use trie_cursor::{
    DatabaseAccountTrieCursor, DatabaseStorageTrieCursor, DatabaseTrieCursorFactory,
};
pub use witness::DatabaseTrieWitness;
