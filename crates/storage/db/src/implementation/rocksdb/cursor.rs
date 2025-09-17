use crate::{
    implementation::rocksdb::{create_write_error, get_cf_handle, rocksdb_error_to_database_error},
    DatabaseError,
};
use reth_db_api::{
    common::{IterPairResult, PairResult, ValueOnlyResult},
    cursor::{DbCursorRO, DbCursorRW, DbDupCursorRO, DbDupCursorRW, Walker},
    table::{Compress, Decode, Decompress, DupSort, Encode, Table},
};
use reth_storage_errors::db::{DatabaseWriteOperation, DatabaseErrorInfo};
use rocksdb::DB;
use std::{fmt::Debug, marker::PhantomData, sync::Arc, cell::UnsafeCell};

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

/// Unsafe wrapper for DBIterator to bypass Send + Sync requirements
/// 
/// # Safety
/// This wrapper assumes single-threaded usage of the cursor.
/// The caller must ensure no concurrent access to the iterator.
struct UnsafeIterator {
    inner: UnsafeCell<Option<rocksdb::DBIterator<'static>>>,
}

impl UnsafeIterator {
    fn new() -> Self {
        Self {
            inner: UnsafeCell::new(None),
        }
    }

    unsafe fn set(&self, iter: rocksdb::DBIterator<'static>) {
        *self.inner.get() = Some(iter);
    }

    unsafe fn get_mut(&self) -> Option<&mut rocksdb::DBIterator<'static>> {
        (*self.inner.get()).as_mut()
    }

    unsafe fn take(&self) -> Option<rocksdb::DBIterator<'static>> {
        (*self.inner.get()).take()
    }

    fn ready(&self) -> bool {
        unsafe { (*self.inner.get()).is_some() }
    }
}

// SAFETY: We guarantee single-threaded usage of cursor
unsafe impl Send for UnsafeIterator {}
unsafe impl Sync for UnsafeIterator {}

impl Debug for UnsafeIterator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnsafeIterator").finish()
    }
}

/// RocksDB cursor with unsafe iterator caching for performance.
pub struct Cursor<K: TransactionKind, T: Table> {
    db: Arc<DB>,
    table_name: &'static str,
    /// Cached iterator for performance
    cached_iterator: UnsafeIterator,
    /// Current iterator direction  
    iterator_direction: rocksdb::Direction,
    /// Current key position for current() interface
    current_key: Option<Vec<u8>>,
    /// Cache buffer that receives compressed values.
    buf: Vec<u8>,
    _phantom: PhantomData<(K, T)>,
}

impl<K: TransactionKind, T: Table> Cursor<K, T> {
    pub(crate) fn new(db: Arc<DB>) -> Self {
        Self {
            db,
            table_name: T::NAME,
            cached_iterator: UnsafeIterator::new(),
            iterator_direction: rocksdb::Direction::Forward,
            current_key: None,
            buf: Vec::new(),
            _phantom: PhantomData,
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

    /// Create a new iterator with the specified mode
    /// 
    /// # Safety
    /// This method extends the lifetime of the iterator to 'static using unsafe transmute.
    /// The caller must ensure the DB remains valid for the iterator's actual usage.
    unsafe fn create_iterator(&self, mode: rocksdb::IteratorMode<'_>) -> Result<(), DatabaseError> {
        let cf_handle = get_cf_handle::<T>(&self.db)?;
        let iter = self.db.iterator_cf(cf_handle, mode);
        // SAFETY: We extend lifetime to 'static, but the actual usage is bounded by self
        self.cached_iterator.set(std::mem::transmute(iter));
        Ok(())
    }

    /// Get mutable reference to the cached iterator
    unsafe fn get_iterator_mut(&mut self) -> Option<&mut rocksdb::DBIterator<'static>> {
        self.cached_iterator.get_mut()
    }

    fn set_direction(&mut self, iterator_direction: rocksdb::Direction) {
        assert!(self.cached_iterator.ready());
        self.iterator_direction = iterator_direction;
    }
}

impl<K: TransactionKind, T: Table> DbCursorRO<T> for Cursor<K, T> {
    fn first(&mut self) -> PairResult<T> {
        unsafe {
            self.create_iterator(rocksdb::IteratorMode::Start)?;
            self.set_direction(rocksdb::Direction::Forward);
            if let Some(iter) = self.get_iterator_mut() {
                if let Some(Ok((key, value))) = iter.next() {
                    self.current_key = Some(key.to_vec());
                    return self.decode_key_value(&key, &value).map(Some);
                } else {
                    self.current_key = None;
                    return Ok(None);
                }
            }
        }
        Ok(None)
    }

    fn seek_exact(&mut self, key: T::Key) -> PairResult<T> {
        unsafe {
            self.cached_iterator.take();
            self.iterator_direction = rocksdb::Direction::Forward;
        }
        let encoded_key = key.encode();
        let cf_handle = get_cf_handle::<T>(&self.db)?;
        // Used for next();
        self.current_key = Some(encoded_key.as_ref().to_vec());

        match self.db.get_cf(cf_handle, &encoded_key) {
            Ok(Some(value)) => {
                self.decode_key_value(encoded_key.as_ref(), &value).map(Some)
            }
            Ok(None) => {
                Ok(None)
            }
            Err(e) => Err(rocksdb_error_to_database_error(e)),
        }
    }

    fn seek(&mut self, key: T::Key) -> PairResult<T> {
        self.current_key = None;
        let encoded_key = key.encode();
        unsafe {
            self.create_iterator(rocksdb::IteratorMode::From(encoded_key.as_ref(), rocksdb::Direction::Forward))?;
        }
        self.set_direction(rocksdb::Direction::Forward);
        self.next()
    }

