//! Generic database environment that supports multiple backends.

// Re-export the appropriate database environment based on features
#[cfg(feature = "mdbx")]
pub use crate::mdbx::{
    create_db, init_db, open_db, open_db_read_only, 
    DatabaseEnv, DatabaseEnvKind, DatabaseArguments
};

#[cfg(all(feature = "rocksdb", not(feature = "mdbx")))]
pub use crate::rocksdb::{
    create_db, init_db, open_db, open_db_read_only, 
    DatabaseEnv, DatabaseEnvKind, DatabaseArguments
};
