//! Transaction implementation for RocksDB.

use crate::{
    implementation::rocksdb::{
        create_write_error, cursor, get_cf_handle, rocksdb_error_to_database_error,
    },
    DatabaseError,
};
use reth_db_api::{
    table::{Compress, Decompress, DupSort, Encode, Table, TableImporter},
    transaction::{DbTx, DbTxMut},
};
use reth_storage_errors::db::{DatabaseErrorInfo, DatabaseWriteOperation};
use rocksdb::{WriteBatch, WriteOptions, DB};
use std::{cell::UnsafeCell, sync::Arc, time::Instant};

pub use cursor::{RO, RW};

/// Single-threaded WriteBatch container.
///
/// # Safety
/// Caller MUST ensure that only one thread accesses this at a time.
/// This is guaranteed by the business layer using the database in a single thread.
pub(crate) struct SingleThreadedBatch {
    inner: UnsafeCell<WriteBatch>,
}

// SAFETY: Business layer guarantees single-threaded access
unsafe impl Send for SingleThreadedBatch {}
unsafe impl Sync for SingleThreadedBatch {}

impl SingleThreadedBatch {
    fn new() -> Self {
        Self { inner: UnsafeCell::new(WriteBatch::default()) }
    }

    /// Get mutable pointer to the WriteBatch.
    ///
    /// # Safety
    /// Caller MUST ensure single-threaded access
    #[inline]
    unsafe fn get_ptr(&self) -> *mut WriteBatch {
        self.inner.get()
    }

    /// Take ownership of the inner WriteBatch.
    ///
    /// # Safety
    /// This consumes self, so it's safe
    #[inline]
    fn into_inner(self) -> WriteBatch {
        self.inner.into_inner()
    }
}

impl std::fmt::Debug for SingleThreadedBatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SingleThreadedBatch").finish()
    }
}

/// Write mode for transactions
#[derive(Debug)]
pub(crate) enum WriteMode {
    /// Direct write mode - writes directly to DB
    Direct,
    /// Batch write mode - accumulates writes in a single WriteBatch
    /// All Tx and Cursor operations share the same WriteBatch
    /// 
    /// SAFETY: Business layer guarantees single-threaded access and Tx outlives all Cursors
    Batch(SingleThreadedBatch),
}

/// RocksDB transaction.
#[derive(Debug)]
pub struct Tx<K: cursor::TransactionKind> {
    db: Arc<DB>,
    write_mode: WriteMode,
    _mode: std::marker::PhantomData<K>,
}

impl<K: cursor::TransactionKind> Tx<K> {
    pub(crate) fn new(db: Arc<DB>) -> Self {
        Self { db, write_mode: WriteMode::Direct, _mode: std::marker::PhantomData }
    }

    pub(crate) fn new_batch(db: Arc<DB>) -> Self {
        let batch = SingleThreadedBatch::new();
        Self { db, write_mode: WriteMode::Batch(batch), _mode: std::marker::PhantomData }
    }

    /// Get a mutable pointer to the shared WriteBatch
    ///
    /// # Safety
    /// Caller must ensure single-threaded access
    #[inline]
    unsafe fn get_batch_ptr(&self) -> Option<*mut WriteBatch> {
        match &self.write_mode {
            WriteMode::Direct => None,
            WriteMode::Batch(batch) => Some(batch.get_ptr()),
        }
    }

