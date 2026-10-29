//! SQLite backend for the durable [`Storage`](crate::durable::Storage) trait.
//!
//! Upstream layout and where each piece went:
//!
//! | upstream | here |
//! |---|---|
//! | `storage/sqlite/index.ts` | this module's re-exports |
//! | `storage/sqlite/schema.ts` | [`schema`] |
//! | `storage/sqlite/migrations.ts` | [`migrations`] |
//! | `storage/sqlite/storage.ts` | [`storage`] |
//!
//! Deliberately not ported: `database.ts` (the cross-runtime executor facade —
//! `rusqlite` is the only runtime here), `node.ts` (its file-backed open is
//! [`SqliteStorage::open`]), `cloudflare.ts` (Durable Object storage), and the
//! `prefix` option that namespaces one schema inside a shared database, since
//! this backend owns its database file.
//!
//! This module is compiled only when a feature that enables `rusqlite` is on
//! (`extensions` or `context-mode`); see `storage/mod.rs`.

pub mod migrations;
pub mod schema;
pub mod storage;

pub use migrations::{
    apply_sqlite_migrations, apply_sqlite_migrations_with, create_sqlite_migrations,
    SqliteMigration, CURRENT_SQLITE_SCHEMA_VERSION,
};
pub use schema::{indexes, tables};
pub use storage::{SqliteStorage, SqliteStorageOptions};
