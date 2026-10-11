use crate::{
    table::TableImporter,
    transaction::{DbTx, DbTxMut},
    DatabaseError,
};
use alloy_primitives::B256;
use std::{any::Any, fmt::Debug, sync::Arc};

/// Complete persisted block and exclusive transaction bound of an RPC read view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RpcReadViewBounds {
    /// Last complete persisted block.
    pub block_number: u64,
    /// Canonical hash at `block_number`.
    pub block_hash: B256,
    /// First transaction number outside this view.
    pub next_tx_num: u64,
}

/// Keeps destructive storage maintenance from invalidating an RPC read view.
pub trait RpcReadLease: Any + Debug + Send + Sync {
    /// Backend-specific lease identity, used to reject a lease from another database.
    fn as_any(&self) -> &dyn Any;
}

/// Closes RPC admission until maintenance has published a complete replacement view.
pub trait RpcMaintenanceGuard: Send {
    /// Maintenance and replacement publication completed successfully.
    fn complete(&mut self);
}

impl RpcMaintenanceGuard for () {
    fn complete(&mut self) {}
}

/// Marks a coordinated write range complete. Dropping an unfinished guard tells backends that
/// may have committed part of the range not to serve a mixed read view.
pub trait ConsistentWriteGuard {
    /// The complete logical write range has been committed successfully.
    fn complete(&mut self);

    /// Recovery has repaired any previously interrupted range.
    fn recovered(&mut self) {
        self.complete();
    }
}

impl ConsistentWriteGuard for () {
    fn complete(&mut self) {}
}

/// Main Database trait that can open read-only and read-write transactions.
///
/// Sealed trait which cannot be implemented by 3rd parties, exposed only for consumption.
pub trait Database: Send + Sync + Debug {
    /// Read-Only database transaction
    type TX: DbTx + Send + Sync + Debug + 'static;
    /// Read-Write database transaction
    type TXMut: DbTxMut + DbTx + TableImporter + Send + Sync + Debug + 'static;

    /// Create read only transaction.
    #[track_caller]
    fn tx(&self) -> Result<Self::TX, DatabaseError>;

    /// Create a read-only transaction that observes subsequent database commits.
    ///
    /// Backends without a separate snapshot mode may use their normal read transaction.
    #[track_caller]
    fn tx_live(&self) -> Result<Self::TX, DatabaseError> {
        self.tx()
    }

    /// Create read write transaction only possible if database is open with write access.
    #[track_caller]
    fn tx_mut(&self) -> Result<Self::TXMut, DatabaseError>;

    /// Coordinate all commits of one logical state/trie update with read snapshot creation.
    fn consistent_write(&self) -> Box<dyn ConsistentWriteGuard + '_> {
        Box::new(())
    }

    /// Acquire before capturing an RPC's in-memory chain, and retain through its storage reads.
    fn rpc_read_lease(&self) -> Result<Option<Arc<dyn RpcReadLease>>, DatabaseError> {
        Ok(None)
    }

    /// Open the most recently published complete view without waiting for ordinary writes.
    fn tx_rpc(&self, _lease: Option<Arc<dyn RpcReadLease>>) -> Result<Self::TX, DatabaseError> {
        self.tx()
    }

    /// Publish a complete view while holding the backend's coordinated write guard.
    ///
    /// The caller must verify all stores and static-file bounds before publication.
    /// `verified` is false when startup trie validation has not established a complete view.
    fn publish_rpc_view(
        &self,
        _bounds: RpcReadViewBounds,
        _verified: bool,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }

    /// Whether this complete height needs a one-time check for legacy partial trie commits.
    fn rpc_read_view_requires_validation(&self, _block_number: u64) -> bool {
        false
    }

    /// Closes RPC admission after a publication-only failure without invalidating execution reads.
    /// A later complete publication may retry validation and reopen admission.
    fn rpc_publication_failed(&self) {}

    /// Close admission and wait for existing RPC readers before destructive maintenance.
    ///
    /// Acquire before `consistent_write` to avoid waiting for readers behind the write barrier.
    /// If `wait` is false, skip maintenance when readers or another maintenance guard exist.
    fn rpc_maintenance(&self, _wait: bool) -> Option<Box<dyn RpcMaintenanceGuard + '_>> {
        Some(Box::new(()))
    }

    /// Takes a function and passes a read-only transaction into it, making sure it's closed in the
    /// end of the execution.
    fn view<T, F>(&self, f: F) -> Result<T, DatabaseError>
    where
        F: FnOnce(&Self::TX) -> T,
    {
        let tx = self.tx()?;

        let res = f(&tx);
        tx.commit()?;

        Ok(res)
    }

    /// Takes a function and passes a write-read transaction into it, making sure it's committed in
    /// the end of the execution.
    fn update<T, F>(&self, f: F) -> Result<T, DatabaseError>
    where
        F: FnOnce(&Self::TXMut) -> T,
    {
        let tx = self.tx_mut()?;

        let res = f(&tx);
        tx.commit()?;

        Ok(res)
    }
}

