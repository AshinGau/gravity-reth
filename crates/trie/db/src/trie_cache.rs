use alloy_primitives::{Address, B256, U256};
use reth_primitives_traits::{Account, Bytecode};
use reth_trie::{BranchNodeCompact, Nibbles};

#[derive(Hash, PartialEq, Eq, Clone, Debug)]
pub enum StorageCacheKey {
    StateAccount(Address),
    StateContract(B256),
    StateStorage(Address, B256),
    HashAccount(B256),
    HashStorage(B256, B256),
    TrieAccout(Nibbles),
    TrieStorage(B256, Nibbles),
}

#[derive(Clone, Debug)]
pub enum StorageCacheValue {
    StateAccount(Account),
    StateContract(Bytecode),
    StateStorage(U256),
    HashAccount(Account),
    HashStorage(U256),
    TrieAccout(BranchNodeCompact),
    TrieStorage(BranchNodeCompact),
}

pub trait TrieCacheReader: Clone + Send + Sync {
    fn hashed_account(&self, hash_address: B256) -> Option<Account>;

    fn hashed_storage(&self, hash_address: B256, hash_slot: B256) -> Option<U256>;

    fn trie_account(&self, nibbles: Nibbles) -> Option<BranchNodeCompact>;

    fn trie_storage(&self, hash_address: B256, nibbles: Nibbles) -> Option<BranchNodeCompact>;

    fn cache(&self, key: StorageCacheKey, value: StorageCacheValue);
}

#[derive(Clone, Debug)]
pub struct NoopTrieCacheReader {}
impl TrieCacheReader for NoopTrieCacheReader {
    fn hashed_account(&self, _: B256) -> Option<Account> {
        None
    }

    fn hashed_storage(&self, _: B256, _: B256) -> Option<U256> {
        None
    }

    fn trie_account(&self, _: Nibbles) -> Option<BranchNodeCompact> {
        None
    }

    fn trie_storage(&self, _: B256, _: Nibbles) -> Option<BranchNodeCompact> {
        None
    }

    fn cache(&self, _: StorageCacheKey, _: StorageCacheValue) {}
}
