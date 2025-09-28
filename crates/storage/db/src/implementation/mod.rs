// Keep mdbx module for reference, but only compile it when the feature is explicitly enabled
#[cfg(feature = "mdbx")]
pub mod mdbx;

// Always export rocksdb as the primary implementation
pub mod rocksdb;
