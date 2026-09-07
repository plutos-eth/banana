//! `quarrel-store` â€” SQLite persistence.
//!
//! Two databases in one `data/` directory (spec Â§4.1):
//!
//! * `history.db` â€” append-only indexed facts. Never rewritten.
//! * `live.db` â€” positions, session budget, trade journal.
//!
//! Every query sits behind a trait so a columnar backend could be added later without
//! touching callers. Analytical tables stay denormalised and append-only, and analytics
//! are done in SQL rather than in Rust.

#![forbid(unsafe_code)]
