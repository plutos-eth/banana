//! `quarrel-store` — SQLite persistence.
//!
//! Two databases in one `data/` directory (spec §4.1):
//!
//! * `history.db` — append-only indexed facts. Never rewritten.
//! * `live.db` — positions, session budget, trade journal.
//!
//! Locking is per database file rather than per directory, because the two have disjoint
//! writers: the indexer writes history, the engine writes live. That is what lets
//! `quarrel index` run from cron while the desktop app is open and trading (spec §10),
//! which a directory-wide lock would have made impossible.
//!
//! Every query sits behind a trait so a columnar backend could be added later without
//! touching callers. Analytical tables stay denormalised and append-only, and analytics
//! are done in SQL rather than in Rust.

#![forbid(unsafe_code)]
