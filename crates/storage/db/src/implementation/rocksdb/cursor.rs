use crate::{
    implementation::rocksdb::{
        create_write_error, get_cf_handle, no_seek_error, rocksdb_error_to_database_error,
    },
    DatabaseError,
};
use reth_db_api::{
    common::{IterPairResult, PairResult, ValueOnlyResult},
    cursor::{DbCursorRO, DbCursorRW, DbDupCursorRO, DbDupCursorRW, Walker},
    table::{Compress, Decode, Decompress, DupSort, Encode, Table},
};
use reth_storage_errors::db::DatabaseWriteOperation;
use rocksdb::DB;
use std::{
    fmt::Debug,
    marker::PhantomData,
    mem,
    ops::{Bound, RangeBounds},
    sync::Arc,
};

/// Transaction kind marker for read-only operations.
#[derive(Debug)]
pub struct RO;

/// Transaction kind marker for read-write operations.
#[derive(Debug)]
pub struct RW;

/// Trait to distinguish between read-only and read-write transaction kinds.
pub trait TransactionKind: Debug + Send + Sync + 'static {
    /// Whether this transaction kind is read-write.
    const IS_READ_WRITE: bool;
}

impl TransactionKind for RO {
    const IS_READ_WRITE: bool = false;
}

impl TransactionKind for RW {
    const IS_READ_WRITE: bool = true;
}

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

/// RocksDB cursor with RawIterator caching for performance.
pub struct Cursor<K: TransactionKind, T: Table> {
    db: Arc<DB>,
    table_name: &'static str,
    /// Cached RawIterator for performance - supports bidirectional operations
    cached_iterator: Option<rocksdb::DBRawIterator<'static>>,
    /// Current key position for current() interface
    current_key: Option<Vec<u8>>,
    /// Cache buffer that receives compressed values.
    buf: Vec<u8>,
    _phantom: PhantomData<(K, T)>,
}

/// DupSort key utilities - handles fixed-length key composite keys (geth-style)
impl<K: TransactionKind, T: DupSort> Cursor<K, T> {
    /// Encode DupSort composite key: key + subkey
    fn encode_dupsort_key(key: &[u8], subkey: &[u8]) -> Vec<u8> {
        let mut composite = Vec::with_capacity(key.len() + subkey.len());
        composite.extend_from_slice(key);
        composite.extend_from_slice(subkey);
        composite
    }

    /// Decode DupSort composite key: split based on fixed key length
    fn decode_dupsort_key(composite: &[u8]) -> Result<(&[u8], &[u8]), DatabaseError> {
        if composite.len() < Self::KEY_LENGTH {
            return Err(DatabaseError::Decode);
        }
        Ok((&composite[..Self::KEY_LENGTH], &composite[Self::KEY_LENGTH..]))
    }
}

impl<K: TransactionKind, T: Table> Cursor<K, T> {
    const KEY_LENGTH: usize = mem::size_of::<<T::Key as Encode>::Encoded>();

    pub(crate) fn new(db: Arc<DB>) -> Self {
        Self {
            db,
            table_name: T::NAME,
            cached_iterator: None,
            current_key: None,
            buf: Vec::new(),
            _phantom: PhantomData,
        }
    }

    fn decode_key_value(key: &[u8], value: &[u8]) -> Result<(T::Key, T::Value), DatabaseError> {
        let decoded_key = if T::DUPSORT {
            // For DupSort tables, key is composite: main_key + subkey
            // Extract only the main key part using fixed length
            T::Key::decode(&key[..Self::KEY_LENGTH]).map_err(|_| DatabaseError::Decode)?
        } else {
            // For regular tables, decode normally
            T::Key::decode(key).map_err(|_| DatabaseError::Decode)?
        };

        let decoded_value = T::Value::decompress(value).map_err(|_| DatabaseError::Decode)?;
        Ok((decoded_key, decoded_value))
    }

