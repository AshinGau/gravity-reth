//! RocksDB implementation for the database.

use crate::{DatabaseError, TableSet};
use reth_db_api::{
    database_metrics::DatabaseMetrics, models::ClientVersion, table::Table, DatabaseWriteOperation,
    Tables,
};
use reth_storage_errors::db::{DatabaseErrorInfo, DatabaseWriteError, LogLevel};
use rocksdb::{Options, TransactionDB, TransactionDBOptions};
use std::{path::Path, sync::Arc};

pub mod cursor;
pub mod tx;

/// Database environment kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseEnvKind {
    /// Read-write database.
    RW,
    /// Read-only database.
    RO,
}

impl DatabaseEnvKind {
    /// Returns `true` if the database is read-write.
    pub const fn is_rw(self) -> bool {
        matches!(self, Self::RW)
    }
}

/// Database arguments for RocksDB.
#[derive(Debug, Clone)]
pub struct DatabaseArguments {
    /// Client version.
    pub client_version: ClientVersion,
    /// Log level.
    pub log_level: Option<LogLevel>,
    /// Maximum database size.
    pub max_size: Option<usize>,
}

impl DatabaseArguments {
    /// Creates a new instance of [`DatabaseArguments`].
    pub fn new(client_version: ClientVersion) -> Self {
        Self { client_version, log_level: None, max_size: None }
    }

    /// Set the log level.
    pub fn log_level(mut self, log_level: Option<LogLevel>) -> Self {
        self.log_level = log_level;
        self
    }

    /// Set the log level (alias for log_level for consistency with MDBX).
    pub fn with_log_level(mut self, log_level: Option<LogLevel>) -> Self {
        self.log_level = log_level;
        self
    }

    /// Set the maximum database size.
    pub fn max_size(mut self, max_size: Option<usize>) -> Self {
        self.max_size = max_size;
        self
    }

    /// Set the maximum database size (alias for max_size for consistency with MDBX).
    pub fn with_geometry_max_size(mut self, max_size: Option<usize>) -> Self {
        self.max_size = max_size;
        self
    }

    /// Get the client version.
    pub fn client_version(&self) -> &ClientVersion {
        &self.client_version
    }
}

impl Default for DatabaseArguments {
    fn default() -> Self {
        Self::new(ClientVersion::default())
    }
}

/// RocksDB database environment.
pub struct DatabaseEnv {
    /// Inner RocksDB transaction database.
    pub(crate) inner: Arc<TransactionDB>,
    /// Database environment kind (read-only or read-write).
    kind: DatabaseEnvKind,
}

impl std::fmt::Debug for DatabaseEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseEnv")
            .field("inner", &"<TransactionDB>")
            .field("kind", &self.kind)
            .finish()
    }
}

impl DatabaseEnv {
    /// Opens the database at the specified path.
    pub fn open(
        path: &Path,
        kind: DatabaseEnvKind,
        _args: DatabaseArguments,
    ) -> Result<Self, DatabaseError> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);

        // Get all required table names
        let required_tables: Vec<String> = Tables::tables().map(|t| t.name().to_string()).collect();

        // Use TransactionDB for proper transaction support
        let txn_db_opts = TransactionDBOptions::default();
        let db =
            TransactionDB::open_cf(&opts, &txn_db_opts, path, &required_tables).map_err(|e| {
                DatabaseError::Other(format!("Failed to open RocksDB TransactionDB: {}", e))
            })?;

        Ok(Self { inner: Arc::new(db), kind })
    }

    /// Returns `true` if the database is read-only.
    pub fn is_read_only(&self) -> bool {
        matches!(self.kind, DatabaseEnvKind::RO)
    }

    /// Creates tables for the given table set.
    pub fn create_tables(&self) -> Result<(), DatabaseError> {
        self.create_tables_for::<Tables>()
    }

    /// Creates tables for the given table set.
    pub fn create_tables_for<TS: TableSet>(&self) -> Result<(), DatabaseError> {
        Ok(())
    }

    /// Records the client version in the database.
    pub fn record_client_version(&self, _version: ClientVersion) -> Result<(), DatabaseError> {
        // RocksDB doesn't require explicit client version recording like MDBX
        // The version information is typically handled at the application level
        Ok(())
    }
}

impl DatabaseMetrics for DatabaseEnv {
    fn report_metrics(&self) {
        // TODO: Implement metrics reporting for RocksDB
    }
}

// Implement Database trait for RocksDB
impl reth_db_api::database::Database for DatabaseEnv {
    type TX = tx::Tx<tx::RO>;
    type TXMut = tx::Tx<tx::RW>;

    fn tx(&self) -> Result<Self::TX, crate::DatabaseError> {
        Ok(tx::Tx::new(self.inner.clone()))
    }

    fn tx_mut(&self) -> Result<Self::TXMut, crate::DatabaseError> {
        Ok(tx::Tx::new(self.inner.clone()))
    }
}

/// Helper function to convert RocksDB errors to DatabaseError
fn rocksdb_error_to_database_error(e: rocksdb::Error) -> DatabaseError {
    DatabaseError::Read(DatabaseErrorInfo { message: e.to_string().into(), code: -1 })
}

/// Helper function to create a write error
fn create_write_error<T: Table>(
    e: rocksdb::Error,
    operation: DatabaseWriteOperation,
    key: Vec<u8>,
) -> DatabaseError {
    DatabaseError::Write(Box::new(DatabaseWriteError {
        info: DatabaseErrorInfo { message: e.to_string().into(), code: -1 },
        operation,
        table_name: T::NAME,
        key,
    }))
}

/// Helper function to get column family handle with proper error handling
fn get_cf_handle<T: Table>(db: &TransactionDB) -> Result<&rocksdb::ColumnFamily, DatabaseError> {
    db.cf_handle(T::NAME).ok_or_else(|| {
        DatabaseError::Open(DatabaseErrorInfo {
            message: format!("Column family '{}' not found", T::NAME).into(),
            code: -1,
        })
    })
}
