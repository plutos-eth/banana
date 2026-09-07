//! `display_rank` — how near a refused launch came to passing.
//!
//! Spec §7.1. With the default rules well over 99% of launches are refused, and an
//! unordered wall of refusals is unusable. So the feed is sorted by this value.
//!
//! # What this is not
//!
//! It is **not a score** and it is deliberately not called one. It:
//!
//! * does not appear in [`crate::filter::Decision`],
//! * is never persisted as a judgement about a launch,
//! * never influences whether a launch is taken.
//!
//! The decision is a pure boolean AND over the filter, computed entirely in
//! [`crate::filter`]. This module cannot change it, and the ordering it produces carries
//! no claim that a launch ranked 9000 is "nearly good" — only that it failed fewer rules
//! than one ranked 2000. Two launches with the same rank are not equally promising; they
//! merely failed a comparable number of conditions.

use crate::BPS;
use crate::features::PitFeatures;
use crate::filter::{Condition, EntryFilter};

/// How close a launch came, in basis points. 10000 means every condition passed.
///
/// Computed as the fraction of atomic conditions satisfied, ignoring tree structure. That
/// is a deliberate simplification: this orders a list, so it needs to be cheap, stable and
/// explainable, not correct in some deeper sense.
pub fn display_rank(filter: &EntryFilter, f: &PitFeatures) -> u32 {
    let conditions = filter.conditions();
    if conditions.is_empty() {
        return BPS;
    }
    let passed = conditions.iter().filter(|c| passes(c, f)).count();
    ((passed as u64 * BPS as u64) / conditions.len() as u64) as u32
}

/// Whether one atomic condition holds, without building a refusal message.
fn passes(c: &Condition, f: &PitFeatures) -> bool {
    EntryFilter::Cond(c.clone()).evaluate(f).passed
}

/// Order launches for the feed: nearest to passing first, then most recent first.
///
/// Takes `(rank, seen_at_ms)` pairs so the caller keeps ownership of its rows.
pub fn feed_order(a: (u32, u64), b: (u32, u64)) -> std::cmp::Ordering {
    b.0.cmp(&a.0).then(b.1.cmp(&a.1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::*;
    use crate::filter::Condition;

    fn clean() -> PitFeatures {
        PitFeatures {
            pair: Pair::Eth,
            name: "SpaceWaffle".into(),
            symbol: "WAFFLE".into(),
            description: String::new(),
            socials: Socials {
                twitter: Presence::Present,
                website: Presence::Present,
                telegram: Presence::Absent,
            },
            exempt_wallets: Some(0),
            dev_buy_bps: Some(300),
            creator_tax_bps: Some(100),
            fee_recipient: FeeRecipient::Deployer,
            deployer_launches: 0,
            deployer_graduations: 0,
            fingerprint_twins_30m: 0,
            deployer_history_depth_blocks: 0,
        }
    }

    fn filter() -> EntryFilter {
        EntryFilter::all_of([
            Condition::RequireTwitter,
            Condition::MaxCreatorTaxBps { bps: 200 },
            Condition::MaxExemptWallets { max: 2 },
            Condition::MaxFingerprintTwins { max: 1 },
        ])
    }

    #[test]
    fn a_passing_launch_ranks_full() {
        assert_eq!(display_rank(&filter(), &clean()), BPS);
    }

    #[test]
    fn each_failed_condition_lowers_the_rank() {
        let mut f = clean();
        f.creator_tax_bps = Some(900); // fails 1 of 4
        assert_eq!(display_rank(&filter(), &f), 7_500);

        f.exempt_wallets = Some(9); // fails 2 of 4
        assert_eq!(display_rank(&filter(), &f), 5_000);

        f.socials.twitter = Presence::Absent; // 3 of 4
        assert_eq!(display_rank(&filter(), &f), 2_500);

        f.fingerprint_twins_30m = 20; // all 4
        assert_eq!(display_rank(&filter(), &f), 0);
    }

    #[test]
    fn rank_never_changes_the_decision() {
        // The contract this module must not break: a launch that ranks 7500 is still
        // refused, and a rank of 10000 is not itself a pass.
        let mut f = clean();
        f.creator_tax_bps = Some(900);
        let d = filter().evaluate(&f);
        assert!(!d.passed);
        assert_eq!(display_rank(&filter(), &f), 7_500);
        assert!(
            !d.passed,
            "a high rank must never turn a refusal into a pass"
        );
    }

    #[test]
    fn an_empty_filter_ranks_full_because_nothing_can_fail() {
        assert_eq!(display_rank(&EntryFilter::All(vec![]), &clean()), BPS);
    }

    #[test]
    fn feed_order_puts_nearest_first_then_newest() {
        let mut rows = vec![(2_500u32, 100u64), (10_000, 50), (7_500, 10), (7_500, 90)];
        rows.sort_by(|a, b| feed_order(*a, *b));
        assert_eq!(
            rows,
            vec![(10_000, 50), (7_500, 90), (7_500, 10), (2_500, 100)]
        );
    }
}