impl<DB: Database> Database for Arc<DB> {
    type TX = <DB as Database>::TX;
    type TXMut = <DB as Database>::TXMut;

    fn tx(&self) -> Result<Self::TX, DatabaseError> {
        <DB as Database>::tx(self)
    }

    fn tx_live(&self) -> Result<Self::TX, DatabaseError> {
        <DB as Database>::tx_live(self)
    }

    fn tx_mut(&self) -> Result<Self::TXMut, DatabaseError> {
        <DB as Database>::tx_mut(self)
    }

    fn consistent_write(&self) -> Box<dyn ConsistentWriteGuard + '_> {
        <DB as Database>::consistent_write(self)
    }

    fn rpc_read_lease(&self) -> Result<Option<Arc<dyn RpcReadLease>>, DatabaseError> {
        <DB as Database>::rpc_read_lease(self)
    }

    fn tx_rpc(&self, lease: Option<Arc<dyn RpcReadLease>>) -> Result<Self::TX, DatabaseError> {
        <DB as Database>::tx_rpc(self, lease)
    }

    fn publish_rpc_view(
        &self,
        bounds: RpcReadViewBounds,
        verified: bool,
    ) -> Result<(), DatabaseError> {
        <DB as Database>::publish_rpc_view(self, bounds, verified)
    }

    fn rpc_read_view_requires_validation(&self, block_number: u64) -> bool {
        <DB as Database>::rpc_read_view_requires_validation(self, block_number)
    }

    fn rpc_publication_failed(&self) {
        <DB as Database>::rpc_publication_failed(self)
    }

    fn rpc_maintenance(&self, wait: bool) -> Option<Box<dyn RpcMaintenanceGuard + '_>> {
        <DB as Database>::rpc_maintenance(self, wait)
    }
}

impl<DB: Database> Database for &DB {
    type TX = <DB as Database>::TX;
    type TXMut = <DB as Database>::TXMut;

    fn tx(&self) -> Result<Self::TX, DatabaseError> {
        <DB as Database>::tx(self)
    }

    fn tx_live(&self) -> Result<Self::TX, DatabaseError> {
        <DB as Database>::tx_live(self)
    }

    fn tx_mut(&self) -> Result<Self::TXMut, DatabaseError> {
        <DB as Database>::tx_mut(self)
    }

    fn consistent_write(&self) -> Box<dyn ConsistentWriteGuard + '_> {
        <DB as Database>::consistent_write(self)
    }

    fn rpc_read_lease(&self) -> Result<Option<Arc<dyn RpcReadLease>>, DatabaseError> {
        <DB as Database>::rpc_read_lease(self)
    }

    fn tx_rpc(&self, lease: Option<Arc<dyn RpcReadLease>>) -> Result<Self::TX, DatabaseError> {
        <DB as Database>::tx_rpc(self, lease)
    }

    fn publish_rpc_view(
        &self,
        bounds: RpcReadViewBounds,
        verified: bool,
    ) -> Result<(), DatabaseError> {
        <DB as Database>::publish_rpc_view(self, bounds, verified)
    }

    fn rpc_read_view_requires_validation(&self, block_number: u64) -> bool {
        <DB as Database>::rpc_read_view_requires_validation(self, block_number)
    }

    fn rpc_publication_failed(&self) {
        <DB as Database>::rpc_publication_failed(self)
    }

    fn rpc_maintenance(&self, wait: bool) -> Option<Box<dyn RpcMaintenanceGuard + '_>> {
        <DB as Database>::rpc_maintenance(self, wait)
    }
}
