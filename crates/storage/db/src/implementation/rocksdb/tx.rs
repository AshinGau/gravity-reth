//! Transaction implementation for RocksDB.

use crate::{
    implementation::rocksdb::{cursor, get_cf_handle, read_error, to_error_info},
    DatabaseError,
};
use parking_lot::Mutex;
use reth_db_api::{
    table::{Compress, Decompress, DupSort, Encode, Table, TableImporter},
    tables,
    transaction::{DbTx, DbTxMut},
};
use reth_storage_errors::db::DatabaseErrorInfo;
use rocksdb::DB;
use std::{sync::Arc, thread};

pub(crate) use cursor::{RO, RW};

/// RocksDB transaction.
pub struct Tx<K: cursor::TransactionKind> {
    state_db: Arc<DB>,
    account_db: Arc<DB>,
    storage_db: Arc<DB>,
    state_batch: Arc<Mutex<rocksdb::WriteBatch>>,
    account_batch: Arc<Mutex<rocksdb::WriteBatch>>,
    storage_batch: Arc<Mutex<rocksdb::WriteBatch>>,
    _mode: std::marker::PhantomData<K>,
}

impl<K: cursor::TransactionKind> std::fmt::Debug for Tx<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tx").field("_mode", &self._mode).finish()
    }
}

impl<K: cursor::TransactionKind> Tx<K> {
    pub(crate) fn new(state_db: Arc<DB>, account_db: Arc<DB>, storage_db: Arc<DB>) -> Self {
        Self {
            state_db,
            account_db,
            storage_db,
            state_batch: Arc::new(Mutex::new(rocksdb::WriteBatch::default())),
            account_batch: Arc::new(Mutex::new(rocksdb::WriteBatch::default())),
            storage_batch: Arc::new(Mutex::new(rocksdb::WriteBatch::default())),
            _mode: std::marker::PhantomData,
        }
    }

    fn db_for_table<T: Table>(&self) -> &Arc<DB> {
        match T::NAME {
            tables::AccountsTrieV2::NAME => &self.account_db,
            tables::StoragesTrieV2::NAME => &self.storage_db,
            _ => &self.state_db,
        }
    }

    fn batch_for_table<T: Table>(&self) -> &Arc<Mutex<rocksdb::WriteBatch>> {
        match T::NAME {
            tables::AccountsTrieV2::NAME => &self.account_batch,
            tables::StoragesTrieV2::NAME => &self.storage_batch,
            _ => &self.state_batch,
        }
    }

    fn db_for_name(&self, name: &str) -> &Arc<DB> {
        match name {
            tables::AccountsTrieV2::NAME => &self.account_db,
            tables::StoragesTrieV2::NAME => &self.storage_db,
            _ => &self.state_db,
        }
    }

    pub fn table_entries(&self, name: &str) -> Result<usize, DatabaseError> {
        let db = self.db_for_name(name);
        let cf_handle = db.cf_handle(name).ok_or_else(|| {
            DatabaseError::Open(DatabaseErrorInfo {
                message: format!("Column family '{}' not found", name).into(),
                code: -1,
            })
        })?;

        // Use property_value_cf to get estimated number of keys
        match db.property_value_cf(cf_handle, "rocksdb.estimate-num-keys") {
            Ok(Some(value)) => {
                // Parse the string value to usize
                value.parse::<usize>().map_err(|_| {
                    DatabaseError::Read(DatabaseErrorInfo {
                        message: "Failed to parse estimated number of keys".into(),
                        code: -1,
                    })
                })
            }
            Ok(None) => Ok(0),
            Err(e) => Err(read_error(e)),
        }
    }
}

impl<K: cursor::TransactionKind> DbTx for Tx<K> {
    type Cursor<T: Table> = cursor::Cursor<K, T>;
    type DupCursor<T: DupSort> = cursor::Cursor<K, T>;

    fn get<T: Table>(&self, key: T::Key) -> Result<Option<T::Value>, DatabaseError> {
        let encoded_key = key.encode();
        self.get_by_encoded_key::<T>(&encoded_key)
    }

    fn get_by_encoded_key<T: Table>(
        &self,
        key: &<T::Key as Encode>::Encoded,
    ) -> Result<Option<T::Value>, DatabaseError> {
        let db = self.db_for_table::<T>();
        let cf_handle = get_cf_handle::<T>(db)?;

        match db.get_cf(cf_handle, key) {
            Ok(Some(value)) => {
                T::Value::decompress(&value).map(Some).map_err(|_| DatabaseError::Decode)
            }
            Ok(None) => Ok(None),
            Err(e) => Err(read_error(e)),
        }
    }

