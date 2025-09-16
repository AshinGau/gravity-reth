//! Transaction implementation for RocksDB.

use std::sync::Arc;
use rocksdb::DB;
use reth_db_api::{
    cursor::{DbCursorRO, DbCursorRW, DbDupCursorRO, DbDupCursorRW},
    transaction::{DbTx, DbTxMut},
    table::{Table, TableImporter, Encode, Decode, Compress, Decompress},
};
use crate::DatabaseError;
use reth_storage_errors::db::{DatabaseErrorInfo, DatabaseWriteError, DatabaseWriteOperation};

/// Helper function to convert RocksDB errors to DatabaseError
fn rocksdb_error_to_database_error(e: rocksdb::Error) -> DatabaseError {
    DatabaseError::Read(DatabaseErrorInfo {
        message: e.to_string().into(),
        code: -1,
    })
}

/// Helper function to create a write error
fn create_write_error<T: Table>(
    e: rocksdb::Error,
    operation: DatabaseWriteOperation,
    key: Vec<u8>,
) -> DatabaseError {
    DatabaseError::Write(Box::new(DatabaseWriteError {
        info: DatabaseErrorInfo {
            message: e.to_string().into(),
            code: -1,
        },
        operation,
        table_name: T::NAME,
        key,
    }))
}

/// Helper function to get column family handle with proper error handling
fn get_cf_handle<T: Table>(db: &DB) -> Result<&rocksdb::ColumnFamily, DatabaseError> {
    db.cf_handle(T::NAME)
        .ok_or_else(|| DatabaseError::Open(DatabaseErrorInfo {
            message: format!("Column family '{}' not found", T::NAME).into(),
            code: -1,
        }))
}

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
        Self {
            db,
            _mode: std::marker::PhantomData,
        }
    }
}

impl<MODE: std::fmt::Debug + Send + Sync> DbTx for Tx<MODE> {
    type Cursor<T: Table> = cursor::Cursor<T>;
    type DupCursor<T: Table + reth_db_api::table::DupSort> = cursor::DupCursor<T>;

    fn get<T: Table>(&self, key: T::Key) -> Result<Option<T::Value>, DatabaseError> {
        let encoded_key = key.encode();
        self.get_by_encoded_key::<T>(&encoded_key)
    }

