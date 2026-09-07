//! `quarrel-core` — domain types, curve math and the rule engine.
//!
//! **Zero I/O.** No network, no filesystem, no database. Everything here is a pure
//! function of its arguments so both halves of the product — the live sniper and the
//! Strategy Lab — can be driven by the same code and tested offline.
//!
//! Two rules this crate enforces for the rest of the workspace:
//!
//! 1. **No `f64` in the money path** (spec §12). Money is `U256`; ratios are integer
//!    basis points. Enforced by `tests/no_float_money.rs`.
//! 2. **Point-in-time separation** (spec §5.3). Features an entry filter may read live
//!    in `PitFeatures`; facts that are the future relative to entry live in
//!    `PostEntryFacts`. The filter cannot name the second type, and that is a compile
//!    error, not a comment.

#![forbid(unsafe_code)]

/// Basis points denominator. The protocol's unit for every rate.
pub const BPS: u32 = 10_000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bps_denominator_matches_protocol() {
        assert_eq!(BPS, 10_000);
    }
}