    /// Create and cache RawIterator
    fn create_raw_iterator(&mut self) -> Result<(), DatabaseError> {
        if self.cached_iterator.is_some() {
            return Ok(());
        }
        let cf_handle = get_cf_handle::<T>(&self.db)?;
        let iter = self.db.raw_iterator_cf_opt(cf_handle, rocksdb::ReadOptions::default());

        // SAFETY: We transmute the iterator to have a 'static lifetime
        // This is safe because we manage the iterator's lifetime within this cursor
        let static_iter: rocksdb::DBRawIterator<'static> = unsafe { std::mem::transmute(iter) };
        self.cached_iterator = Some(static_iter);
        Ok(())
    }

    /// Get mutable reference to the cached RawIterator
    fn get_raw_iterator_mut(&mut self) -> Option<&mut rocksdb::DBRawIterator<'static>> {
        self.cached_iterator.as_mut()
    }
}

impl<K: TransactionKind, T: Table> DbCursorRO<T> for Cursor<K, T> {
    fn first(&mut self) -> PairResult<T> {
        self.create_raw_iterator()?;
        if let Some(iter) = self.get_raw_iterator_mut() {
            iter.seek_to_first();
            if iter.valid() {
                if let (Some(key), Some(value)) = (iter.key(), iter.value()) {
                    let result = Self::decode_key_value(key, value).map(Some);
                    self.current_key = Some(key.to_vec());
                    return result;
                }
            }
        }
        self.current_key = None;
        Ok(None)
    }

    fn seek_exact(&mut self, key: T::Key) -> PairResult<T> {
        self.cached_iterator.take();
        let encoded_key = key.encode();
        let cf_handle = get_cf_handle::<T>(&self.db)?;

        match self.db.get_cf(cf_handle, &encoded_key) {
            Ok(Some(value)) => {
                let result = Self::decode_key_value(encoded_key.as_ref(), &value).map(Some);
                self.current_key = Some(encoded_key.into());
                result
            }
            Ok(None) => {
                self.current_key = None;
                Ok(None)
            }
            Err(e) => Err(rocksdb_error_to_database_error(e)),
        }
    }

    fn seek(&mut self, key: T::Key) -> PairResult<T> {
        let encoded_key = key.encode();
        self.create_raw_iterator()?;
        if let Some(iter) = self.get_raw_iterator_mut() {
            iter.seek(encoded_key.as_ref());
            if iter.valid() {
                if let (Some(key), Some(value)) = (iter.key(), iter.value()) {
                    let result = Self::decode_key_value(key, value).map(Some);
                    self.current_key = Some(key.to_vec());
                    return result;
                }
            }
        }
        self.current_key = None;
        Ok(None)
    }

    fn next(&mut self) -> PairResult<T> {
        if let Some(iter) = self.get_raw_iterator_mut() {
            iter.next();
            if iter.valid() {
                if let (Some(key), Some(value)) = (iter.key(), iter.value()) {
                    let result = Self::decode_key_value(key, value).map(Some);
                    self.current_key = Some(key.to_vec());
                    return result;
                }
            }
        } else {
            return Err(no_seek_error::<T>());
        }
        self.current_key = None;
        Ok(None)
    }

    fn prev(&mut self) -> PairResult<T> {
        if let Some(iter) = self.get_raw_iterator_mut() {
            iter.prev();
            if iter.valid() {
                if let (Some(key), Some(value)) = (iter.key(), iter.value()) {
                    let result = Self::decode_key_value(key, value).map(Some);
                    self.current_key = Some(key.to_vec());
                    return result;
                }
            }
        } else {
            return Err(no_seek_error::<T>());
        }
        self.current_key = None;
        Ok(None)
    }

    fn last(&mut self) -> PairResult<T> {
        self.create_raw_iterator()?;
        if let Some(iter) = self.get_raw_iterator_mut() {
            iter.seek_to_last();
            if iter.valid() {
                if let (Some(key), Some(value)) = (iter.key(), iter.value()) {
                    let result = Self::decode_key_value(key, value).map(Some);
                    self.current_key = Some(key.to_vec());
                    return result;
                }
            }
        }
        self.current_key = None;
        Ok(None)
    }

