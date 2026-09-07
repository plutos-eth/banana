//! `quarrel` — thin headless entrypoint for `index` and `doctor`.
//!
//! The desktop app is the primary surface (spec §10). This binary exists so a long index
//! can run from a terminal or a cron job, and so `doctor` can be scripted.
//!
//! **No command here requires a private key**, and this crate deliberately does not
//! depend on `quarrel-live`, which is what makes that guarantee mechanical rather than
//! aspirational (PLAN.md C6).

#![forbid(unsafe_code)]

fn main() {
    println!("quarrel {} — phase 0 skeleton", env!("CARGO_PKG_VERSION"));
}
