//! `quarrel-chain` — everything that speaks JSON-RPC.
//!
//! Typed contract bindings, the priority RPC gate (spec §6.3), Multicall3 batching and
//! log decoding. This crate is the *only* place in the workspace that opens a socket.
//!
//! It holds no private key and signs nothing; see `quarrel-live` for that.

#![forbid(unsafe_code)]
