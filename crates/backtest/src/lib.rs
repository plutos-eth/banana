//! `banana-backtest` — run a strategy over the store.
//!
//! Pure: fed by `banana-store`, never by the network. Measures the objective fate of
//! tokens that passed the filter and **does not simulate selling** (spec §5.6) —
//! `ExitPolicy` belongs to the live sniper and is ignored here.
//!
//! The honesty guards of spec §5.5 are enforced in this crate, not in the UI: maturity
//! cutoff, sample-size gate, the mandatory funnel, and the regime warning.

#![forbid(unsafe_code)]

pub mod funnel;
pub mod metrics;
pub mod run;

pub use funnel::{Funnel, Stage};
pub use metrics::{HoldStats, MIN_SAMPLE, Measured, PeakStats, Percentiles, Results};
pub use run::{BacktestError, BacktestResult, run};