    fn get_by_encoded_key<T: Table>(&self, key: &<T::Key as Encode>::Encoded) -> Result<Option<T::Value>, DatabaseError> {
        let cf_handle = get_cf_handle::<T>(&self.db)?;
        
        match self.db.get_cf(cf_handle, key) {
            Ok(Some(value)) => {
                T::Value::decompress(&value)
                    .map(Some)
                    .map_err(|_| DatabaseError::Decode)
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

    fn cursor_dup_read<T: Table + reth_db_api::table::DupSort>(&self) -> Result<Self::DupCursor<T>, DatabaseError> {
        Ok(cursor::DupCursor::new(self.db.clone()))
    }

    fn entries<T: Table>(&self) -> Result<usize, DatabaseError> {
        let cf_handle = get_cf_handle::<T>(&self.db)?;
        
        // Use property_value_cf to get estimated number of keys
        match self.db.property_value_cf(cf_handle, "rocksdb.estimate-num-keys") {
            Ok(Some(value)) => {
                // Parse the string value to usize
                value.parse::<usize>()
                    .map_err(|_| DatabaseError::Read(DatabaseErrorInfo {
                        message: "Failed to parse estimated number of keys".into(),
                        code: -1,
                    }))
            }
            Ok(None) => Ok(0),
            Err(e) => Err(rocksdb_error_to_database_error(e)),
        }
    }

    fn disable_long_read_transaction_safety(&mut self) {
        // For RocksDB, this is a no-op as it doesn't have the same long read transaction safety concerns as MDBX
        // RocksDB handles concurrent reads and writes differently
    }
}

impl DbTxMut for Tx<RW> {
    type CursorMut<T: Table> = cursor::Cursor<T>;
    type DupCursorMut<T: Table + reth_db_api::table::DupSort> = cursor::DupCursor<T>;

    fn put<T: Table>(&self, key: T::Key, value: T::Value) -> Result<(), DatabaseError> {
        let cf_handle = get_cf_handle::<T>(&self.db)?;
        
        let encoded_key = key.encode();
        let encoded_value = value.compress();
        
        self.db.put_cf(cf_handle, &encoded_key, &encoded_value)
            .map_err(|e| create_write_error::<T>(e, DatabaseWriteOperation::Put, encoded_key.as_ref().to_vec()))?;
        
        Ok(())
    }

    fn delete<T: Table>(&self, key: T::Key, _value: Option<T::Value>) -> Result<bool, DatabaseError> {
        let cf_handle = get_cf_handle::<T>(&self.db)?;
        
        let encoded_key = key.encode();
        
        match self.db.delete_cf(cf_handle, &encoded_key) {
            Ok(()) => Ok(true),
            Err(e) => Err(create_write_error::<T>(e, DatabaseWriteOperation::Put, encoded_key.as_ref().to_vec())),
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
            self.db.delete_range_cf(cf_handle, &first_key, &last_key)
                .map_err(|e| create_write_error::<T>(e, DatabaseWriteOperation::Put, first_key.to_vec()))?;
        }
        
        Ok(())
    }

    fn cursor_write<T: Table>(&self) -> Result<Self::CursorMut<T>, DatabaseError> {
        Ok(cursor::Cursor::new(self.db.clone()))
    }

    fn cursor_dup_write<T: Table + reth_db_api::table::DupSort>(&self) -> Result<Self::DupCursorMut<T>, DatabaseError> {
        Ok(cursor::DupCursor::new(self.db.clone()))
    }
}

impl TableImporter for Tx<RW> {
    // Default implementation is sufficient for now
}

pub mod cursor {
    //! Cursor implementation for RocksDB.

    use std::sync::Arc;
    use rocksdb::DB;
    use reth_db_api::{
        cursor::{DbCursorRO, DbCursorRW, DbDupCursorRO, DbDupCursorRW},
        table::{Table, Encode, Decode, Compress, Decompress},
    };
    use crate::DatabaseError;
    use reth_storage_errors::db::{DatabaseErrorInfo, DatabaseWriteError, DatabaseWriteOperation};

    /// Helper function to convert RocksDB errors to DatabaseError
    fn rocksdb_error_to_database_error(e: rocksdb::Error) -> DatabaseError {
        DatabaseError::Read(DatabaseErrorInfo {
            message: e.to_string().into(),
            code: -1,
        })
    }

    /// Helper function to create a write error
    fn create_write_error<T: Table>(
        e: rocksdb::Error,
        operation: DatabaseWriteOperation,
        key: Vec<u8>,
    ) -> DatabaseError {
        DatabaseError::Write(Box::new(DatabaseWriteError {
            info: DatabaseErrorInfo {
                message: e.to_string().into(),
                code: -1,
            },
            operation,
            table_name: T::NAME,
            key,
        }))
    }

    /// Helper function to get column family handle with proper error handling
    fn get_cf_handle<T: Table>(db: &DB) -> Result<&rocksdb::ColumnFamily, DatabaseError> {
        db.cf_handle(T::NAME)
            .ok_or_else(|| DatabaseError::Open(DatabaseErrorInfo {
                message: format!("Column family '{}' not found", T::NAME).into(),
                code: -1,
            }))
    }

    /// RocksDB cursor.
    pub struct Cursor<T: Table> {
        db: Arc<DB>,
        table_name: &'static str,
        current_key: Option<Vec<u8>>,
        current_value: Option<Vec<u8>>,
        _table: std::marker::PhantomData<T>,
    }

    impl<T: Table> Cursor<T> {
        pub(crate) fn new(db: Arc<DB>) -> Self {
            Self {
                db,
                table_name: T::NAME,
                current_key: None,
                current_value: None,
                _table: std::marker::PhantomData,
            }
        }

        fn get_cf_handle(&self) -> &rocksdb::ColumnFamily {
            self.db.cf_handle(self.table_name)
                .expect("Column family should exist")
        }

        fn decode_key_value(&self, key: &[u8], value: &[u8]) -> Result<(T::Key, T::Value), DatabaseError> {
            let decoded_key = T::Key::decode(key)
                .map_err(|_| DatabaseError::Decode)?;
            let decoded_value = T::Value::decompress(value)
                .map_err(|_| DatabaseError::Decode)?;
            Ok((decoded_key, decoded_value))
        }
    }

    impl<T: Table> DbCursorRO<T> for Cursor<T> {
        fn first(&mut self) -> Result<Option<(T::Key, T::Value)>, DatabaseError> {
            let cf_handle = self.get_cf_handle();
            let mut iter = self.db.iterator_cf(cf_handle, rocksdb::IteratorMode::Start);
            
            if let Some(Ok((key, value))) = iter.next() {
                self.current_key = Some(key.to_vec());
                self.current_value = Some(value.to_vec());
                self.decode_key_value(&key, &value)
                    .map(Some)
            } else {
                self.current_key = None;
                self.current_value = None;
                Ok(None)
            }
        }

        fn seek_exact(&mut self, key: T::Key) -> Result<Option<(T::Key, T::Value)>, DatabaseError> {
            let encoded_key = key.encode();
            let cf_handle = self.get_cf_handle();
            
            match self.db.get_cf(cf_handle, &encoded_key) {
                Ok(Some(value)) => {
                    self.current_key = Some(encoded_key.as_ref().to_vec());
                    self.current_value = Some(value.to_vec());
                    self.decode_key_value(encoded_key.as_ref(), &value)
                        .map(Some)
                }
                Ok(None) => {
                    self.current_key = None;
                    self.current_value = None;
                    Ok(None)
                }
                Err(e) => Err(rocksdb_error_to_database_error(e)),
            }
        }

        fn seek(&mut self, key: T::Key) -> Result<Option<(T::Key, T::Value)>, DatabaseError> {
            let encoded_key = key.encode();
            let mut iter = self.db.iterator_cf(
                self.get_cf_handle(), 
                rocksdb::IteratorMode::From(encoded_key.as_ref(), rocksdb::Direction::Forward)
            );
            
            if let Some(Ok((key, value))) = iter.next() {
                self.current_key = Some(key.to_vec());
                self.current_value = Some(value.to_vec());
                self.decode_key_value(&key, &value)
                    .map(Some)
            } else {
                self.current_key = None;
                self.current_value = None;
                Ok(None)
            }
        }

        fn next(&mut self) -> Result<Option<(T::Key, T::Value)>, DatabaseError> {
            if let Some(ref current_key) = self.current_key {
                let mut iter = self.db.iterator_cf(
                    self.get_cf_handle(),
                    rocksdb::IteratorMode::From(current_key, rocksdb::Direction::Forward)
                );
                
                // Skip the current key
                iter.next();
                
                if let Some(Ok((key, value))) = iter.next() {
                    self.current_key = Some(key.to_vec());
                    self.current_value = Some(value.to_vec());
                    self.decode_key_value(&key, &value)
                        .map(Some)
                } else {
                    self.current_key = None;
                    self.current_value = None;
                    Ok(None)
                }
            } else {
                Ok(None)
            }
        }

        fn prev(&mut self) -> Result<Option<(T::Key, T::Value)>, DatabaseError> {
            if let Some(ref current_key) = self.current_key {
                let mut iter = self.db.iterator_cf(
                    self.get_cf_handle(),
                    rocksdb::IteratorMode::From(current_key, rocksdb::Direction::Reverse)
                );
                
                // Skip the current key
                iter.next();
                
                if let Some(Ok((key, value))) = iter.next() {
                    self.current_key = Some(key.to_vec());
                    self.current_value = Some(value.to_vec());
                    self.decode_key_value(&key, &value)
                        .map(Some)
                } else {
                    self.current_key = None;
                    self.current_value = None;
                    Ok(None)
                }
            } else {
                Ok(None)
            }
        }

        fn last(&mut self) -> Result<Option<(T::Key, T::Value)>, DatabaseError> {
            let mut iter = self.db.iterator_cf(self.get_cf_handle(), rocksdb::IteratorMode::End);
            
            if let Some(Ok((key, value))) = iter.next() {
                self.current_key = Some(key.to_vec());
                self.current_value = Some(value.to_vec());
                self.decode_key_value(&key, &value)
                    .map(Some)
            } else {
                self.current_key = None;
                self.current_value = None;
                Ok(None)
            }
        }

        fn current(&mut self) -> Result<Option<(T::Key, T::Value)>, DatabaseError> {
            if let (Some(ref key), Some(ref value)) = (&self.current_key, &self.current_value) {
                self.decode_key_value(key, value)
                    .map(Some)
            } else {
                Ok(None)
            }
        }

        fn walk(&mut self, start_key: Option<T::Key>) -> Result<reth_db_api::cursor::Walker<'_, T, Self>, DatabaseError> {
            let start: reth_db_api::common::IterPairResult<T> = match start_key {
                Some(key) => self.seek(key).transpose(),
                None => self.first().transpose(),
            };

            Ok(reth_db_api::cursor::Walker::new(self, start))
        }

        fn walk_range(&mut self, range: impl std::ops::RangeBounds<T::Key>) -> Result<reth_db_api::cursor::RangeWalker<'_, T, Self>, DatabaseError> {
            use std::ops::Bound;
            
            let start_key = match range.start_bound() {
                Bound::Included(key) | Bound::Excluded(key) => Some((*key).clone()),
                Bound::Unbounded => None,
            };

            let end_key = match range.end_bound() {
                Bound::Included(key) | Bound::Excluded(key) => Bound::Included((*key).clone()),
                Bound::Unbounded => Bound::Unbounded,
            };

            let start: reth_db_api::common::IterPairResult<T> = match start_key {
                Some(key) => self.seek(key).transpose(),
                None => self.first().transpose(),
            };

            Ok(reth_db_api::cursor::RangeWalker::new(self, start, end_key))
        }

        fn walk_back(&mut self, start_key: Option<T::Key>) -> Result<reth_db_api::cursor::ReverseWalker<'_, T, Self>, DatabaseError> {
            let start: reth_db_api::common::IterPairResult<T> = match start_key {
                Some(key) => {
                    // For reverse walk, we need to find the position and then get current
                    self.seek(key)?;
                    self.current().transpose()
                }
                None => self.last().transpose(),
            };

            Ok(reth_db_api::cursor::ReverseWalker::new(self, start))
        }
    }

