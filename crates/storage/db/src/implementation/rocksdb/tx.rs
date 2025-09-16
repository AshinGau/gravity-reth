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
use rocksdb::DB;
use std::sync::Arc;

/// Transaction mode marker.
#[derive(Debug)]
pub struct RO;
/// Transaction mode marker.
#[derive(Debug)]
pub struct RW;

/// RocksDB transaction.
#[derive(Debug)]
pub struct Tx<MODE> {
    db: Arc<DB>,
    _mode: std::marker::PhantomData<MODE>,
}

impl<MODE> Tx<MODE> {
    pub(crate) fn new(db: Arc<DB>) -> Self {
        Self { db, _mode: std::marker::PhantomData }
    }
}

impl<MODE: std::fmt::Debug + Send + Sync> DbTx for Tx<MODE> {
    type Cursor<T: Table> = cursor::Cursor<T>;
    type DupCursor<T: DupSort> = cursor::DupCursor<T>;

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
        // For RocksDB, there's no explicit commit for read operations
        Ok(true)
    }

    fn abort(self) {
        // Nothing to abort for RocksDB
    }

    fn cursor_read<T: Table>(&self) -> Result<Self::Cursor<T>, DatabaseError> {
        Ok(cursor::Cursor::new(self.db.clone()))
    }

    fn cursor_dup_read<T: DupSort>(&self) -> Result<Self::DupCursor<T>, DatabaseError> {
        Ok(cursor::DupCursor::new(self.db.clone()))
    }

    fn entries<T: Table>(&self) -> Result<usize, DatabaseError> {
        let cf_handle = get_cf_handle::<T>(&self.db)?;

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

    fn disable_long_read_transaction_safety(&mut self) {
        // For RocksDB, this is a no-op as it doesn't have the same long read transaction safety
        // concerns as MDBX RocksDB handles concurrent reads and writes differently
    }
}

impl DbTxMut for Tx<RW> {
    type CursorMut<T: Table> = cursor::Cursor<T>;
    type DupCursorMut<T: DupSort> = cursor::DupCursor<T>;

    fn put<T: Table>(&self, key: T::Key, value: T::Value) -> Result<(), DatabaseError> {
        let cf_handle = get_cf_handle::<T>(&self.db)?;

        let encoded_key = key.encode();
        let encoded_value = value.compress();

        self.db.put_cf(cf_handle, &encoded_key, &encoded_value).map_err(|e| {
            create_write_error::<T>(e, DatabaseWriteOperation::Put, encoded_key.as_ref().to_vec())
        })?;

        Ok(())
    }

    fn delete<T: Table>(
        &self,
        key: T::Key,
        _value: Option<T::Value>,
    ) -> Result<bool, DatabaseError> {
        let cf_handle = get_cf_handle::<T>(&self.db)?;

        let encoded_key = key.encode();

        match self.db.delete_cf(cf_handle, &encoded_key) {
            Ok(()) => Ok(true),
            Err(e) => Err(create_write_error::<T>(
                e,
                DatabaseWriteOperation::Put,
                encoded_key.as_ref().to_vec(),
            )),
        }
    }

    fn clear<T: Table>(&self) -> Result<(), DatabaseError> {
        let cf_handle = get_cf_handle::<T>(&self.db)?;

        // Get the range of all keys in the column family
        let mut iter = self.db.iterator_cf(cf_handle, rocksdb::IteratorMode::Start);

        if let Some(Ok((first_key, _))) = iter.next() {
            // Find the last key
            let mut last_key = first_key.clone();
            while let Some(Ok((key, _))) = iter.next() {
                last_key = key;
            }

            // Delete the range
            self.db.delete_range_cf(cf_handle, &first_key, &last_key).map_err(|e| {
                create_write_error::<T>(e, DatabaseWriteOperation::Put, first_key.to_vec())
            })?;
        }

        Ok(())
    }

    fn cursor_write<T: Table>(&self) -> Result<Self::CursorMut<T>, DatabaseError> {
        Ok(cursor::Cursor::new(self.db.clone()))
    }

    fn cursor_dup_write<T: DupSort>(&self) -> Result<Self::DupCursorMut<T>, DatabaseError> {
        Ok(cursor::DupCursor::new(self.db.clone()))
    }
}

impl TableImporter for Tx<RW> {
    // Default implementation is sufficient for now
}