    fn commit(self) -> Result<bool, DatabaseError> {
        // Write the batch to RocksDB atomically
        let mut state_batch = self.state_batch.lock();
        let mut account_batch = self.account_batch.lock();
        let mut storage_batch = self.storage_batch.lock();
        if account_batch.len() > 0 && storage_batch.len() > 0 {
            // parallel commit
            thread::scope(|scope| -> Result<(), DatabaseError> {
                let mut handles = vec![];
                handles.push(scope.spawn(|| -> Result<(), DatabaseError> {
                    let account_batch = std::mem::take(&mut *account_batch);
                    self.account_db
                        .write(account_batch)
                        .map_err(|e| DatabaseError::Commit(to_error_info(e)))
                }));
                handles.push(scope.spawn(|| -> Result<(), DatabaseError> {
                    let storage_batch = std::mem::take(&mut *storage_batch);
                    self.storage_db
                        .write(storage_batch)
                        .map_err(|e| DatabaseError::Commit(to_error_info(e)))
                }));
                for handle in handles {
                    handle.join().unwrap()?;
                }
                Ok(())
            })?;
        } else {
            if account_batch.len() > 0 {
                let account_batch = std::mem::take(&mut *account_batch);
                self.account_db
                    .write(account_batch)
                    .map_err(|e| DatabaseError::Commit(to_error_info(e)))?;
            }
            if storage_batch.len() > 0 {
                let storage_batch = std::mem::take(&mut *storage_batch);
                self.storage_db
                    .write(storage_batch)
                    .map_err(|e| DatabaseError::Commit(to_error_info(e)))?;
            }
        }
        if state_batch.len() > 0 {
            // Make sure state batch commit last
            let state_batch = std::mem::take(&mut *state_batch);
            self.state_db
                .write(state_batch)
                .map_err(|e| DatabaseError::Commit(to_error_info(e)))?;
        }
        Ok(true)
    }

    fn abort(self) {
        // Nothing to abort for RocksDB
    }

    fn cursor_read<T: Table>(&self) -> Result<Self::Cursor<T>, DatabaseError> {
        cursor::Cursor::new(self.db_for_table::<T>().clone(), self.batch_for_table::<T>().clone())
    }

    fn cursor_dup_read<T: DupSort>(&self) -> Result<Self::DupCursor<T>, DatabaseError> {
        cursor::Cursor::new(self.db_for_table::<T>().clone(), self.batch_for_table::<T>().clone())
    }

    fn entries<T: Table>(&self) -> Result<usize, DatabaseError> {
        self.table_entries(T::NAME)
    }

    fn disable_long_read_transaction_safety(&mut self) {
        // For RocksDB, this is a no-op as it doesn't have the same long read transaction safety
        // concerns as MDBX RocksDB handles concurrent reads and writes differently
    }
}

impl DbTxMut for Tx<cursor::RW> {
    type CursorMut<T: Table> = cursor::Cursor<cursor::RW, T>;
    type DupCursorMut<T: DupSort> = cursor::Cursor<cursor::RW, T>;

    fn put<T: Table>(&self, key: T::Key, value: T::Value) -> Result<(), DatabaseError> {
        let db = self.db_for_table::<T>();
        let cf_handle = get_cf_handle::<T>(db)?;

        let encoded_key = key.encode();
        let encoded_value = value.compress();

        let mut batch = self.batch_for_table::<T>().lock();
        batch.put_cf(cf_handle, &encoded_key, &encoded_value);

        Ok(())
    }

    fn delete<T: Table>(
        &self,
        key: T::Key,
        _value: Option<T::Value>,
    ) -> Result<bool, DatabaseError> {
        let db = self.db_for_table::<T>();
        let cf_handle = get_cf_handle::<T>(db)?;

        let encoded_key = key.encode();

        let mut batch = self.batch_for_table::<T>().lock();
        batch.delete_cf(cf_handle, &encoded_key);
        Ok(true)
    }

    fn clear<T: Table>(&self) -> Result<(), DatabaseError> {
        let db = self.db_for_table::<T>();
        let cf_handle = get_cf_handle::<T>(db)?;

        // Get first and last keys using separate iterators to avoid borrowing conflicts
        let (first_key, last_key) = {
            let mut iter = db.raw_iterator_cf(cf_handle);
            iter.seek_to_first();
            if !iter.valid() {
                // No data in this column family, nothing to clear
                return Ok(());
            }
            let first_key = iter
                .key()
                .ok_or_else(|| {
                    DatabaseError::Read(DatabaseErrorInfo {
                        message: "Failed to get first key".into(),
                        code: -1,
                    })
                })?
                .to_vec();

            iter.seek_to_last();
            if !iter.valid() {
                // This shouldn't happen if we found a first key, but handle it gracefully
                return Ok(());
            }
            let last_key = iter
                .key()
                .ok_or_else(|| {
                    DatabaseError::Read(DatabaseErrorInfo {
                        message: "Failed to get last key".into(),
                        code: -1,
                    })
                })?
                .to_vec();

            (first_key, last_key)
        };

        let mut batch = self.batch_for_table::<T>().lock();
        // Delete the range [first_key, last_key] - note that delete_range_cf is [start, end)
        // so we need to handle the last key separately
        batch.delete_range_cf(cf_handle, &first_key, &last_key);

        // Delete the last key separately since delete_range_cf is [start, end)
        batch.delete_cf(cf_handle, &last_key);

        Ok(())
    }

    fn cursor_write<T: Table>(&self) -> Result<Self::CursorMut<T>, DatabaseError> {
        cursor::Cursor::new(self.db_for_table::<T>().clone(), self.batch_for_table::<T>().clone())
    }

    fn cursor_dup_write<T: DupSort>(&self) -> Result<Self::DupCursorMut<T>, DatabaseError> {
        cursor::Cursor::new(self.db_for_table::<T>().clone(), self.batch_for_table::<T>().clone())
    }
}

impl TableImporter for Tx<RW> {
    // Default implementation is sufficient for now
}