    impl<T: Table> DbCursorRW<T> for Cursor<T> {
        fn upsert(&mut self, key: T::Key, value: &T::Value) -> Result<(), DatabaseError> {
            let cf_handle = self.get_cf_handle();
            let encoded_key = key.encode();
            let mut encoded_value = Vec::new();
            value.compress_to_buf(&mut encoded_value);
            
            self.db.put_cf(cf_handle, &encoded_key, &encoded_value)
            .map_err(|e| create_write_error::<T>(e, DatabaseWriteOperation::Put, encoded_key.as_ref().to_vec()))?;
            
            // Update current position
            self.current_key = Some(encoded_key.as_ref().to_vec());
            self.current_value = Some(encoded_value.into());
            
            Ok(())
        }

        fn insert(&mut self, key: T::Key, value: &T::Value) -> Result<(), DatabaseError> {
            let cf_handle = self.get_cf_handle();
            let encoded_key = key.encode();
            
            // Check if key already exists
            if self.db.get_cf(cf_handle, &encoded_key)
                .map_err(|e| DatabaseError::Read(DatabaseErrorInfo {
                    message: e.to_string().into(),
                    code: -1,
                }))?.is_some() {
                return Err(DatabaseError::Write(Box::new(DatabaseWriteError {
                    info: DatabaseErrorInfo {
                        message: "Key already exists".into(),
                        code: -1,
                    },
                    operation: DatabaseWriteOperation::CursorInsert,
                    table_name: T::NAME,
                    key: encoded_key.as_ref().to_vec(),
                })));
            }
            
            let mut encoded_value = Vec::new();
            value.compress_to_buf(&mut encoded_value);
            self.db.put_cf(cf_handle, &encoded_key, &encoded_value)
            .map_err(|e| create_write_error::<T>(e, DatabaseWriteOperation::CursorInsert, encoded_key.as_ref().to_vec()))?;
            
            // Update current position
            self.current_key = Some(encoded_key.as_ref().to_vec());
            self.current_value = Some(encoded_value.into());
            
            Ok(())
        }

