//! `quarrel-store` — SQLite persistence.
//!
//! Two databases in one `data/` directory (spec §4.1):
//!
//! * `history.db` — append-only indexed facts. Never rewritten.
//! * `live.db` — positions, session budget, trade journal.
//!
//! Locking is **per database file, not per directory** (PLAN.md C3). The two have disjoint
//! writers by design: the indexer writes history, the engine writes live. The CLI takes
//! the `history.db` writer lock; the app takes `live.db`'s and opens `history.db`
//! read-only when it does not hold that lock. This is what lets `quarrel index` run from
//! cron while the desktop app is open and trading, which spec §10 requires and a
//! directory-wide lock would have made impossible.
//!
//! Every query sits behind a trait so a columnar backend could be added later without
//! touching callers, and analytics are done in SQL rather than in Rust.

#![forbid(unsafe_code)]

pub mod history;
pub mod lab;
pub mod lock;
pub mod schema;
pub mod sql;
pub mod types;

pub use history::History;
pub use lab::{Candidate, Outcome, Window};
pub use lock::{Lock, LockError};
pub use sql::{SqlFilter, push_down};
pub use types::{u256_from_blob, u256_to_blob};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Corrupt(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;
