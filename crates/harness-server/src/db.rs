// Re-export the shared database layer from harness-core so that all modules
// within this crate can continue to import from `crate::db::*`.
pub use harness_core::db::{open_pool, Db, DbEntity, DbSerializable, Migration, Migrator};