        fn append(&mut self, key: T::Key, value: &T::Value) -> Result<(), DatabaseError> {
            // For RocksDB, append is the same as upsert
            self.upsert(key, value)
        }

        fn delete_current(&mut self) -> Result<(), DatabaseError> {
            if let Some(ref key) = self.current_key {
                let cf_handle = self.get_cf_handle();
                self.db.delete_cf(cf_handle, key)
                    .map_err(|e| create_write_error::<T>(e, DatabaseWriteOperation::Put, key.clone()))?;
                
                self.current_key = None;
                self.current_value = None;
            }
            
            Ok(())
        }
    }

    /// RocksDB duplicate cursor.
    pub struct DupCursor<T: Table> {
        db: Arc<DB>,
        table_name: &'static str,
        current_key: Option<Vec<u8>>,
        current_value: Option<Vec<u8>>,
        _table: std::marker::PhantomData<T>,
    }

    impl<T: Table> DupCursor<T> {
        pub(crate) fn new(db: Arc<DB>) -> Self {
            Self {
                db,
                table_name: T::NAME,
                current_key: None,
                current_value: None,
                _table: std::marker::PhantomData,
            }
        }

        fn get_cf_handle(&self) -> &rocksdb::ColumnFamily {
            self.db.cf_handle(self.table_name)
                .expect("Column family should exist")
        }

