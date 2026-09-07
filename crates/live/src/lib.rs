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

pub mod arm;
pub mod entry;
pub mod exits;
pub mod guards;
pub mod route;
pub mod session;
pub mod signer;

pub use arm::{ARM_PHRASE, Briefing, phrase_arms};
pub use entry::{Achieved, Schedule, Step, TaxReport};
pub use exits::{Exit, Mark};
pub use guards::{Budget, Refused, Spend};
pub use route::{PoolKey, Route};
pub use session::{Armed, Mode, Session, SessionError};
pub use signer::{EnvSigner, NoSigner, Signer, SignerError, TxRequest};
