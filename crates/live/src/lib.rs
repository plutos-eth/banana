//! `quarrel-live` — the sniper executor.
//!
//! # Trust boundary
//!
//! **This is the only crate in the workspace that reads `PRIVATE_KEY` or signs a
//! transaction** (spec §3.8).
//!
//! `core`, `chain`, `store`, `indexer` and `backtest` must never depend on this crate,
//! and neither must `cli` — indexing, backtesting, the feed, watching and scanning all
//! run with no key present at all. Only `app` may depend on `live`.
//!
//! The direction is enforced mechanically by `scripts/check-trust-boundary.ps1`, which
//! runs in CI. If you are adding a dependency edge into this crate, that check will fail
//! and it is right to.
//!
//! Key handling and the plaintext-`.env` trade-off are documented in `docs/SAFETY.md`.

#![forbid(unsafe_code)]

pub mod briefing;
pub mod engine;
pub mod enrich;
pub mod entry;
pub mod exec;
pub mod exits;
pub mod guards;
pub mod route;
pub mod seen;
pub mod session;
pub mod signer;
pub mod watch;

pub use briefing::Briefing;
pub use engine::{Engine, EngineConfig, Event};
pub use enrich::{ChainState, Reading};
pub use entry::{Achieved, Schedule, Step, TaxReport};
pub use exec::{ExecError, Filled, Order};
pub use exits::{Exit, Mark};
pub use guards::{Budget, Refused, Spend};
pub use route::{PoolKey, Route};
pub use seen::{Coverage, Seen};
pub use session::{Mode, Session, SessionError};
pub use signer::{KeySigner, NoSigner, Signer, SignerError, TxRequest, keystore};
pub use watch::{Launch, Sighting, WatchConfig, Watcher};
