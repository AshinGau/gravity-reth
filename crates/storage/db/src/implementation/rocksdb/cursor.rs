use crate::{
    implementation::rocksdb::{
        create_write_error, cursor, get_cf_handle, rocksdb_error_to_database_error,
    },
    DatabaseError,
};
use reth_db_api::{
    common::{IterPairResult, PairResult}, cursor::{DbCursorRO, DbCursorRW, DbDupCursorRO, DbDupCursorRW, Walker}, table::{Compress, Decode, Decompress, DupSort, Encode, Table}
};
use reth_storage_errors::db::{DatabaseErrorInfo, DatabaseWriteError, DatabaseWriteOperation};
use rocksdb::DB;
use std::sync::Arc;

/// Some types don't support compression (eg. B256), and we don't want to be copying them to the
/// allocated buffer when we can just use their reference.
macro_rules! compress_to_buf_or_ref {
    ($self:expr, $value:expr) => {
        if let Some(value) = $value.uncompressable_ref() {
            Some(value)
        } else {
            $self.buf.clear();
            $value.compress_to_buf(&mut $self.buf);
            None
        }
    };
}

/// RocksDB cursor.
pub struct Cursor<T: Table> {
    db: Arc<DB>,
    table_name: &'static str,
    current_key: Option<Vec<u8>>,
    current_value: Option<Vec<u8>>,
    /// Cache buffer that receives compressed values.
    buf: Vec<u8>,
    _table: std::marker::PhantomData<T>,
}

impl<T: Table> Cursor<T> {
    pub(crate) fn new(db: Arc<DB>) -> Self {
        Self {
            db,
            table_name: T::NAME,
            current_key: None,
            current_value: None,
            buf: Vec::new(),
            _table: std::marker::PhantomData,
        }
    }

    fn decode_key_value(
        &self,
        key: &[u8],
        value: &[u8],
    ) -> Result<(T::Key, T::Value), DatabaseError> {
        let decoded_key = T::Key::decode(key).map_err(|_| DatabaseError::Decode)?;
        let decoded_value = T::Value::decompress(value).map_err(|_| DatabaseError::Decode)?;
        Ok((decoded_key, decoded_value))
    }
}

impl<T: Table> DbCursorRO<T> for Cursor<T> {
    fn first(&mut self) -> PairResult<T> {
        let cf_handle = get_cf_handle::<T>(&self.db)?;
        let mut iter = self.db.iterator_cf(cf_handle, rocksdb::IteratorMode::Start);

        if let Some(Ok((key, value))) = iter.next() {
            self.current_key = Some(key.to_vec());
            self.current_value = Some(value.to_vec());
            self.decode_key_value(&key, &value).map(Some)
        } else {
            self.current_key = None;
            self.current_value = None;
            Ok(None)
        }
    }

    fn seek_exact(&mut self, key: T::Key) -> PairResult<T> {
        let encoded_key = key.encode();
        let cf_handle = get_cf_handle::<T>(&self.db)?;

        match self.db.get_cf(cf_handle, &encoded_key) {
            Ok(Some(value)) => {
                self.current_key = Some(encoded_key.as_ref().to_vec());
                self.current_value = Some(value.to_vec());
                self.decode_key_value(encoded_key.as_ref(), &value).map(Some)
            }
            Ok(None) => {
                self.current_key = None;
                self.current_value = None;
                Ok(None)
            }
            Err(e) => Err(rocksdb_error_to_database_error(e)),
        }
    }

    fn seek(&mut self, key: T::Key) -> PairResult<T> {
        let encoded_key = key.encode();
        let mut iter = self.db.iterator_cf(
            get_cf_handle::<T>(&self.db)?,
            rocksdb::IteratorMode::From(encoded_key.as_ref(), rocksdb::Direction::Forward),
        );

        if let Some(Ok((key, value))) = iter.next() {
            self.current_key = Some(key.to_vec());
            self.current_value = Some(value.to_vec());
            self.decode_key_value(&key, &value).map(Some)
        } else {
            self.current_key = None;
            self.current_value = None;
            Ok(None)
        }
    }

