use alloy_primitives::B256;
use reth_db::{
    cursor::{DbCursorRW, DbDupCursorRW},
    tables,
};
use reth_db_api::{
    cursor::{DbCursorRO, DbDupCursorRO},
    transaction::DbTx,
};
use reth_storage_errors::db::DatabaseError;
use reth_trie::{
    trie_cursor::{TrieCursor, TrieCursorFactory},
    updates::StorageTrieUpdates,
    BranchNodeCompact, Nibbles, StorageTrieEntry, StoredNibbles, StoredNibblesSubKey,
};

use crate::{NoopTrieCacheReader, StorageCacheKey, StorageCacheValue, TrieCacheReader};

/// Wrapper struct for database transaction implementing trie cursor factory trait.
#[derive(Debug)]
pub struct DatabaseTrieCursorFactory<'a, TX, CR = NoopTrieCacheReader>(&'a TX, Option<CR>);

impl<TX, CR: Clone> Clone for DatabaseTrieCursorFactory<'_, TX, CR> {
    fn clone(&self) -> Self {
        Self(self.0, self.1.clone())
    }
}

impl<'a, TX> DatabaseTrieCursorFactory<'a, TX> {
    /// Create new [`DatabaseTrieCursorFactory`].
    pub const fn new(tx: &'a TX) -> Self {
        Self(tx, None)
    }
}

impl<'a, TX, CR> DatabaseTrieCursorFactory<'a, TX, CR> {
    pub const fn with_cache(tx: &'a TX, cache: Option<CR>) -> Self {
        Self(tx, cache)
    }
}

/// Implementation of the trie cursor factory for a database transaction.
impl<TX: DbTx, CR: TrieCacheReader> TrieCursorFactory for DatabaseTrieCursorFactory<'_, TX, CR> {
    type AccountTrieCursor =
        DatabaseAccountTrieCursor<<TX as DbTx>::Cursor<tables::AccountsTrie>, CR>;
    type StorageTrieCursor =
        DatabaseStorageTrieCursor<<TX as DbTx>::DupCursor<tables::StoragesTrie>, CR>;

    fn account_trie_cursor(&self) -> Result<Self::AccountTrieCursor, DatabaseError> {
        Ok(DatabaseAccountTrieCursor::with_cache(
            self.0.cursor_read::<tables::AccountsTrie>()?,
            self.1.clone(),
        ))
    }

    fn storage_trie_cursor(
        &self,
        hashed_address: B256,
    ) -> Result<Self::StorageTrieCursor, DatabaseError> {
        Ok(DatabaseStorageTrieCursor::with_cache(
            self.0.cursor_dup_read::<tables::StoragesTrie>()?,
            self.1.clone(),
            hashed_address,
        ))
    }
}

/// A cursor over the account trie.
#[derive(Debug)]
pub struct DatabaseAccountTrieCursor<C, R = NoopTrieCacheReader> {
    cursor: C,
    cache: Option<R>,
    current_key: Option<(Nibbles, bool)>,
}

impl<C> DatabaseAccountTrieCursor<C> {
    /// Create a new account trie cursor.
    pub const fn new(cursor: C) -> Self {
        Self { cursor, cache: None, current_key: None }
    }
}

impl<C, R> DatabaseAccountTrieCursor<C, R> {
    /// Create a new account trie cursor.
    pub const fn with_cache(cursor: C, cache: Option<R>) -> Self {
        Self { cursor, cache, current_key: None }
    }
}

