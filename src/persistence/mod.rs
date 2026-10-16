//! PostgreSQL: the store, the schema migrations, the typed search plan and the SQL built from it.

pub mod plan;
pub mod postgres;
pub mod sql;
pub mod translate;

pub use postgres::{PostgresStore, connect, migrate};
