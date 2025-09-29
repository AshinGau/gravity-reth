//! Transaction implementation for RocksDB.

use crate::{
    implementation::rocksdb::{
        create_write_error, cursor, get_cf_handle, rocksdb_error_to_database_error,
    },
    DatabaseError,
};
use parking_lot::Mutex;
use reth_db_api::{
    table::{Compress, Decompress, DupSort, Encode, Table, TableImporter},
    transaction::{DbTx, DbTxMut},
};
use reth_storage_errors::db::DatabaseWriteOperation;
use rocksdb::{Transaction, TransactionDB};
use std::sync::Arc;

pub use cursor::{RO, RW};

/// RocksDB transaction.
pub struct Tx<K: cursor::TransactionKind> {
    db: Arc<TransactionDB>,
    transaction: Arc<Mutex<Transaction<'static, TransactionDB>>>,
    _mode: std::marker::PhantomData<K>,
}

impl<K: cursor::TransactionKind> std::fmt::Debug for Tx<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tx")
            .field("db", &"<TransactionDB>")
            .field("transaction", &"<Transaction>")
            .field("_mode", &self._mode)
            .finish()
    }
}

impl<K: cursor::TransactionKind> Tx<K> {
    pub(crate) fn new(db: Arc<TransactionDB>) -> Self {
        let transaction = unsafe {
            // SAFETY: We ensure the TransactionDB outlives the transaction by holding
            // Arc<TransactionDB>
            std::mem::transmute::<Transaction<'_, TransactionDB>, Transaction<'static, TransactionDB>>(
                db.transaction(),
            )
        };

        Self { db, transaction: Arc::new(Mutex::new(transaction)), _mode: std::marker::PhantomData }
    }

    pub fn table_entries(&self, _name: &str) -> Result<usize, DatabaseError> {
        Ok(0)
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
        let transaction = self.transaction.lock();

        // Use transaction for read operations to ensure consistency
        match transaction.get_cf(cf_handle, key) {
            Ok(Some(value)) => {
                T::Value::decompress(&value).map(Some).map_err(|_| DatabaseError::Decode)
            }
            Ok(None) => Ok(None),
            Err(e) => Err(rocksdb_error_to_database_error(e)),
        }
    }

    fn commit(self) -> Result<bool, DatabaseError> {
        let transaction = Arc::try_unwrap(self.transaction)
            .map_err(|_| DatabaseError::Other("Failed to unwrap transaction".into()))?
            .into_inner();
        transaction
            .commit()
            .map_err(|e| DatabaseError::Other(format!("Failed to commit transaction: {}", e)))?;
        Ok(true)
    }

    fn abort(self) {
        // Nothing to abort for RocksDB
    }

    fn cursor_read<T: Table>(&self) -> Result<Self::Cursor<T>, DatabaseError> {
        cursor::Cursor::new(self.db.clone(), self.transaction.clone())
    }

    fn cursor_dup_read<T: DupSort>(&self) -> Result<Self::DupCursor<T>, DatabaseError> {
        cursor::Cursor::new(self.db.clone(), self.transaction.clone())
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
        let transaction = self.transaction.lock();

        let encoded_key = key.encode();
        let encoded_value = value.compress();

        transaction.put_cf(cf_handle, &encoded_key, &encoded_value).map_err(|e| {
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
        let transaction = self.transaction.lock();

        let encoded_key = key.encode();
        match transaction.delete_cf(cf_handle, &encoded_key) {
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
        let transaction = self.transaction.lock();

        // Simple iteration and delete
        let mut iter = self.db.raw_iterator_cf(cf_handle);
        iter.seek_to_first();

        while iter.valid() {
            if let Some(key) = iter.key() {
                transaction.delete_cf(cf_handle, key).map_err(|e| {
                    create_write_error::<T>(e, DatabaseWriteOperation::Put, key.to_vec())
                })?;
                iter.next();
            } else {
                break;
            }
        }

        Ok(())
    }

    fn cursor_write<T: Table>(&self) -> Result<Self::CursorMut<T>, DatabaseError> {
        cursor::Cursor::new(self.db.clone(), self.transaction.clone())
    }

    fn cursor_dup_write<T: DupSort>(&self) -> Result<Self::DupCursorMut<T>, DatabaseError> {
        cursor::Cursor::new(self.db.clone(), self.transaction.clone())
    }
}

impl TableImporter for Tx<RW> {
    // Default implementation is sufficient for now
}
