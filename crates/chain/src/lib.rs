//! `banana-chain` — everything that speaks JSON-RPC.
//!
//! Typed contract bindings, the priority RPC gate (spec §6.3), Multicall3 batching and
//! log decoding. This crate is the *only* place in the workspace that opens a socket.
//!
//! It holds no private key and signs nothing; see `banana-live` for that.

#![forbid(unsafe_code)]

pub mod abi;
pub mod addr;
pub mod doctor;
pub mod gate;
pub mod launch_log;
pub mod launch_tx;
pub mod rpc;
pub mod transport;

pub use gate::{Endpoint, Gate, GateConfig, GateStats, Priority, RpcError};
pub use launch_tx::{LaunchCalldata, LaunchMeta, decode_launch};
pub use rpc::{Client, LogFilter, RawLog, TxInfo};
pub use transport::{LiveTransport, RecordingTransport, ReplayTransport, Transport};
