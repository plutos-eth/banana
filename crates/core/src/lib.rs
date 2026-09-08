//! `banana-core` — domain types, curve math and the rule engine.
//!
//! **Zero I/O.** No network, no filesystem, no database. Everything here is a pure
//! function of its arguments so both halves of the product — the live sniper and the
//! Strategy Lab — can be driven by the same code and tested offline.
//!
//! Three rules this crate enforces for the rest of the workspace:
//!
//! 1. **No `f64` in the money path** (spec §12). Money is [`alloy_primitives::U256`];
//!    ratios are integer basis points ([`Bps`]). Enforced by `tests/source_hygiene.rs`.
//! 2. **Point-in-time separation** (spec §5.3). Features an entry filter may read live in
//!    `PitFeatures`; facts that are the future relative to entry live in `PostEntryFacts`.
//!    The evaluator only ever receives the former, so reading the latter in a filter is a
//!    type error rather than a code-review question.
//! 3. **The contract's arithmetic is the authority.** The curve math is a port, not a
//!    reimplementation, and it is checked against a real on-chain trade.

#![forbid(unsafe_code)]
// `EntryFilter` is a recursive enum, and serde's generated impls for it exceed the
// default trait-resolution depth. This is the cost of the predicate tree being a real
// tree rather than a flat list of rules (spec §7.1).
#![recursion_limit = "256"]

pub mod curve;
pub mod features;
pub mod filter;
pub mod pattern;
pub mod rank;
pub mod strategy;

/// Basis points. The protocol's unit for every rate: 10000 = 100%.
///
/// `u32` rather than `u16` because intermediate sums (fee + creator tax + opening tax) can
/// exceed 65535 while still being meaningful to report.
pub type Bps = u32;

/// Basis-points denominator.
pub const BPS: Bps = 10_000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bps_denominator_matches_protocol() {
        assert_eq!(BPS, 10_000);
    }
}