    fn current(&mut self) -> PairResult<T> {
        if let Some(ref current_key) = self.current_key {
            // Re-query the value from database
            let cf_handle = get_cf_handle::<T>(&self.db)?;
            if let Some(value) =
                self.db.get_cf(cf_handle, current_key).map_err(rocksdb_error_to_database_error)?
            {
                Self::decode_key_value(current_key, &value).map(Some)
            } else {
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }

    fn walk(&mut self, start_key: Option<T::Key>) -> Result<Walker<'_, T, Self>, DatabaseError> {
        let start = match start_key {
            Some(key) => self.seek(key).transpose(),
            None => self.first().transpose(),
        };
        Ok(Walker::new(self, start))
    }

    fn walk_range(
        &mut self,
        range: impl RangeBounds<T::Key>,
    ) -> Result<reth_db_api::cursor::RangeWalker<'_, T, Self>, DatabaseError> {
        let start_key = match range.start_bound() {
            Bound::Included(key) => Some(key.clone()),
            Bound::Excluded(_key) => {
                unreachable!("Rust doesn't allow for Bound::Excluded in starting bounds");
            }
            Bound::Unbounded => None,
        };
        let start = match start_key {
            Some(key) => self.seek(key).transpose(),
            None => self.first().transpose(),
        };

        Ok(reth_db_api::cursor::RangeWalker::new(self, start, range.end_bound().cloned()))
    }

    fn walk_back(
        &mut self,
        start_key: Option<T::Key>,
    ) -> Result<reth_db_api::cursor::ReverseWalker<'_, T, Self>, DatabaseError> {
        let start: IterPairResult<T> = match start_key {
            Some(key) => {
                self.seek(key)?;
                self.current().transpose()
            }
            None => self.last().transpose(),
        };

        Ok(reth_db_api::cursor::ReverseWalker::new(self, start))
    }
}

impl<T: Table> DbCursorRW<T> for Cursor<RW, T> {
    fn upsert(&mut self, key: T::Key, value: &T::Value) -> Result<(), DatabaseError> {
        self.current_key.take();
        let cf_handle = get_cf_handle::<T>(&self.db)?;
        let encoded_key = key.encode();
        let value_ref = compress_to_buf_or_ref!(self, value);

        self.db.put_cf(cf_handle, &encoded_key, value_ref.unwrap_or(&self.buf)).map_err(|e| {
            create_write_error::<T>(e, DatabaseWriteOperation::Put, encoded_key.into())
        })?;
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
        }
        Ok(())
    }
}

