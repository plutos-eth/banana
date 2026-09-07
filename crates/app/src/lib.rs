//! `quarrel-app` — Tauri backend: commands, events, state and the engine supervisor.
//!
//! The engine does not live in the window (spec §4.2). It runs as a supervised
//! background task with its own lifecycle; the UI is a view over it. Closing or
//! reloading the window must not stop trading, and a UI panic must not take positions
//! with it. UI→engine traffic goes through Tauri commands, engine→UI through events.

#![forbid(unsafe_code)]