    fn next(&mut self) -> PairResult<T> {
        if let Some(ref current_key) = self.current_key {
            let mut iter = self.db.iterator_cf(
                get_cf_handle::<T>(&self.db)?,
                rocksdb::IteratorMode::From(current_key, rocksdb::Direction::Forward),
            );

            // Skip the current key
            iter.next();

            if let Some(Ok((key, value))) = iter.next() {
                self.current_key = Some(key.to_vec());
                self.current_value = Some(value.to_vec());
                self.decode_key_value(&key, &value).map(Some)
            } else {
                self.current_key = None;
                self.current_value = None;
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }

    fn prev(&mut self) -> PairResult<T> {
        if let Some(ref current_key) = self.current_key {
            let mut iter = self.db.iterator_cf(
                get_cf_handle::<T>(&self.db)?,
                rocksdb::IteratorMode::From(current_key, rocksdb::Direction::Reverse),
            );

            // Skip the current key
            iter.next();

            if let Some(Ok((key, value))) = iter.next() {
                self.current_key = Some(key.to_vec());
                self.current_value = Some(value.to_vec());
                self.decode_key_value(&key, &value).map(Some)
            } else {
                self.current_key = None;
                self.current_value = None;
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }

    fn last(&mut self) -> PairResult<T> {
        let mut iter = self.db.iterator_cf(get_cf_handle::<T>(&self.db)?, rocksdb::IteratorMode::End);

        if let Some(Ok((key, value))) = iter.next() {
            self.current_key = Some(key.to_vec());
            self.current_value = Some(value.to_vec());
            self.decode_key_value(&key, &value).map(Some)
        } else {
            self.current_key = None;
            self.current_value = None;
            Ok(None)
        }
    }

    fn current(&mut self) -> PairResult<T> {
        if let (Some(ref key), Some(ref value)) = (&self.current_key, &self.current_value) {
            self.decode_key_value(key, value).map(Some)
        } else {
            Ok(None)
        }
    }

    fn walk(
        &mut self,
        start_key: Option<T::Key>,
    ) -> Result<Walker<'_, T, Self>, DatabaseError> {
        let start: IterPairResult<T> = match start_key {
            Some(key) => self.seek(key).transpose(),
            None => self.first().transpose(),
        };

        Ok(Walker::new(self, start))
    }

    fn walk_range(
        &mut self,
        range: impl std::ops::RangeBounds<T::Key>,
    ) -> Result<reth_db_api::cursor::RangeWalker<'_, T, Self>, DatabaseError> {
        use std::ops::Bound;

        let start_key = match range.start_bound() {
            Bound::Included(key) | Bound::Excluded(key) => Some((*key).clone()),
            Bound::Unbounded => None,
        };

        let end_key = match range.end_bound() {
            Bound::Included(key) | Bound::Excluded(key) => Bound::Included((*key).clone()),
            Bound::Unbounded => Bound::Unbounded,
        };

        let start: IterPairResult<T> = match start_key {
            Some(key) => self.seek(key).transpose(),
            None => self.first().transpose(),
        };

        Ok(reth_db_api::cursor::RangeWalker::new(self, start, end_key))
    }

    fn walk_back(
        &mut self,
        start_key: Option<T::Key>,
    ) -> Result<reth_db_api::cursor::ReverseWalker<'_, T, Self>, DatabaseError> {
        let start: IterPairResult<T> = match start_key {
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
        let cf_handle = get_cf_handle::<T>(&self.db)?;
        let encoded_key = key.encode();
        let value_ref = compress_to_buf_or_ref!(self, value);

        self.db.put_cf(cf_handle, &encoded_key, value_ref.unwrap_or(&self.buf)).map_err(|e| {
            create_write_error::<T>(e, DatabaseWriteOperation::Put, encoded_key.as_ref().to_vec())
        })?;

        // Update current position
        self.current_key = Some(encoded_key.as_ref().to_vec());
        self.current_value = Some(value_ref.unwrap_or(&self.buf).to_vec());

        Ok(())
    }

    fn insert(&mut self, key: T::Key, value: &T::Value) -> Result<(), DatabaseError> {
        self.upsert(key, value)
    }

    fn append(&mut self, key: T::Key, value: &T::Value) -> Result<(), DatabaseError> {
        self.upsert(key, value)
    }

    fn delete_current(&mut self) -> Result<(), DatabaseError> {
        if let Some(ref key) = self.current_key {
            let cf_handle = get_cf_handle::<T>(&self.db)?;
            self.db.delete_cf(cf_handle, key).map_err(|e| {
                create_write_error::<T>(e, DatabaseWriteOperation::Put, key.clone())
            })?;

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
    /// Cache buffer that receives compressed values.
    buf: Vec<u8>,
    _table: std::marker::PhantomData<T>,
}

impl<T: Table> DupCursor<T> {
    pub(crate) fn new(db: Arc<DB>) -> Self {
        Self {
            db,
            table_name: T::NAME,
            current_key: None,
            current_value: None,
            buf: Vec::new(),
            _table: std::marker::PhantomData,
        }
    }

    fn decode_key_value(
        &self,
        key: &[u8],
        value: &[u8],
    ) -> Result<(T::Key, T::Value), DatabaseError> {
        let decoded_key = T::Key::decode(key).map_err(|_| DatabaseError::Decode)?;
        let decoded_value = T::Value::decompress(value).map_err(|_| DatabaseError::Decode)?;
        Ok((decoded_key, decoded_value))
    }
}

impl<T: Table> DbCursorRO<T> for DupCursor<T> {
    fn first(&mut self) -> PairResult<T> {
        let mut iter = self.db.iterator_cf(get_cf_handle::<T>(&self.db)?, rocksdb::IteratorMode::Start);

        if let Some(Ok((key, value))) = iter.next() {
            self.current_key = Some(key.to_vec());
            self.current_value = Some(value.to_vec());
            self.decode_key_value(&key, &value).map(Some)
        } else {
            self.current_key = None;
            self.current_value = None;
            Ok(None)
        }
    }

    fn seek_exact(&mut self, key: T::Key) -> PairResult<T> {
        let encoded_key = key.encode();
        let cf_handle = get_cf_handle::<T>(&self.db)?;

        match self.db.get_cf(cf_handle, &encoded_key) {
            Ok(Some(value)) => {
                self.current_key = Some(encoded_key.as_ref().to_vec());
                self.current_value = Some(value.to_vec());
                self.decode_key_value(encoded_key.as_ref(), &value).map(Some)
            }
            Ok(None) => {
                self.current_key = None;
                self.current_value = None;
                Ok(None)
            }
            Err(e) => Err(rocksdb_error_to_database_error(e)),
        }
    }

    fn seek(&mut self, key: T::Key) -> PairResult<T> {
        let encoded_key = key.encode();
        let mut iter = self.db.iterator_cf(
            get_cf_handle::<T>(&self.db)?,
            rocksdb::IteratorMode::From(encoded_key.as_ref(), rocksdb::Direction::Forward),
        );

        if let Some(Ok((key, value))) = iter.next() {
            self.current_key = Some(key.to_vec());
            self.current_value = Some(value.to_vec());
            self.decode_key_value(&key, &value).map(Some)
        } else {
            self.current_key = None;
            self.current_value = None;
            Ok(None)
        }
    }

    fn next(&mut self) -> PairResult<T> {
        if let Some(ref current_key) = self.current_key {
            let mut iter = self.db.iterator_cf(
                get_cf_handle::<T>(&self.db)?,
                rocksdb::IteratorMode::From(current_key, rocksdb::Direction::Forward),
            );

            // Skip the current key
            iter.next();

            if let Some(Ok((key, value))) = iter.next() {
                self.current_key = Some(key.to_vec());
                self.current_value = Some(value.to_vec());
                self.decode_key_value(&key, &value).map(Some)
            } else {
                self.current_key = None;
                self.current_value = None;
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }

    fn prev(&mut self) -> PairResult<T> {
        if let Some(ref current_key) = self.current_key {
            let mut iter = self.db.iterator_cf(
                get_cf_handle::<T>(&self.db)?,
                rocksdb::IteratorMode::From(current_key, rocksdb::Direction::Reverse),
            );

            // Skip the current key
            iter.next();

            if let Some(Ok((key, value))) = iter.next() {
                self.current_key = Some(key.to_vec());
                self.current_value = Some(value.to_vec());
                self.decode_key_value(&key, &value).map(Some)
            } else {
                self.current_key = None;
                self.current_value = None;
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }

    fn last(&mut self) -> PairResult<T> {
        let mut iter = self.db.iterator_cf(get_cf_handle::<T>(&self.db)?, rocksdb::IteratorMode::End);

        if let Some(Ok((key, value))) = iter.next() {
            self.current_key = Some(key.to_vec());
            self.current_value = Some(value.to_vec());
            self.decode_key_value(&key, &value).map(Some)
        } else {
            self.current_key = None;
            self.current_value = None;
            Ok(None)
        }
    }

    fn current(&mut self) -> PairResult<T> {
        if let (Some(ref key), Some(ref value)) = (&self.current_key, &self.current_value) {
            self.decode_key_value(key, value).map(Some)
        } else {
            Ok(None)
        }
    }

    fn walk(
        &mut self,
        start_key: Option<T::Key>,
    ) -> Result<Walker<'_, T, Self>, DatabaseError> {
        let start: IterPairResult<T> = match start_key {
            Some(key) => self.seek(key).transpose(),
            None => self.first().transpose(),
        };

        Ok(Walker::new(self, start))
    }

    fn walk_range(
        &mut self,
        range: impl std::ops::RangeBounds<T::Key>,
    ) -> Result<reth_db_api::cursor::RangeWalker<'_, T, Self>, DatabaseError> {
        use std::ops::Bound;

        let start_key = match range.start_bound() {
            Bound::Included(key) | Bound::Excluded(key) => Some((*key).clone()),
            Bound::Unbounded => None,
        };

        let end_key = match range.end_bound() {
            Bound::Included(key) | Bound::Excluded(key) => Bound::Included((*key).clone()),
            Bound::Unbounded => Bound::Unbounded,
        };

        let start: IterPairResult<T> = match start_key {
            Some(key) => self.seek(key).transpose(),
            None => self.first().transpose(),
        };

        Ok(reth_db_api::cursor::RangeWalker::new(self, start, end_key))
    }

    fn walk_back(
        &mut self,
        start_key: Option<T::Key>,
    ) -> Result<reth_db_api::cursor::ReverseWalker<'_, T, Self>, DatabaseError> {
        let start: IterPairResult<T> = match start_key {
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

impl<T: DupSort> DbDupCursorRO<T> for DupCursor<T> {
    fn next_dup(&mut self) -> PairResult<T> {
        // For RocksDB, we need to implement duplicate handling
        // This is a simplified implementation that treats it like a regular next
        self.next()
    }

    fn next_no_dup(&mut self) -> PairResult<T> {
        // For RocksDB, we need to skip duplicates
        // This is a simplified implementation
        if let Some(current_key) = &self.current_key {
            let mut iter = self.db.iterator_cf(
                get_cf_handle::<T>(&self.db)?,
                rocksdb::IteratorMode::From(current_key, rocksdb::Direction::Forward),
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
                get_cf_handle::<T>(&self.db)?,
                rocksdb::IteratorMode::From(current_key, rocksdb::Direction::Forward),
            );

            // Skip the current key
            iter.next();

            // Find the next entry with the same key
            while let Some(Ok((key, value))) = iter.next() {
                if key.as_ref() == current_key.as_slice() {
                    self.current_value = Some(value.to_vec());
                    return T::Value::decompress(&value)
                        .map(Some)
                        .map_err(|_| DatabaseError::Decode);
                } else {
                    break;
                }
            }
        }
        Ok(None)
    }

    fn seek_by_key_subkey(
        &mut self,
        key: T::Key,
        subkey: T::SubKey,
    ) -> Result<Option<T::Value>, DatabaseError> {
        // For RocksDB, we need to construct a composite key from key and subkey
        // This is a simplified implementation
        let encoded_key = key.encode();
        let encoded_subkey = subkey.encode();

        // Combine key and subkey (this is table-specific and would need proper implementation)
        let mut composite_key = encoded_key.as_ref().to_vec();
        composite_key.extend_from_slice(encoded_subkey.as_ref());

        let cf_handle = get_cf_handle::<T>(&self.db)?;
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

    fn walk_dup(
        &mut self,
        key: Option<T::Key>,
        subkey: Option<T::SubKey>,
    ) -> Result<reth_db_api::cursor::DupWalker<'_, T, Self>, DatabaseError> {
        let start: IterPairResult<T> = match (key, subkey) {
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
        let cf_handle = get_cf_handle::<T>(&self.db)?;
        let encoded_key = key.encode();
        let value_ref = compress_to_buf_or_ref!(self, value);

        self.db.put_cf(cf_handle, &encoded_key, value_ref.unwrap_or(&self.buf)).map_err(|e| {
            create_write_error::<T>(e, DatabaseWriteOperation::Put, encoded_key.as_ref().to_vec())
        })?;

        // Update current position
        self.current_key = Some(encoded_key.as_ref().to_vec());
        self.current_value = Some(value_ref.unwrap_or(&self.buf).to_vec());

        Ok(())
    }

    fn insert(&mut self, key: T::Key, value: &T::Value) -> Result<(), DatabaseError> {
        let cf_handle = get_cf_handle::<T>(&self.db)?;
        let encoded_key = key.encode();

        // Check if key already exists
        if self
            .db
            .get_cf(cf_handle, &encoded_key)
            .map_err(|e| {
                DatabaseError::Read(DatabaseErrorInfo { message: e.to_string().into(), code: -1 })
            })?
            .is_some()
        {
            return Err(DatabaseError::Write(Box::new(DatabaseWriteError {
                info: DatabaseErrorInfo { message: "Key already exists".into(), code: -1 },
                operation: DatabaseWriteOperation::CursorInsert,
                table_name: T::NAME,
                key: encoded_key.as_ref().to_vec(),
            })));
        }

        let value_ref = compress_to_buf_or_ref!(self, value);
        self.db.put_cf(cf_handle, &encoded_key, value_ref.unwrap_or(&self.buf)).map_err(|e| {
            create_write_error::<T>(
                e,
                DatabaseWriteOperation::CursorInsert,
                encoded_key.as_ref().to_vec(),
            )
        })?;

        // Update current position
        self.current_key = Some(encoded_key.as_ref().to_vec());
        self.current_value = Some(value_ref.unwrap_or(&self.buf).to_vec());

        Ok(())
    }

    fn append(&mut self, key: T::Key, value: &T::Value) -> Result<(), DatabaseError> {
        // For RocksDB, append is the same as upsert
        self.upsert(key, value)
    }

    fn delete_current(&mut self) -> Result<(), DatabaseError> {
        if let Some(ref key) = self.current_key {
            let cf_handle = get_cf_handle::<T>(&self.db)?;
            self.db.delete_cf(cf_handle, key).map_err(|e| {
                create_write_error::<T>(e, DatabaseWriteOperation::Put, key.clone())
            })?;

            self.current_key = None;
            self.current_value = None;
        }

        Ok(())
    }
}

impl<T: DupSort> DbDupCursorRW<T> for DupCursor<T> {
    fn delete_current_duplicates(&mut self) -> Result<(), DatabaseError> {
        if let Some(ref current_key) = self.current_key {
            let cf_handle = get_cf_handle::<T>(&self.db)?;

            // Find all entries with the same key and delete them
            let mut iter = self.db.iterator_cf(
                cf_handle,
                rocksdb::IteratorMode::From(current_key, rocksdb::Direction::Forward),
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
                self.db.delete_cf(cf_handle, &key).map_err(|e| {
                    create_write_error::<T>(e, DatabaseWriteOperation::Put, key.clone())
                })?;
            }

            self.current_key = None;
            self.current_value = None;
        }

        Ok(())
    }

    fn append_dup(&mut self, key: T::Key, value: T::Value) -> Result<(), DatabaseError> {
        let cf_handle = get_cf_handle::<T>(&self.db)?;
        let encoded_key = key.encode();
        let value_ref = compress_to_buf_or_ref!(self, value);

        // For RocksDB, we need to create a unique key for duplicates
        // This is a simplified implementation - in practice, this would need
        // to be table-specific to handle the composite key properly
        let mut composite_key = encoded_key.as_ref().to_vec();
        composite_key.extend_from_slice(value_ref.unwrap_or(&self.buf));

        self.db.put_cf(cf_handle, &composite_key, value_ref.unwrap_or(&self.buf)).map_err(|e| {
            create_write_error::<T>(e, DatabaseWriteOperation::Put, composite_key.clone())
        })?;

        // Update current position
        self.current_key = Some(composite_key);
        self.current_value = Some(value_ref.unwrap_or(&self.buf).to_vec());

        Ok(())
    }
}
