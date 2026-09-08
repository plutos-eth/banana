//! Point-in-time features, computed by streaming launches in block order.
//!
//! Spec §5.3: every feature an entry filter can read must come only from blocks strictly
//! before `launch_block`. Rather than trusting each computation to respect that, this
//! module makes it structural — launches are processed in ascending block order and the
//! builder can only ever have seen earlier ones. A feature reading the future would have to
//! read data the builder does not hold.
//!
//! # The fingerprint is calldata-only
//!
//! bodkin's `FarmDetector` keys partly on `getTokenInfo()` socials, which is **current**
//! state, so its launch-farm signal is not point-in-time either. Ours is built entirely
//! from what the launch transaction fixed: dev-buy size, creator tax, which link fields
//! were filled in, and how many wallets were declared exempt.
//!
//! # The window-edge bias (PLAN.md C2)
//!
//! On a 24-hour index a launch two hours in has two hours of visible deployer history and
//! one twenty hours in has twenty, so every deployer looks fresher than it is and
//! progressively more so toward the start of the window. `deployer_history_depth_blocks`
//! records how much history actually existed, and any strategy using a deployer feature
//! restricts the universe by it and says so in the funnel.

use std::collections::HashMap;

use alloy_primitives::Address;
use banana_core::Bps;
// The fingerprint and its window live in `core` because both halves of the product need
// them: the indexer computes one per launch, and the sniper computes one live to count
// twins. Two implementations of a farm signature would be two different farm signatures.
pub use banana_core::features::{Fingerprint, TWIN_WINDOW_BLOCKS};

/// A launch, reduced to what feature computation needs.
#[derive(Debug, Clone)]
pub struct LaunchFacts {
    pub token: Address,
    pub deployer: Address,
    pub block: u64,
    pub fingerprint: Fingerprint,
}

/// The computed features for one launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PitRow {
    pub token: Address,
    pub deployer_launches: u32,
    pub deployer_graduations: u32,
    pub deployer_grad_rate_bps: Option<Bps>,
    pub fingerprint: String,
    pub fingerprint_twins_30m: u32,
    pub deployer_history_depth_blocks: u64,
}

/// Streams launches in block order, accumulating only what came before.
#[derive(Debug)]
pub struct FeatureBuilder {
    /// Where the indexed window starts, for the history-depth figure.
    window_from: u64,
    /// deployer -> blocks at which it launched, ascending.
    by_deployer: HashMap<Address, Vec<u64>>,
    /// token -> the block at which it graduated, for tokens that did.
    graduated_at: HashMap<Address, u64>,
    /// deployer -> the tokens it launched, in order, parallel to `by_deployer`.
    tokens_by_deployer: HashMap<Address, Vec<Address>>,
    /// fingerprint -> (block, deployer) of earlier launches.
    by_fingerprint: HashMap<String, Vec<(u64, Address)>>,
    last_block: u64,
}

impl FeatureBuilder {
    /// `graduated_at` maps every token that ever graduated to the block it did so.
    ///
    /// Passing graduations up front is not a point-in-time leak: each is only ever counted
    /// for a launch whose block is strictly greater, which `compute` enforces.
    pub fn new(window_from: u64, graduated_at: HashMap<Address, u64>) -> Self {
        Self {
            window_from,
            by_deployer: HashMap::new(),
            graduated_at,
            tokens_by_deployer: HashMap::new(),
            by_fingerprint: HashMap::new(),
            last_block: 0,
        }
    }