        fn decode_key_value(&self, key: &[u8], value: &[u8]) -> Result<(T::Key, T::Value), DatabaseError> {
            let decoded_key = T::Key::decode(key)
                .map_err(|_| DatabaseError::Decode)?;
            let decoded_value = T::Value::decompress(value)
                .map_err(|_| DatabaseError::Decode)?;
            Ok((decoded_key, decoded_value))
        }
    }

    impl<T: Table> DbCursorRO<T> for DupCursor<T> {
        fn first(&mut self) -> Result<Option<(T::Key, T::Value)>, DatabaseError> {
            let mut iter = self.db.iterator_cf(self.get_cf_handle(), rocksdb::IteratorMode::Start);
            
            if let Some(Ok((key, value))) = iter.next() {
                self.current_key = Some(key.to_vec());
                self.current_value = Some(value.to_vec());
                self.decode_key_value(&key, &value)
                    .map(Some)
            } else {
                self.current_key = None;
                self.current_value = None;
                Ok(None)
            }
        }

        fn seek_exact(&mut self, key: T::Key) -> Result<Option<(T::Key, T::Value)>, DatabaseError> {
            let encoded_key = key.encode();
            let cf_handle = self.get_cf_handle();
            
            match self.db.get_cf(cf_handle, &encoded_key) {
                Ok(Some(value)) => {
                    self.current_key = Some(encoded_key.as_ref().to_vec());
                    self.current_value = Some(value.to_vec());
                    self.decode_key_value(encoded_key.as_ref(), &value)
                        .map(Some)
                }
                Ok(None) => {
                    self.current_key = None;
                    self.current_value = None;
                    Ok(None)
                }
                Err(e) => Err(rocksdb_error_to_database_error(e)),
            }
        }

        fn seek(&mut self, key: T::Key) -> Result<Option<(T::Key, T::Value)>, DatabaseError> {
            let encoded_key = key.encode();
            let mut iter = self.db.iterator_cf(
                self.get_cf_handle(), 
                rocksdb::IteratorMode::From(encoded_key.as_ref(), rocksdb::Direction::Forward)
            );
            
            if let Some(Ok((key, value))) = iter.next() {
                self.current_key = Some(key.to_vec());
                self.current_value = Some(value.to_vec());
                self.decode_key_value(&key, &value)
                    .map(Some)
            } else {
                self.current_key = None;
                self.current_value = None;
                Ok(None)
            }
        }