    pub fn table_entries(&self, name: &str) -> Result<usize, DatabaseError> {
        let cf_handle = self.db.cf_handle(name).ok_or_else(|| {
            DatabaseError::Open(DatabaseErrorInfo {
                message: format!("Column family '{}' not found", name).into(),
                code: -1,
            })
        })?;

        // Use property_value_cf to get estimated number of keys
        match self.db.property_value_cf(cf_handle, "rocksdb.estimate-num-keys") {
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
            Err(e) => Err(rocksdb_error_to_database_error(e)),
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
        let cf_handle = get_cf_handle::<T>(&self.db)?;

        match self.db.get_cf(cf_handle, key) {
            Ok(Some(value)) => {
                T::Value::decompress(&value).map(Some).map_err(|_| DatabaseError::Decode)
            }
            Ok(None) => Ok(None),
            Err(e) => Err(rocksdb_error_to_database_error(e)),
        }
    }

    fn commit(self) -> Result<bool, DatabaseError> {
        // For batch mode, write the shared WriteBatch to DB
        if let WriteMode::Batch(batch) = self.write_mode {
            let start = Instant::now();
            let batch = batch.into_inner();
            let mut write_opts = WriteOptions::default();
            write_opts.set_sync(false);
            self.db.write_opt(batch, &write_opts).map_err(|e| {
                DatabaseError::Write(Box::new(reth_storage_errors::db::DatabaseWriteError {
                    info: DatabaseErrorInfo {
                        message: format!("Failed to commit batch: {}", e).into(),
                        code: -1,
                    },
                    operation: DatabaseWriteOperation::Put,
                    table_name: "<batch>",
                    key: vec![],
                }))
            })?;
            println!("commit time: {}ms", start.elapsed().as_millis());
        }
        Ok(true)
    }

    fn abort(self) {
        // Nothing to abort for RocksDB
    }

    fn cursor_read<T: Table>(&self) -> Result<Self::Cursor<T>, DatabaseError> {
        cursor::Cursor::new_read(self.db.clone())
    }

    fn cursor_dup_read<T: DupSort>(&self) -> Result<Self::DupCursor<T>, DatabaseError> {
        cursor::Cursor::new_read(self.db.clone())
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
        let cf_handle = get_cf_handle::<T>(&self.db)?;

        let encoded_key = key.encode();
        let encoded_value = value.compress();

        match &self.write_mode {
            WriteMode::Direct => {
                self.db.put_cf(cf_handle, &encoded_key, &encoded_value).map_err(|e| {
                    create_write_error::<T>(
                        e,
                        DatabaseWriteOperation::Put,
                        encoded_key.as_ref().to_vec(),
                    )
                })?;
            }
            WriteMode::Batch(_) => {
                // SAFETY: Business layer guarantees single-threaded access
                unsafe {
                    let batch = self.get_batch_ptr().unwrap();
                    (*batch).put_cf(cf_handle, &encoded_key, &encoded_value);
                }
            }
        }

        Ok(())
    }

    fn delete<T: Table>(
        &self,
        key: T::Key,
        _value: Option<T::Value>,
    ) -> Result<bool, DatabaseError> {
        let cf_handle = get_cf_handle::<T>(&self.db)?;

        let encoded_key = key.encode();

        match &self.write_mode {
            WriteMode::Direct => {
                match self.db.delete_cf(cf_handle, &encoded_key) {
                    Ok(()) => Ok(true),
                    Err(e) => Err(create_write_error::<T>(
                        e,
                        DatabaseWriteOperation::Put,
                        encoded_key.as_ref().to_vec(),
                    )),
                }
            }
            WriteMode::Batch(_) => {
                // SAFETY: Business layer guarantees single-threaded access
                unsafe {
                    let batch = self.get_batch_ptr().unwrap();
                    (*batch).delete_cf(cf_handle, &encoded_key);
                }
                Ok(true)
            }
        }
    }

    fn clear<T: Table>(&self) -> Result<(), DatabaseError> {
        let cf_handle = get_cf_handle::<T>(&self.db)?;

        // Get first and last keys using separate iterators to avoid borrowing conflicts
        let (first_key, last_key) = {
            let mut iter = self.db.raw_iterator_cf(cf_handle);
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

        match &self.write_mode {
            WriteMode::Direct => {
                // Delete the range [first_key, last_key] - note that delete_range_cf is [start,
                // end) so we need to handle the last key separately
                self.db.delete_range_cf(cf_handle, &first_key, &last_key).map_err(|e| {
                    create_write_error::<T>(e, DatabaseWriteOperation::Put, first_key.clone())
                })?;

                // Delete the last key separately since delete_range_cf is [start, end)
                self.db.delete_cf(cf_handle, &last_key).map_err(|e| {
                    create_write_error::<T>(e, DatabaseWriteOperation::Put, last_key.clone())
                })?;
            }
            WriteMode::Batch(_) => {
                // SAFETY: Business layer guarantees single-threaded access
                unsafe {
                    let batch = self.get_batch_ptr().unwrap();
                    (*batch).delete_range_cf(cf_handle, &first_key, &last_key);
                    (*batch).delete_cf(cf_handle, &last_key);
                }
            }
        }

        Ok(())
    }

    fn cursor_write<T: Table>(&self) -> Result<Self::CursorMut<T>, DatabaseError> {
        // Share the same WriteBatch pointer with cursor
        // SAFETY: Business layer guarantees single-threaded access and Tx outlives Cursor
        let batch_ptr = unsafe { self.get_batch_ptr() };
        cursor::Cursor::new_write(self.db.clone(), batch_ptr)
    }

    fn cursor_dup_write<T: DupSort>(&self) -> Result<Self::DupCursorMut<T>, DatabaseError> {
        // Share the same WriteBatch pointer with cursor
        // SAFETY: Business layer guarantees single-threaded access and Tx outlives Cursor
        let batch_ptr = unsafe { self.get_batch_ptr() };
        cursor::Cursor::new_write(self.db.clone(), batch_ptr)
    }
}

impl TableImporter for Tx<RW> {
    // Default implementation is sufficient for now
}