    /// Compute features for one launch, then record it for the launches that follow.
    ///
    /// **Must be called in ascending block order.** Out of order, a later launch would be
    /// visible to an earlier one, which is exactly the leak this design prevents.
    pub fn push(&mut self, l: &LaunchFacts) -> PitRow {
        debug_assert!(
            l.block >= self.last_block,
            "launches must arrive in block order or the point-in-time guarantee is void"
        );
        self.last_block = l.block;

        // --- deployer history, strictly before this block --------------------------------
        let prior_blocks = self.by_deployer.get(&l.deployer);
        let prior_tokens = self.tokens_by_deployer.get(&l.deployer);
        let mut deployer_launches = 0u32;
        let mut deployer_graduations = 0u32;
        if let (Some(blocks), Some(tokens)) = (prior_blocks, prior_tokens) {
            for (i, b) in blocks.iter().enumerate() {
                if *b >= l.block {
                    break;
                }
                deployer_launches += 1;
                // A prior launch counts as graduated only if it graduated BEFORE this
                // block. A graduation that happened later is the future.
                if let Some(t) = tokens.get(i)
                    && let Some(g) = self.graduated_at.get(t)
                    && *g < l.block
                {
                    deployer_graduations += 1;
                }
            }
        }

        let deployer_grad_rate_bps = (deployer_launches > 0)
            .then(|| (deployer_graduations * banana_core::BPS) / deployer_launches);

        // --- fingerprint twins in the preceding 30 minutes -------------------------------
        let cutoff = l.block.saturating_sub(TWIN_WINDOW_BLOCKS);
        let twins = if l.fingerprint.is_uninformative() {
            // Two launches nobody could decode are not evidence of a shared operator.
            0
        } else {
            self.by_fingerprint
                .get(l.fingerprint.as_str())
                .map(|v| {
                    v.iter()
                        .filter(|(b, d)| *b >= cutoff && *b < l.block && *d != l.deployer)
                        .count() as u32
                })
                .unwrap_or(0)
        };

        let row = PitRow {
            token: l.token,
            deployer_launches,
            deployer_graduations,
            deployer_grad_rate_bps,
            fingerprint: l.fingerprint.as_str().to_owned(),
            fingerprint_twins_30m: twins,
            deployer_history_depth_blocks: l.block.saturating_sub(self.window_from),
        };

        // Record only after computing, so a launch never sees itself.
        self.by_deployer
            .entry(l.deployer)
            .or_default()
            .push(l.block);
        self.tokens_by_deployer
            .entry(l.deployer)
            .or_default()
            .push(l.token);
        self.by_fingerprint
            .entry(l.fingerprint.as_str().to_owned())
            .or_default()
            .push((l.block, l.deployer));

        row
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    use banana_core::features::{Presence, Socials};

    fn addr(n: u8) -> Address {
        Address::repeat_byte(n)
    }

    fn fp(dev: u64) -> Fingerprint {
        Fingerprint::new(
            Some(U256::from(dev)),
            Some(100),
            Socials {
                twitter: Presence::Present,
                website: Presence::Absent,
                telegram: Presence::Absent,
            },
            Some(0),
        )
    }

    fn launch(token: u8, deployer: u8, block: u64, fingerprint: Fingerprint) -> LaunchFacts {
        LaunchFacts {
            token: addr(token),
            deployer: addr(deployer),
            block,
            fingerprint,
        }
    }

    #[test]
    fn a_fresh_deployer_has_no_history_and_no_rate() {
        let mut b = FeatureBuilder::new(0, HashMap::new());
        let r = b.push(&launch(1, 10, 1_000, fp(5)));
        assert_eq!(r.deployer_launches, 0);
        assert_eq!(
            r.deployer_grad_rate_bps, None,
            "no prior launches means no rate, which is not the same as a zero rate"
        );
    }

    #[test]
    fn prior_launches_accumulate_but_a_launch_never_counts_itself() {
        let mut b = FeatureBuilder::new(0, HashMap::new());
        assert_eq!(b.push(&launch(1, 10, 1_000, fp(5))).deployer_launches, 0);
        assert_eq!(b.push(&launch(2, 10, 2_000, fp(5))).deployer_launches, 1);
        assert_eq!(b.push(&launch(3, 10, 3_000, fp(5))).deployer_launches, 2);
        // A different deployer has its own history.
        assert_eq!(b.push(&launch(4, 11, 4_000, fp(5))).deployer_launches, 0);
    }

    /// The core point-in-time property: a graduation that happens after this launch must
    /// not be visible to it. Getting this wrong produces a beautiful, wrong backtest.
    #[test]
    fn a_graduation_that_happened_later_is_not_counted() {
        let mut graduated = HashMap::new();
        // Token 1 graduated at block 5,000.
        graduated.insert(addr(1), 5_000u64);

        let mut b = FeatureBuilder::new(0, graduated);
        b.push(&launch(1, 10, 1_000, fp(5)));

        // A launch at 3,000 is before that graduation: it must not see it.
        let early = b.push(&launch(2, 10, 3_000, fp(5)));
        assert_eq!(early.deployer_launches, 1);
        assert_eq!(
            early.deployer_graduations, 0,
            "the graduation is in this launch's future"
        );
        assert_eq!(early.deployer_grad_rate_bps, Some(0));

        // A launch at 7,000 is after it, so it may.
        let later = b.push(&launch(3, 10, 7_000, fp(5)));
        assert_eq!(later.deployer_graduations, 1);
        assert_eq!(later.deployer_grad_rate_bps, Some(5_000), "1 of 2 = 50%");
    }

    #[test]
    fn the_grad_rate_is_integer_basis_points() {
        let mut graduated = HashMap::new();
        // Token 1 launches at block 100 and graduates at 150, before the launch under test.
        graduated.insert(addr(1), 150u64);
        let mut b = FeatureBuilder::new(0, graduated);
        b.push(&launch(1, 10, 100, fp(5)));
        b.push(&launch(2, 10, 200, fp(5)));
        b.push(&launch(3, 10, 300, fp(5)));
        let r = b.push(&launch(4, 10, 400, fp(5)));
        assert_eq!(r.deployer_launches, 3);
        assert_eq!(r.deployer_graduations, 1);
        assert_eq!(r.deployer_grad_rate_bps, Some(3_333));
    }

    // --- fingerprints -------------------------------------------------------------------

    #[test]
    fn twins_are_earlier_launches_from_other_deployers_sharing_a_fingerprint() {
        let mut b = FeatureBuilder::new(0, HashMap::new());
        // One operator, three wallets, same template, minutes apart.
        assert_eq!(
            b.push(&launch(1, 10, 1_000, fp(7))).fingerprint_twins_30m,
            0
        );
        assert_eq!(
            b.push(&launch(2, 11, 2_000, fp(7))).fingerprint_twins_30m,
            1
        );
        assert_eq!(
            b.push(&launch(3, 12, 3_000, fp(7))).fingerprint_twins_30m,
            2
        );
    }

    #[test]
    fn a_deployers_own_earlier_launches_are_not_its_twins() {
        // A serial deployer is a different signal from a farm of fresh wallets, and
        // max_deployer_launches already covers it.
        let mut b = FeatureBuilder::new(0, HashMap::new());
        b.push(&launch(1, 10, 1_000, fp(7)));
        let r = b.push(&launch(2, 10, 2_000, fp(7)));
        assert_eq!(r.fingerprint_twins_30m, 0);
        assert_eq!(r.deployer_launches, 1, "counted here instead");
    }

    #[test]
    fn twins_outside_the_thirty_minute_window_do_not_count() {
        let mut b = FeatureBuilder::new(0, HashMap::new());
        b.push(&launch(1, 10, 1_000, fp(7)));
        let far = b.push(&launch(2, 11, 1_000 + TWIN_WINDOW_BLOCKS + 1, fp(7)));
        assert_eq!(far.fingerprint_twins_30m, 0, "too long ago");

        let near = b.push(&launch(3, 12, 1_000 + TWIN_WINDOW_BLOCKS + 2, fp(7)));
        assert_eq!(near.fingerprint_twins_30m, 1, "the one just before it does");
    }

    #[test]
    fn different_templates_are_not_twins() {
        let mut b = FeatureBuilder::new(0, HashMap::new());
        b.push(&launch(1, 10, 1_000, fp(7)));
        assert_eq!(
            b.push(&launch(2, 11, 1_100, fp(8))).fingerprint_twins_30m,
            0
        );
    }

    #[test]
    fn the_fingerprint_is_built_only_from_calldata() {
        // Nothing here comes from current contract state, which is what makes it
        // point-in-time -- unlike the reference implementation's.
        let a = Fingerprint::new(
            Some(U256::from(5u64)),
            Some(200),
            Socials {
                twitter: Presence::Present,
                website: Presence::Present,
                telegram: Presence::Absent,
            },
            Some(2),
        );
        let b = Fingerprint::new(
            Some(U256::from(5u64)),
            Some(200),
            Socials {
                twitter: Presence::Present,
                website: Presence::Present,
                telegram: Presence::Absent,
            },
            Some(2),
        );
        assert_eq!(a, b);

        // Each field distinguishes.
        assert_ne!(
            a,
            Fingerprint::new(
                Some(U256::from(6u64)),
                Some(200),
                Socials {
                    twitter: Presence::Present,
                    website: Presence::Present,
                    telegram: Presence::Absent
                },
                Some(2)
            )
        );
        assert_ne!(
            a,
            Fingerprint::new(
                Some(U256::from(5u64)),
                Some(300),
                Socials {
                    twitter: Presence::Present,
                    website: Presence::Present,
                    telegram: Presence::Absent
                },
                Some(2)
            )
        );
        assert_ne!(
            a,
            Fingerprint::new(
                Some(U256::from(5u64)),
                Some(200),
                Socials {
                    twitter: Presence::Absent,
                    website: Presence::Present,
                    telegram: Presence::Absent
                },
                Some(2)
            )
        );
        assert_ne!(
            a,
            Fingerprint::new(
                Some(U256::from(5u64)),
                Some(200),
                Socials {
                    twitter: Presence::Present,
                    website: Presence::Present,
                    telegram: Presence::Absent
                },
                Some(3)
            )
        );
    }

    #[test]
    fn undecodable_launches_are_not_twins_of_each_other() {
        // Two launches nobody could read are not evidence of a shared operator, and
        // treating them as a farm would refuse a whole class of launches for a reason that
        // is about our decoder rather than about them.
        let unknown = Fingerprint::new(None, None, Socials::UNKNOWN, None);
        assert!(unknown.is_uninformative());

        let mut b = FeatureBuilder::new(0, HashMap::new());
        b.push(&launch(1, 10, 1_000, unknown.clone()));
        let r = b.push(&launch(2, 11, 1_100, unknown));
        assert_eq!(r.fingerprint_twins_30m, 0);
    }

    #[test]
    fn a_readable_zero_dev_buy_is_distinct_from_an_unreadable_one() {
        let zero = Fingerprint::new(
            Some(U256::ZERO),
            Some(0),
            Socials {
                twitter: Presence::Absent,
                website: Presence::Absent,
                telegram: Presence::Absent,
            },
            Some(0),
        );
        let unknown = Fingerprint::new(None, None, Socials::UNKNOWN, None);
        assert_ne!(zero, unknown);
        assert!(!zero.is_uninformative(), "a real zero is information");
    }

    // --- the window-edge bias -----------------------------------------------------------

    #[test]
    fn history_depth_records_how_much_history_actually_existed() {
        // PLAN.md C2: this is what stops a 24-hour window from making every deployer look
        // fresh, and it is what the funnel restricts on.
        let mut b = FeatureBuilder::new(1_000_000, HashMap::new());
        let early = b.push(&launch(1, 10, 1_000_100, fp(5)));
        assert_eq!(
            early.deployer_history_depth_blocks, 100,
            "a launch near the window edge has almost no visible history"
        );

        let late = b.push(&launch(2, 11, 1_800_000, fp(5)));
        assert_eq!(late.deployer_history_depth_blocks, 800_000);
    }

    #[test]
    fn a_launch_before_the_window_start_reports_zero_depth_rather_than_underflowing() {
        let mut b = FeatureBuilder::new(1_000_000, HashMap::new());
        let r = b.push(&launch(1, 10, 999, fp(5)));
        assert_eq!(r.deployer_history_depth_blocks, 0);
    }

    #[test]
    fn launches_in_the_same_block_do_not_see_each_other() {
        // Two launches at the same block are simultaneous; neither is "before" the other,
        // so neither may count the other.
        let mut b = FeatureBuilder::new(0, HashMap::new());
        assert_eq!(b.push(&launch(1, 10, 5_000, fp(7))).deployer_launches, 0);
        let second = b.push(&launch(2, 10, 5_000, fp(7)));
        assert_eq!(second.deployer_launches, 0, "same block is not earlier");
        assert_eq!(second.fingerprint_twins_30m, 0);
    }
}