impl<C, R> TrieCursor for DatabaseAccountTrieCursor<C, R>
where
    C: DbCursorRO<tables::AccountsTrie> + Send + Sync,
    R: TrieCacheReader,
{
    /// Seeks an exact match for the provided key in the account trie.
    fn seek_exact(
        &mut self,
        key: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        if let Some(cache) = &self.cache {
            if let Some(node) = cache.trie_account(key.clone()) {
                self.current_key = Some((key.clone(), true));
                return Ok(Some((key, node)));
            }
        }
        self.current_key = None;
        if let Some((key, value)) =
            self.cursor.seek_exact(StoredNibbles(key))?.map(|value| (value.0 .0, value.1))
        {
            if let Some(cache) = &self.cache {
                cache.cache(
                    StorageCacheKey::TrieAccout(key.clone()),
                    StorageCacheValue::TrieAccout(value.clone()),
                );
            }
            Ok(Some((key, value)))
        } else {
            Ok(None)
        }
    }

    /// Seeks a key in the account trie that matches or is greater than the provided key.
    fn seek(
        &mut self,
        key: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        if let Some(cache) = &self.cache {
            if let Some(node) = cache.trie_account(key.clone()) {
                self.current_key = Some((key.clone(), false));
                return Ok(Some((key, node)));
            }
        }
        self.current_key = None;
        if let Some((key, value)) =
            self.cursor.seek(StoredNibbles(key))?.map(|value| (value.0 .0, value.1))
        {
            if let Some(cache) = &self.cache {
                cache.cache(
                    StorageCacheKey::TrieAccout(key.clone()),
                    StorageCacheValue::TrieAccout(value.clone()),
                );
            }
            Ok(Some((key, value)))
        } else {
            Ok(None)
        }
    }

    /// Move the cursor to the next entry and return it.
    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        if let Some((key, exact)) = self.current_key.take() {
            if exact {
                let _ = self.cursor.seek_exact(StoredNibbles(key))?;
            } else {
                let _ = self.cursor.seek(StoredNibbles(key))?;
            }
        }
        Ok(self.cursor.next()?.map(|value| (value.0 .0, value.1)))
    }

    /// Retrieves the current key in the cursor.
    fn current(&mut self) -> Result<Option<Nibbles>, DatabaseError> {
        if let Some((key, _)) = &self.current_key {
            return Ok(Some(key.clone()));
        }
        Ok(self.cursor.current()?.map(|(k, _)| k.0))
    }
}

/// A cursor over the storage tries stored in the database.
#[derive(Debug)]
pub struct DatabaseStorageTrieCursor<C, R = NoopTrieCacheReader> {
    /// The underlying cursor.
    pub cursor: C,
    cache: Option<R>,
    /// Hashed address used for cursor positioning.
    hashed_address: B256,
    current_subkey: Option<Nibbles>,
}

impl<C> DatabaseStorageTrieCursor<C> {
    /// Create a new storage trie cursor.
    pub const fn new(cursor: C, hashed_address: B256) -> Self {
        Self { cursor, cache: None, hashed_address, current_subkey: None }
    }
}

impl<C, R> DatabaseStorageTrieCursor<C, R> {
    /// Create a new storage trie cursor.
    pub const fn with_cache(cursor: C, cache: Option<R>, hashed_address: B256) -> Self {
        Self { cursor, cache, hashed_address, current_subkey: None }
    }
}

impl<C, R> DatabaseStorageTrieCursor<C, R>
where
    C: DbCursorRO<tables::StoragesTrie>
        + DbCursorRW<tables::StoragesTrie>
        + DbDupCursorRO<tables::StoragesTrie>
        + DbDupCursorRW<tables::StoragesTrie>,
{
    /// Writes storage updates
    pub fn write_storage_trie_updates(
        &mut self,
        updates: &StorageTrieUpdates,
    ) -> Result<usize, DatabaseError> {
        // The storage trie for this account has to be deleted.
        if updates.is_deleted() && self.cursor.seek_exact(self.hashed_address)?.is_some() {
            self.cursor.delete_current_duplicates()?;
        }

        // Merge updated and removed nodes. Updated nodes must take precedence.
        let mut storage_updates = updates
            .removed_nodes_ref()
            .iter()
            .filter_map(|n| (!updates.storage_nodes_ref().contains_key(n)).then_some((n, None)))
            .collect::<Vec<_>>();
        storage_updates.extend(
            updates.storage_nodes_ref().iter().map(|(nibbles, node)| (nibbles, Some(node))),
        );

        // Sort trie node updates.
        storage_updates.sort_unstable_by(|a, b| a.0.cmp(b.0));

        let mut num_entries = 0;
        for (nibbles, maybe_updated) in storage_updates.into_iter().filter(|(n, _)| !n.is_empty()) {
            num_entries += 1;
            let nibbles = StoredNibblesSubKey(nibbles.clone());
            // Delete the old entry if it exists.
            if self
                .cursor
                .seek_by_key_subkey(self.hashed_address, nibbles.clone())?
                .filter(|e| e.nibbles == nibbles)
                .is_some()
            {
                self.cursor.delete_current()?;
            }

            // There is an updated version of this node, insert new entry.
            if let Some(node) = maybe_updated {
                self.cursor.upsert(
                    self.hashed_address,
                    &StorageTrieEntry { nibbles, node: node.clone() },
                )?;
            }
        }

        Ok(num_entries)
    }
}