    fn next(&mut self) -> PairResult<T> {
        match self.iterator_direction {
            rocksdb::Direction::Forward => unsafe {
                if let Some(iter) = self.get_iterator_mut() {
                    if let Some(Ok((key, value))) = iter.next() {
                        let new_key = key.to_vec();
                        if let Some(origin_key) = self.current_key.take() {
                            if origin_key == new_key {
                                return self.next();
                            }
                        }
                        self.current_key = Some(new_key);
                        return self.decode_key_value(&key, &value).map(Some);
                    } else {
                        self.current_key = None;
                        return Ok(None);
                    }
                } else if let Some(current_key) = &self.current_key {
                    self.create_iterator(rocksdb::IteratorMode::From(current_key, rocksdb::Direction::Forward))?;
                    self.set_direction(rocksdb::Direction::Forward);
                    return self.next();
                }
            },
            rocksdb::Direction::Reverse => unsafe {
                if let Some(current_key) = &self.current_key {
                    self.create_iterator(rocksdb::IteratorMode::From(current_key, rocksdb::Direction::Forward))?;
                    self.set_direction(rocksdb::Direction::Forward);
                    return self.next();
                }
            },
        }
        Ok(None)
    }

    fn prev(&mut self) -> PairResult<T> {
        match self.iterator_direction {
            rocksdb::Direction::Forward => unsafe {
                if let Some(current_key) = &self.current_key {
                    self.create_iterator(rocksdb::IteratorMode::From(current_key, rocksdb::Direction::Reverse))?;
                    self.set_direction(rocksdb::Direction::Reverse);
                    return self.prev();
                }
            },
            rocksdb::Direction::Reverse => unsafe {
                if let Some(iter) = self.get_iterator_mut() {
                    if let Some(Ok((key, value))) = iter.next() {
                        self.current_key = Some(key.to_vec());
                        return self.decode_key_value(&key, &value).map(Some);
                    } else {
                        self.current_key = None;
                        return Ok(None);
                    }
                } else if let Some(current_key) = &self.current_key {
                    self.create_iterator(rocksdb::IteratorMode::From(current_key, rocksdb::Direction::Reverse))?;
                    self.set_direction(rocksdb::Direction::Reverse);
                    return self.prev();
                }
            },
        }
        Ok(None)
    }

    fn last(&mut self) -> PairResult<T> {
        unsafe {
            self.create_iterator(rocksdb::IteratorMode::End)?;
            self.set_direction(rocksdb::Direction::Forward);
            if let Some(iter) = self.get_iterator_mut() {
                if let Some(Ok((key, value))) = iter.next() {
                    self.current_key = Some(key.to_vec());
                    return self.decode_key_value(&key, &value).map(Some);
                } else {
                    self.current_key = None;
                    return Ok(None);
                }
            }
        }
        Ok(None)
    }

    fn current(&mut self) -> PairResult<T> {
        if let Some(ref current_key) = self.current_key {
            // Re-query the value from database
            let cf_handle = get_cf_handle::<T>(&self.db)?;
            if let Some(value) = self.db.get_cf(cf_handle, current_key).map_err(rocksdb_error_to_database_error)? {
                self.decode_key_value(current_key, &value).map(Some)
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

impl<T: Table> DbCursorRW<T> for Cursor<RW, T> {
    fn upsert(&mut self, key: T::Key, value: &T::Value) -> Result<(), DatabaseError> {
        let cf_handle = get_cf_handle::<T>(&self.db)?;
        let encoded_key = key.encode();
        let value_ref = compress_to_buf_or_ref!(self, value);

        self.db.put_cf(cf_handle, &encoded_key, value_ref.unwrap_or(&self.buf)).map_err(|e| {
            create_write_error::<T>(e, DatabaseWriteOperation::Put, encoded_key.as_ref().to_vec())
        })?;

        // Update current position
        self.current_key = Some(encoded_key.as_ref().to_vec());

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

// DupSort implementations
impl<K: TransactionKind, T: DupSort> DbDupCursorRO<T> for Cursor<K, T> {
    fn next_dup(&mut self) -> PairResult<T> {
        self.next()
    }

    fn next_no_dup(&mut self) -> PairResult<T> {
        self.next()
    }

    fn next_dup_val(&mut self) -> ValueOnlyResult<T> {
        if let Some((_, value)) = self.next()? {
            Ok(Some(value))
        } else {
            Ok(None)
        }
    }

    fn seek_by_key_subkey(
        &mut self,
        key: T::Key,
        _subkey: T::SubKey,
    ) -> ValueOnlyResult<T> {
        if let Some((_, value)) = self.seek_exact(key)? {
            Ok(Some(value))
        } else {
            Ok(None)
        }
    }

    fn walk_dup(
        &mut self,
        key: Option<T::Key>,
        _subkey: Option<T::SubKey>,
    ) -> Result<reth_db_api::cursor::DupWalker<'_, T, Self>, DatabaseError> {
        let start = if let Some(key) = key {
            self.seek_exact(key).transpose()
        } else {
            self.first().transpose()
        };

        Ok(reth_db_api::cursor::DupWalker { cursor: self, start })
    }
}

impl<T: DupSort> DbDupCursorRW<T> for Cursor<RW, T> {
    fn delete_current_duplicates(&mut self) -> Result<(), DatabaseError> {
        // Simplified implementation for RocksDB
        self.delete_current()
    }

    fn append_dup(&mut self, key: T::Key, value: T::Value) -> Result<(), DatabaseError> {
        // Simplified implementation for RocksDB
        self.upsert(key, &value)
    }
}