impl<K: TransactionKind, T: DupSort> DbDupCursorRO<T> for Cursor<K, T> {
    fn next_dup(&mut self) -> PairResult<T> {
        // Get current main key to check if next entry is still a duplicate
        let current_main_key = if let Some(ref current_key) = self.current_key {
            let (main_key, _) = Self::decode_dupsort_key(current_key)?;
            main_key.to_vec() // Only clone when we need to store it
        } else {
            // No current key, just get the first entry
            return self.first();
        };

        // Use base next() method and check if it's still the same main key
        if let Some((key, value)) = self.next()? {
            let returned_key_encoded = key.clone().encode();
            if returned_key_encoded.as_ref() == current_main_key {
                Ok(Some((key, value)))
            } else {
                // Different main key, no more duplicates
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }

    fn next_no_dup(&mut self) -> PairResult<T> {
        // Get the current key to skip all its duplicates
        let mut next_key = if let Some(current_key) = &self.current_key {
            let (main_key, _) = Self::decode_dupsort_key(current_key)?;
            main_key.to_vec() // Only clone when we need to store it
        } else {
            // No current key, just get the first entry
            return self.first();
        };
        
        // Increment the key to find the next different key
        let mut carry = true;
        for byte in next_key.iter_mut().rev() {
            if carry {
                if *byte == u8::MAX {
                    *byte = 0;
                } else {
                    *byte += 1;
                    carry = false;
                    break;
                }
            }
        }  
        if carry {
            // Overflow: no next key possible
            return Ok(None);
        }
        
        // Seek to the incremented key
        self.create_raw_iterator()?;
        if let Some(iter) = self.get_raw_iterator_mut() {
            iter.seek(&next_key);
            if iter.valid() {
                if let (Some(key), Some(value)) = (iter.key(), iter.value()) {
                    let result = Self::decode_key_value(key, value).map(Some);
                    self.current_key = Some(key.to_vec());
                    return result;
                }
            }
        }     
        Ok(None)
    }

    fn next_dup_val(&mut self) -> ValueOnlyResult<T> {
        self.next_dup().map(|result| result.map(|(_, value)| value))
    }

    fn seek_by_key_subkey(&mut self, key: T::Key, subkey: T::SubKey) -> ValueOnlyResult<T> {
        let composite_key =
            Self::encode_dupsort_key(key.encode().as_ref(), subkey.encode().as_ref());

        self.cached_iterator.take();
        let cf_handle = get_cf_handle::<T>(&self.db)?;
        match self.db.get_cf(cf_handle, &composite_key) {
            Ok(Some(value)) => {
                let result = Self::decode_key_value(composite_key.as_ref(), &value).map(Some);
                self.current_key = Some(composite_key);
                result.map(|result| result.map(|(_, value)| value))
            }
            Ok(None) => {
                self.current_key = None;
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
        let start = match (key, subkey) {
            (Some(_key), Some(_subkey)) => {
                unimplemented!("DupSort walk_dup(Some, Some)")
            }
            (Some(key), None) => {
                // Seek to first entry of this key
                self.seek(key).transpose()
            }
            (None, Some(_subkey)) => {
                unimplemented!("DupSort walk_dup(None, Some)")
            }
            (None, None) => {
                self.first().transpose()
            }
        };

        Ok(reth_db_api::cursor::DupWalker { cursor: self, start })
    }
}

impl<T: DupSort> DbDupCursorRW<T> for Cursor<RW, T> {
    fn delete_current_duplicates(&mut self) -> Result<(), DatabaseError> {
        if let Some(ref current_composite_key) = self.current_key.take() {
            let (main_key, _) = Self::decode_dupsort_key(current_composite_key)?;
            // main_key is used for comparison in the loop below

            // Find and delete all entries with the same main key
            let cf_handle = get_cf_handle::<T>(&self.db)?;

            // Create iterator to find all duplicates
            // Use the current composite key as starting point to find all duplicates
            let iter = self.db.iterator_cf(
                cf_handle,
                rocksdb::IteratorMode::From(current_composite_key, rocksdb::Direction::Forward),
            );
            let mut keys_to_delete = Vec::new();

            for result in iter {
                if let Ok((composite_key, _)) = result {
                    let (found_main_key, _) = Self::decode_dupsort_key(&composite_key)?;
                    if found_main_key == main_key {
                        keys_to_delete.push(composite_key.to_vec());
                    } else {
                        // Different key, stop
                        break;
                    }
                }
            }

            // Delete all found keys
            for key_to_delete in keys_to_delete {
                self.db.delete_cf(cf_handle, &key_to_delete).map_err(|e| {
                    create_write_error::<T>(e, DatabaseWriteOperation::Put, key_to_delete)
                })?;
            }
        }
        Ok(())
    }

    fn append_dup(&mut self, key: T::Key, value: T::Value) -> Result<(), DatabaseError> {
        let cf_handle = get_cf_handle::<T>(&self.db)?;
        let encoded_key = key.encode();

        // For DupSort tables, we need to extract the subkey from the value
        // First compress the value and use it as subkey
        let value_ref = compress_to_buf_or_ref!(self, &value);
        let compressed_value = value_ref.unwrap_or(&self.buf);

        // Use compressed value as subkey for sorting
        let composite_key = Self::encode_dupsort_key(encoded_key.as_ref(), compressed_value);

        self.db.put_cf(cf_handle, &composite_key, compressed_value).map_err(|e| {
            create_write_error::<T>(
                e,
                DatabaseWriteOperation::CursorAppendDup,
                composite_key.clone(),
            )
        })?;

        // Update current position
        self.current_key = Some(composite_key);
        Ok(())
    }
}
