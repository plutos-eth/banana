//! `quarrel-indexer` — resumable backfill and incremental update.
//!
//! Four weighted phases (PLAN.md D4), all running at the RPC gate's `Bulk` priority so a
//! background index can never add latency to a live entry:
//!
//! | phase | source | cost per block |
//! |---|---|---|
//! | A | factory logs: launches, graduations, sweeps | very low |
//! | B | trade logs: `CurveBuy`, `CurveSell`, `SnipeTaxCharged` | high |
//! | C | launch calldata, one transaction per launch | high in request count |
//! | D | block-timestamp anchors | very low |

#![forbid(unsafe_code)]

pub mod chunking;
pub mod features;
pub mod outcomes;
pub mod progress;
pub mod run;
pub mod scan;
pub mod verify;

pub use chunking::{Chunker, ChunkerConfig, Range};
pub use progress::{Phase, Progress, ProgressEvent};
pub use run::{IndexPlan, IndexReport};
