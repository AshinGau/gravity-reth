//! RocksDB implementation for the database.

use crate::{DatabaseError, TableSet};
use reth_db_api::{
    database_metrics::DatabaseMetrics, models::ClientVersion, table::Table, DatabaseWriteOperation,
    Tables,
};
use reth_storage_errors::db::{DatabaseErrorInfo, DatabaseWriteError, LogLevel};
use rocksdb::{Options, DB};
use std::{path::Path, sync::Arc};
use metrics::Label;

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
#[derive(Debug)]
pub struct DatabaseEnv {
    /// Inner RocksDB database.
    pub(crate) inner: Arc<DB>,
    /// Database environment kind (read-only or read-write).
    kind: DatabaseEnvKind,
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
        let db = DB::open_cf(&opts, path, &required_tables)
            .map_err(|e| DatabaseError::Other(format!("Failed to open RocksDB: {}", e)))?;
        
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
    fn histogram_metrics(&self) -> Vec<(&'static str, f64, Vec<Label>)> {
        let mut metrics = Vec::new();
        
        // 1. rocksdb.actual-delayed-write-rate
        // Gets the actual delayed write rate (bytes/sec) when write stall occurs
        if let Ok(Some(value)) = self.inner.property_value("rocksdb.actual-delayed-write-rate") {
            if let Ok(rate) = value.parse::<u64>() {
                metrics.push((
                    "rocksdb.actual_delayed_write_rate",
                    rate as f64,
                    vec![],
                ));
            }
        }
        
        // 2. rocksdb.is-write-stopped
        // Indicates whether writes are completely stopped (0 = no, 1 = yes)
        if let Ok(Some(value)) = self.inner.property_value("rocksdb.is-write-stopped") {
            if let Ok(stopped) = value.parse::<u64>() {
                metrics.push((
                    "rocksdb.is_write_stopped",
                    stopped as f64,
                    vec![],
                ));
            }
        }
        
        // 3. rocksdb.cfstats.stall-micros
        // Collect stall statistics for each column family
        for table_name in Tables::tables().map(|t| t.name()) {
            if let Some(cf) = self.inner.cf_handle(table_name) {
                // Get cfstats string and parse stall-related microseconds
                if let Ok(Some(cfstats)) = self.inner.property_value_cf(cf, "rocksdb.cfstats") {
                    // Parse stall metrics from cfstats
                    // The cfstats format contains multiple lines, we need to find lines with "Stall"
                    let stall_micros = parse_stall_micros_from_cfstats(&cfstats);
                    
                    if let Some(total_stall) = stall_micros.total {
                        metrics.push((
                            "rocksdb.cfstats.stall_micros",
                            total_stall as f64,
                            vec![
                                Label::new("cf", table_name),
                                Label::new("type", "total"),
                            ],
                        ));
                    }
                    
                    if let Some(level0_slowdown) = stall_micros.level0_slowdown {
                        metrics.push((
                            "rocksdb.cfstats.stall_micros",
                            level0_slowdown as f64,
                            vec![
                                Label::new("cf", table_name),
                                Label::new("type", "level0_slowdown"),
                            ],
                        ));
                    }
                    
                    if let Some(level0_numfiles) = stall_micros.level0_numfiles {
                        metrics.push((
                            "rocksdb.cfstats.stall_micros",
                            level0_numfiles as f64,
                            vec![
                                Label::new("cf", table_name),
                                Label::new("type", "level0_numfiles"),
                            ],
                        ));
                    }
                    
                    if let Some(pending_compaction) = stall_micros.pending_compaction_bytes {
                        metrics.push((
                            "rocksdb.cfstats.stall_micros",
                            pending_compaction as f64,
                            vec![
                                Label::new("cf", table_name),
                                Label::new("type", "pending_compaction_bytes"),
                            ],
                        ));
                    }
                }
            }
        }
        
        // 4. rocksdb.cur-size-all-mem-tables
        // Total size of all memory tables in bytes
        if let Ok(Some(value)) = self.inner.property_value("rocksdb.cur-size-all-mem-tables") {
            if let Ok(size) = value.parse::<u64>() {
                metrics.push((
                    "rocksdb.cur_size_all_mem_tables",
                    size as f64,
                    vec![],
                ));
            }
        }
        
        // 5. rocksdb.num-running-compactions
        // Number of currently running compactions
        if let Ok(Some(value)) = self.inner.property_value("rocksdb.num-running-compactions") {
            if let Ok(num) = value.parse::<u64>() {
                metrics.push((
                    "rocksdb.num_running_compactions",
                    num as f64,
                    vec![],
                ));
            }
        }
        
        // 6. rocksdb.num-running-flushes
        // Number of currently running flushes
        if let Ok(Some(value)) = self.inner.property_value("rocksdb.num-running-flushes") {
            if let Ok(num) = value.parse::<u64>() {
                metrics.push((
                    "rocksdb.num_running_flushes",
                    num as f64,
                    vec![],
                ));
            }
        }
        
        metrics
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

/// Convert RocksDB error to DatabaseErrorInfo
fn to_error_info(e: rocksdb::Error) -> DatabaseErrorInfo {
    DatabaseErrorInfo { message: e.to_string().into(), code: -1 }
}

/// Create a read error from RocksDB error
fn read_error(e: rocksdb::Error) -> DatabaseError {
    DatabaseError::Read(to_error_info(e))
}

/// Create a delete error from RocksDB error
fn delete_error(e: rocksdb::Error) -> DatabaseError {
    DatabaseError::Delete(to_error_info(e))
}

/// Create a write error from RocksDB error with operation context
fn write_error<T: Table>(
    e: rocksdb::Error,
    operation: DatabaseWriteOperation,
    key: Vec<u8>,
) -> DatabaseError {
    DatabaseError::Write(Box::new(DatabaseWriteError {
        info: to_error_info(e),
        operation,
        table_name: T::NAME,
        key,
    }))
}

/// Helper function to get column family handle with proper error handling
fn get_cf_handle<T: Table>(db: &DB) -> Result<&rocksdb::ColumnFamily, DatabaseError> {
    db.cf_handle(T::NAME).ok_or_else(|| {
        DatabaseError::Open(DatabaseErrorInfo {
            message: format!("Column family '{}' not found", T::NAME).into(),
            code: -1,
        })
    })
}

/// Stall-related statistics in microseconds
#[derive(Debug, Default)]
struct StallMicros {
    /// Total stall time
    total: Option<u64>,
    /// Stall time caused by level0 slowdown
    level0_slowdown: Option<u64>,
    /// Stall time caused by level0 file number limit
    level0_numfiles: Option<u64>,
    /// Stall time caused by pending compaction bytes limit
    pending_compaction_bytes: Option<u64>,
}

/// Parses stall-related microseconds from RocksDB cfstats string
/// 
/// Example cfstats format:
/// ```
/// ** Compaction Stats [default] **
/// Level    Files   Size     ...
/// Stall(count): level0_slowdown: 5, level0_numfiles: 0, ...
/// Stall(us): level0_slowdown: 1234567, level0_numfiles: 0, ...
/// ```
fn parse_stall_micros_from_cfstats(cfstats: &str) -> StallMicros {
    let mut result = StallMicros::default();
    
    // Find lines containing "Stall(us):"
    for line in cfstats.lines() {
        let line = line.trim();
        
        if line.starts_with("Stall(us):") || line.contains("Stall(us):") {
            // Parse format: "Stall(us): level0_slowdown: 1234, level0_numfiles: 5678, ..."
            // Remove "Stall(us):" prefix
            let content = line.strip_prefix("Stall(us):").unwrap_or(line);
            
            // Split by different stall types
            for part in content.split(',') {
                let part = part.trim();
                if let Some((key, value)) = part.split_once(':') {
                    let key = key.trim();
                    let value = value.trim();
                    
                    if let Ok(micros) = value.parse::<u64>() {
                        match key {
                            "level0_slowdown" => result.level0_slowdown = Some(micros),
                            "level0_numfiles" | "level0_numfiles_limit" => {
                                result.level0_numfiles = Some(micros)
                            }
                            "pending_compaction_bytes" | "pending_compaction_bytes_limit" => {
                                result.pending_compaction_bytes = Some(micros)
                            }
                            _ => {}
                        }
                    }
                }
            }
            
            // Calculate total stall time
            let mut total = 0u64;
            if let Some(v) = result.level0_slowdown {
                total += v;
            }
            if let Some(v) = result.level0_numfiles {
                total += v;
            }
            if let Some(v) = result.pending_compaction_bytes {
                total += v;
            }
            if total > 0 {
                result.total = Some(total);
            }
        }
    }
    
    result
}