impl<C, R> TrieCursor for DatabaseStorageTrieCursor<C, R>
where
    C: DbCursorRO<tables::StoragesTrie> + DbDupCursorRO<tables::StoragesTrie> + Send + Sync,
    R: TrieCacheReader,
{
    /// Seeks an exact match for the given key in the storage trie.
    fn seek_exact(
        &mut self,
        key: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        if let Some(cache) = &self.cache {
            if let Some(node) = cache.trie_storage(self.hashed_address, key.clone()) {
                self.current_subkey = Some(key.clone());
                return Ok(Some((key, node)));
            }
        }
        self.current_subkey = None;
        if let Some((subkey, value)) = self
            .cursor
            .seek_by_key_subkey(self.hashed_address, StoredNibblesSubKey(key.clone()))?
            .filter(|e| e.nibbles == StoredNibblesSubKey(key))
            .map(|value| (value.nibbles.0, value.node))
        {
            if let Some(cache) = &self.cache {
                cache.cache(
                    StorageCacheKey::TrieStorage(self.hashed_address, subkey.clone()),
                    StorageCacheValue::TrieStorage(value.clone()),
                );
            }
            Ok(Some((subkey, value)))
        } else {
            Ok(None)
        }
    }

    /// Seeks the given key in the storage trie.
    fn seek(
        &mut self,
        key: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        if let Some(cache) = &self.cache {
            if let Some(node) = cache.trie_storage(self.hashed_address, key.clone()) {
                self.current_subkey = Some(key.clone());
                return Ok(Some((key, node)));
            }
        }
        self.current_subkey = None;
        if let Some((subkey, value)) = self
            .cursor
            .seek_by_key_subkey(self.hashed_address, StoredNibblesSubKey(key))?
            .map(|value| (value.nibbles.0, value.node))
        {
            if let Some(cache) = &self.cache {
                cache.cache(
                    StorageCacheKey::TrieStorage(self.hashed_address, subkey.clone()),
                    StorageCacheValue::TrieStorage(value.clone()),
                );
            }
            Ok(Some((subkey, value)))
        } else {
            Ok(None)
        }
    }

    /// Move the cursor to the next entry and return it.
    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        if let Some(subkey) = self.current_subkey.take() {
            let _ =
                self.cursor.seek_by_key_subkey(self.hashed_address, StoredNibblesSubKey(subkey))?;
        }
        Ok(self.cursor.next_dup()?.map(|(_, v)| (v.nibbles.0, v.node)))
    }

    /// Retrieves the current value in the storage trie cursor.
    fn current(&mut self) -> Result<Option<Nibbles>, DatabaseError> {
        if let Some(subkey) = &self.current_subkey {
            return Ok(Some(subkey.clone()));
        }
        Ok(self.cursor.current()?.map(|(_, v)| v.nibbles.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::hex_literal::hex;
    use reth_db_api::{cursor::DbCursorRW, transaction::DbTxMut};
    use reth_provider::test_utils::create_test_provider_factory;

    #[test]
    fn test_account_trie_order() {
        let factory = create_test_provider_factory();
        let provider = factory.provider_rw().unwrap();
        let mut cursor = provider.tx_ref().cursor_write::<tables::AccountsTrie>().unwrap();

        let data = vec![
            hex!("0303040e").to_vec(),
            hex!("030305").to_vec(),
            hex!("03030500").to_vec(),
            hex!("0303050a").to_vec(),
        ];

        for key in data.clone() {
            cursor
                .upsert(
                    key.into(),
                    &BranchNodeCompact::new(
                        0b0000_0010_0000_0001,
                        0b0000_0010_0000_0001,
                        0,
                        Vec::default(),
                        None,
                    ),
                )
                .unwrap();
        }

        let db_data = cursor.walk_range(..).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(db_data[0].0 .0.to_vec(), data[0]);
        assert_eq!(db_data[1].0 .0.to_vec(), data[1]);
        assert_eq!(db_data[2].0 .0.to_vec(), data[2]);
        assert_eq!(db_data[3].0 .0.to_vec(), data[3]);

        assert_eq!(
            cursor.seek(hex!("0303040f").to_vec().into()).unwrap().map(|(k, _)| k.0.to_vec()),
            Some(data[1].clone())
        );
    }

    // tests that upsert and seek match on the storage trie cursor
    #[test]
    fn test_storage_cursor_abstraction() {
        let factory = create_test_provider_factory();
        let provider = factory.provider_rw().unwrap();
        let mut cursor = provider.tx_ref().cursor_dup_write::<tables::StoragesTrie>().unwrap();

        let hashed_address = B256::random();
        let key = StoredNibblesSubKey::from(vec![0x2, 0x3]);
        let value = BranchNodeCompact::new(1, 1, 1, vec![B256::random()], None);

        cursor
            .upsert(hashed_address, &StorageTrieEntry { nibbles: key.clone(), node: value.clone() })
            .unwrap();

        let mut cursor = DatabaseStorageTrieCursor::new(cursor, hashed_address);
        assert_eq!(cursor.seek(key.into()).unwrap().unwrap().1, value);
    }
}