        fn next(&mut self) -> Result<Option<(T::Key, T::Value)>, DatabaseError> {
            if let Some(ref current_key) = self.current_key {
                let mut iter = self.db.iterator_cf(
                    self.get_cf_handle(),
                    rocksdb::IteratorMode::From(current_key, rocksdb::Direction::Forward)
                );
                
                // Skip the current key
                iter.next();
                
                if let Some(Ok((key, value))) = iter.next() {
                    self.current_key = Some(key.to_vec());
                    self.current_value = Some(value.to_vec());
                    self.decode_key_value(&key, &value)
                        .map(Some)
                } else {
                    self.current_key = None;
                    self.current_value = None;
                    Ok(None)
                }
            } else {
                Ok(None)
            }
        }

        fn prev(&mut self) -> Result<Option<(T::Key, T::Value)>, DatabaseError> {
            if let Some(ref current_key) = self.current_key {
                let mut iter = self.db.iterator_cf(
                    self.get_cf_handle(),
                    rocksdb::IteratorMode::From(current_key, rocksdb::Direction::Reverse)
                );
                
                // Skip the current key
                iter.next();
                
                if let Some(Ok((key, value))) = iter.next() {
                    self.current_key = Some(key.to_vec());
                    self.current_value = Some(value.to_vec());
                    self.decode_key_value(&key, &value)
                        .map(Some)
                } else {
                    self.current_key = None;
                    self.current_value = None;
                    Ok(None)
                }
            } else {
                Ok(None)
            }
        }

        fn last(&mut self) -> Result<Option<(T::Key, T::Value)>, DatabaseError> {
            let mut iter = self.db.iterator_cf(self.get_cf_handle(), rocksdb::IteratorMode::End);
            
            if let Some(Ok((key, value))) = iter.next() {
                self.current_key = Some(key.to_vec());
                self.current_value = Some(value.to_vec());
                self.decode_key_value(&key, &value)
                    .map(Some)
            } else {
                self.current_key = None;
                self.current_value = None;
                Ok(None)
            }
        }

        fn current(&mut self) -> Result<Option<(T::Key, T::Value)>, DatabaseError> {
            if let (Some(ref key), Some(ref value)) = (&self.current_key, &self.current_value) {
                self.decode_key_value(key, value)
                    .map(Some)
            } else {
                Ok(None)
            }
        }

        fn walk(&mut self, start_key: Option<T::Key>) -> Result<reth_db_api::cursor::Walker<'_, T, Self>, DatabaseError> {
            let start: reth_db_api::common::IterPairResult<T> = match start_key {
                Some(key) => self.seek(key).transpose(),
                None => self.first().transpose(),
            };

            Ok(reth_db_api::cursor::Walker::new(self, start))
        }

        fn walk_range(&mut self, range: impl std::ops::RangeBounds<T::Key>) -> Result<reth_db_api::cursor::RangeWalker<'_, T, Self>, DatabaseError> {
            use std::ops::Bound;
            
            let start_key = match range.start_bound() {
                Bound::Included(key) | Bound::Excluded(key) => Some((*key).clone()),
                Bound::Unbounded => None,
            };

            let end_key = match range.end_bound() {
                Bound::Included(key) | Bound::Excluded(key) => Bound::Included((*key).clone()),
                Bound::Unbounded => Bound::Unbounded,
            };

            let start: reth_db_api::common::IterPairResult<T> = match start_key {
                Some(key) => self.seek(key).transpose(),
                None => self.first().transpose(),
            };

            Ok(reth_db_api::cursor::RangeWalker::new(self, start, end_key))
        }

        fn walk_back(&mut self, start_key: Option<T::Key>) -> Result<reth_db_api::cursor::ReverseWalker<'_, T, Self>, DatabaseError> {
            let start: reth_db_api::common::IterPairResult<T> = match start_key {
                Some(key) => {
                    // For reverse walk, we need to find the position and then get current
                    self.seek(key)?;
                    self.current().transpose()
                }
                None => self.last().transpose(),
            };

            Ok(reth_db_api::cursor::ReverseWalker::new(self, start))
        }
    }

    impl<T: Table + reth_db_api::table::DupSort> DbDupCursorRO<T> for DupCursor<T> {
        fn next_dup(&mut self) -> Result<Option<(T::Key, T::Value)>, DatabaseError> {
            // For RocksDB, we need to implement duplicate handling
            // This is a simplified implementation that treats it like a regular next
            self.next()
        }

        fn next_no_dup(&mut self) -> Result<Option<(T::Key, T::Value)>, DatabaseError> {
            // For RocksDB, we need to skip duplicates
            // This is a simplified implementation
            if let Some(current_key) = &self.current_key {
                let mut iter = self.db.iterator_cf(
                    self.get_cf_handle(),
                    rocksdb::IteratorMode::From(current_key, rocksdb::Direction::Forward)
                );
                
                // Skip the current key
                iter.next();
                
                // Find the next key that's different from current
                while let Some(Ok((key, value))) = iter.next() {
                    if key.as_ref() != current_key.as_slice() {
                        self.current_key = Some(key.to_vec());
                        self.current_value = Some(value.to_vec());
                        return self.decode_key_value(&key, &value).map(Some);
                    }
                }
                
                self.current_key = None;
                self.current_value = None;
            }
            Ok(None)
        }

        fn next_dup_val(&mut self) -> Result<Option<T::Value>, DatabaseError> {
            // For RocksDB, this would need to handle duplicate values
            // This is a simplified implementation
            if let Some(current_key) = &self.current_key {
                let mut iter = self.db.iterator_cf(
                    self.get_cf_handle(),
                    rocksdb::IteratorMode::From(current_key, rocksdb::Direction::Forward)
                );
                
                // Skip the current key
                iter.next();
                
                // Find the next entry with the same key
                while let Some(Ok((key, value))) = iter.next() {
                    if key.as_ref() == current_key.as_slice() {
                        self.current_value = Some(value.to_vec());
                        return T::Value::decompress(&value).map(Some).map_err(|_| DatabaseError::Decode);
                    } else {
                        break;
                    }
                }
            }
            Ok(None)
        }

        fn seek_by_key_subkey(&mut self, key: T::Key, subkey: T::SubKey) -> Result<Option<T::Value>, DatabaseError> {
            // For RocksDB, we need to construct a composite key from key and subkey
            // This is a simplified implementation
            let encoded_key = key.encode();
            let encoded_subkey = subkey.encode();
            
            // Combine key and subkey (this is table-specific and would need proper implementation)
            let mut composite_key = encoded_key.as_ref().to_vec();
            composite_key.extend_from_slice(encoded_subkey.as_ref());
            
            let cf_handle = self.get_cf_handle();
            match self.db.get_cf(cf_handle, &composite_key) {
                Ok(Some(value)) => {
                    self.current_key = Some(composite_key);
                    self.current_value = Some(value.to_vec());
                    T::Value::decompress(&value).map(Some).map_err(|_| DatabaseError::Decode)
                }
                Ok(None) => {
                    self.current_key = None;
                    self.current_value = None;
                    Ok(None)
                }
                Err(e) => Err(rocksdb_error_to_database_error(e)),
            }
        }

        fn walk_dup(&mut self, key: Option<T::Key>, subkey: Option<T::SubKey>) -> Result<reth_db_api::cursor::DupWalker<'_, T, Self>, DatabaseError> {
            let start: reth_db_api::common::IterPairResult<T> = match (key, subkey) {
                (Some(key), Some(subkey)) => {
                    // Seek to the specific key/subkey combination
                    self.seek_by_key_subkey(key, subkey)?;
                    self.current().transpose()
                }
                (Some(key), None) => {
                    // Seek to the key
                    self.seek(key).transpose()
                }
                (None, Some(_subkey)) => {
                    // This case is not well-defined in the API, fall back to first
                    self.first().transpose()
                }
                (None, None) => {
                    // Start from the beginning
                    self.first().transpose()
                }
            };

            Ok(reth_db_api::cursor::DupWalker { cursor: self, start })
        }
    }

    impl<T: Table> DbCursorRW<T> for DupCursor<T> {
        fn upsert(&mut self, key: T::Key, value: &T::Value) -> Result<(), DatabaseError> {
            let cf_handle = self.get_cf_handle();
            let encoded_key = key.encode();
            let mut encoded_value = Vec::new();
            value.compress_to_buf(&mut encoded_value);
            
            self.db.put_cf(cf_handle, &encoded_key, &encoded_value)
            .map_err(|e| create_write_error::<T>(e, DatabaseWriteOperation::Put, encoded_key.as_ref().to_vec()))?;
            
            // Update current position
            self.current_key = Some(encoded_key.as_ref().to_vec());
            self.current_value = Some(encoded_value.into());
            
            Ok(())
        }

        fn insert(&mut self, key: T::Key, value: &T::Value) -> Result<(), DatabaseError> {
            let cf_handle = self.get_cf_handle();
            let encoded_key = key.encode();
            
            // Check if key already exists
            if self.db.get_cf(cf_handle, &encoded_key)
                .map_err(|e| DatabaseError::Read(DatabaseErrorInfo {
                    message: e.to_string().into(),
                    code: -1,
                }))?.is_some() {
                return Err(DatabaseError::Write(Box::new(DatabaseWriteError {
                    info: DatabaseErrorInfo {
                        message: "Key already exists".into(),
                        code: -1,
                    },
                    operation: DatabaseWriteOperation::CursorInsert,
                    table_name: T::NAME,
                    key: encoded_key.as_ref().to_vec(),
                })));
            }
            
            let mut encoded_value = Vec::new();
            value.compress_to_buf(&mut encoded_value);
            self.db.put_cf(cf_handle, &encoded_key, &encoded_value)
            .map_err(|e| create_write_error::<T>(e, DatabaseWriteOperation::CursorInsert, encoded_key.as_ref().to_vec()))?;
            
            // Update current position
            self.current_key = Some(encoded_key.as_ref().to_vec());
            self.current_value = Some(encoded_value.into());
            
            Ok(())
        }

        fn append(&mut self, key: T::Key, value: &T::Value) -> Result<(), DatabaseError> {
            // For RocksDB, append is the same as upsert
            self.upsert(key, value)
        }

        fn delete_current(&mut self) -> Result<(), DatabaseError> {
            if let Some(ref key) = self.current_key {
                let cf_handle = self.get_cf_handle();
                self.db.delete_cf(cf_handle, key)
                    .map_err(|e| create_write_error::<T>(e, DatabaseWriteOperation::Put, key.clone()))?;
                
                self.current_key = None;
                self.current_value = None;
            }
            
            Ok(())
        }
    }

    impl<T: Table + reth_db_api::table::DupSort> DbDupCursorRW<T> for DupCursor<T> {
        fn delete_current_duplicates(&mut self) -> Result<(), DatabaseError> {
            if let Some(ref current_key) = self.current_key {
                let cf_handle = self.get_cf_handle();
                
                // Find all entries with the same key and delete them
                let mut iter = self.db.iterator_cf(
                    cf_handle,
                    rocksdb::IteratorMode::From(current_key, rocksdb::Direction::Forward)
                );
                
                let mut keys_to_delete = Vec::new();
                while let Some(Ok((key, _))) = iter.next() {
                    if key.as_ref() == current_key.as_slice() {
                        keys_to_delete.push(key.to_vec());
                    } else {
                        break;
                    }
                }
                
                // Delete all found keys
                for key in keys_to_delete {
                    self.db.delete_cf(cf_handle, &key)
                        .map_err(|e| create_write_error::<T>(e, DatabaseWriteOperation::Put, key.clone()))?;
                }
                
                self.current_key = None;
                self.current_value = None;
            }
            
            Ok(())
        }

        fn append_dup(&mut self, key: T::Key, value: T::Value) -> Result<(), DatabaseError> {
            let cf_handle = self.get_cf_handle();
            let encoded_key = key.encode();
            let mut encoded_value = Vec::new();
            value.compress_to_buf(&mut encoded_value);
            
            // For RocksDB, we need to create a unique key for duplicates
            // This is a simplified implementation - in practice, this would need
            // to be table-specific to handle the composite key properly
            let mut composite_key = encoded_key.as_ref().to_vec();
            composite_key.extend_from_slice(&encoded_value.as_ref());
            
            self.db.put_cf(cf_handle, &composite_key, &encoded_value)
            .map_err(|e| create_write_error::<T>(e, DatabaseWriteOperation::Put, composite_key.clone()))?;
            
            // Update current position
            self.current_key = Some(composite_key);
            self.current_value = Some(encoded_value.into());
            
            Ok(())
        }
    }
}
