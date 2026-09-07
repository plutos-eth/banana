//! When to close a position (spec §7, "Exit").
//!
//! Take profit, stop loss, trailing stop, max hold, and partial exits. Live only — the
//! Lab ignores `ExitPolicy` entirely (§5.6), because a backtest that simulates selling is
//! measuring the exit rather than the filter.
//!
//! # Everything here is integer basis points
//!
//! A position's mark is a multiple of its entry, in bps: 10000 is break-even, 20000 is
//! twice the entry. No floating point (spec §12).
//!
//! # The trailing stop trails the peak, and the peak is not profit
//!
//! `peak_bps` is the best mark this position has *seen*, which is exactly the number §5.4
//! says must never be presented as a return. Here it is not a claim about what was made;
//! it is the reference the trailing stop measures down from, which is the one legitimate
//! use for it.

use quarrel_core::BPS;
use quarrel_core::strategy::ExitPolicy;
use serde::{Deserialize, Serialize};

/// What a position looks like to the exit rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mark {
    /// Current value as basis points of entry. 10000 = break-even.
    pub mult_bps: u64,
    /// The best `mult_bps` seen since entry.
    pub peak_bps: u64,
    /// How long the position has been open.
    pub held_secs: u64,
    /// Fraction of the ORIGINAL position still held, in bps. 10000 = untouched.
    pub remaining_bps: u32,
}

impl Mark {
    pub fn new(mult_bps: u64, held_secs: u64) -> Self {
        Self {
            mult_bps,
            peak_bps: mult_bps.max(BPS as u64),
            held_secs,
            remaining_bps: BPS,
        }
    }

    /// Fold in a new mark, carrying the peak forward.
    pub fn observe(self, mult_bps: u64, held_secs: u64) -> Self {
        Self {
            mult_bps,
            peak_bps: self.peak_bps.max(mult_bps),
            held_secs,
            ..self
        }
    }
}

/// What to do about a position, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Exit {
    /// Nothing fired.
    Hold,
    /// Sell `sell_bps` of the ORIGINAL position, naming the rule and its numbers.
    Sell {
        /// Basis points of the original position size.
        sell_bps: u32,
        rule: String,
        /// Reads as a sentence a user can check (spec §3.4).
        detail: String,
    },
}

impl Exit {
    fn sell(sell_bps: u32, rule: &str, detail: String) -> Self {
        Exit::Sell {
            sell_bps,
            rule: rule.to_owned(),
            detail,
        }
    }

    pub fn is_hold(&self) -> bool {
        matches!(self, Exit::Hold)
    }
}

/// Format basis points as a multiple: 20000 -> "2.00x". Integer arithmetic only.
fn x(bps: u64) -> String {
    format!("{}.{:02}x", bps / 10_000, (bps % 10_000) / 100)
}

/// Decide what to do with a position.
///
/// Rules are checked in a fixed order, and the **first** one to fire wins. The order is
/// not arbitrary: the ones that get you out entirely come before the ones that trim, so a
/// position that has both hit its stop and passed a partial threshold is closed rather
/// than reduced. Getting out is never the wrong answer to "two rules disagree".
///
/// A consequence worth knowing when writing a policy: **a partial above the take-profit
/// can never fire**, because the take-profit closes the position first. `take_profit 1.80x`
/// with a partial at `2.00x` is dead configuration, not a staged exit.
pub fn decide(policy: &ExitPolicy, m: Mark) -> Exit {
    // Nothing left to sell.
    if m.remaining_bps == 0 {
        return Exit::Hold;
    }

    if let Some(sl) = policy.stop_loss_bps {
        // `stop_loss_bps` is how far BELOW entry the stop sits: 3500 means "out at 0.65x".
        let floor = (BPS as u64).saturating_sub(sl as u64);
        if m.mult_bps <= floor {
            return Exit::sell(
                m.remaining_bps,
                "stop_loss",
                format!("mark {} at or below the {} stop", x(m.mult_bps), x(floor)),
            );
        }
    }

    if let Some(trail) = policy.trailing_bps {
        // Only once the position has been in front: a trailing stop that can fire below
        // entry is a second, looser stop loss wearing the wrong name.
        if m.peak_bps > BPS as u64 {
            let floor = m.peak_bps.saturating_sub(trail as u64).max(BPS as u64);
            if m.mult_bps <= floor {
                return Exit::sell(
                    m.remaining_bps,
                    "trailing",
                    format!(
                        "mark {} fell {} from a peak of {} — the peak was never captured, \
                         it is what the trail measures from",
                        x(m.mult_bps),
                        x(m.peak_bps.saturating_sub(m.mult_bps)),
                        x(m.peak_bps)
                    ),
                );
            }
        }
    }

    if let Some(tp) = policy.take_profit_bps {
        // `take_profit_bps` is how far ABOVE entry: 8000 means "out at 1.80x".
        let target = (BPS as u64).saturating_add(tp as u64);
        if m.mult_bps >= target {
            return Exit::sell(
                m.remaining_bps,
                "take_profit",
                format!("mark {} reached the {} target", x(m.mult_bps), x(target)),
            );
        }
    }

    if let Some(max_hold) = policy.max_hold_secs
        && m.held_secs >= max_hold
    {
        return Exit::sell(
            m.remaining_bps,
            "max_hold",
            format!(
                "held {}s, past the {max_hold}s limit, at {}",
                m.held_secs,
                x(m.mult_bps)
            ),
        );
    }

    // Partials last, in ascending trigger order, accounted CUMULATIVELY.
    //
    // The obvious implementation — "a partial is taken when the remainder is at or below
    // what it would leave" — is wrong as soon as there are two of them: after a 50%
    // partial the remainder is 50%, which also looks like the remainder a 25% partial
    // would have left, so the second one never fires. Tracking the running total instead
    // makes each partial mean "bring the total sold up to here", which is both correct
    // and what a user writing `50% at 2x, 25% at 5x` intends.
    let sold = BPS.saturating_sub(m.remaining_bps);
    let mut cumulative = 0u32;
    let mut partials: Vec<_> = policy.partials.iter().collect();
    partials.sort_by_key(|p| p.at_multiple_bps);
    for p in partials {
        cumulative = cumulative.saturating_add(p.sell_bps);
        if cumulative <= sold {
            continue; // already covered by what has been sold
        }
        if m.mult_bps < p.at_multiple_bps as u64 {
            // Ascending, so nothing further can have triggered either.
            break;
        }
        // Never more than is actually left, even if the accounting has been disturbed by
        // a rule outside this function.
        let sell = cumulative.saturating_sub(sold).min(m.remaining_bps);
        if sell == 0 {
            continue;
        }
        return Exit::sell(
            sell,
            "partial",
            format!(
                "mark {} reached the {} partial; selling {} of the original position,                  taking the total sold to {}",
                x(m.mult_bps),
                x(p.at_multiple_bps as u64),
                pct(sell),
                pct(sold.saturating_add(sell))
            ),
        );
    }

    Exit::Hold
}

fn pct(bps: u32) -> String {
    format!("{}.{:02}%", bps / 100, bps % 100)
}

#[cfg(test)]
mod tests {
    use super::*;
    use quarrel_core::strategy::PartialExit;

    /// Nothing set, so nothing can fire. Each test switches on the rule it is about.
    fn none() -> ExitPolicy {
        ExitPolicy {
            take_profit_bps: None,
            stop_loss_bps: None,
            trailing_bps: None,
            max_hold_secs: None,
            partials: Vec::new(),
        }
    }

    fn sold(e: &Exit) -> u32 {
        match e {
            Exit::Sell { sell_bps, .. } => *sell_bps,
            Exit::Hold => 0,
        }
    }

    fn rule(e: &Exit) -> &str {
        match e {
            Exit::Sell { rule, .. } => rule,
            Exit::Hold => "hold",
        }
    }

    #[test]
    fn an_empty_policy_never_sells() {
        for mult in [0, 5_000, 10_000, 100_000] {
            assert!(decide(&none(), Mark::new(mult, 99_999)).is_hold());
        }
    }

    // --- stop loss ------------------------------------------------------------------

    #[test]
    fn the_stop_loss_closes_the_whole_position() {
        let p = ExitPolicy {
            stop_loss_bps: Some(3_500),
            ..none()
        };
        // 3500 bps below entry is 0.65x.
        assert!(decide(&p, Mark::new(6_600, 10)).is_hold());
        let e = decide(&p, Mark::new(6_500, 10));
        assert_eq!(sold(&e), 10_000, "a stop is not a partial");
        assert_eq!(rule(&e), "stop_loss");
    }

    #[test]
    fn a_stop_loss_refusal_names_the_mark_and_the_stop() {
        let p = ExitPolicy {
            stop_loss_bps: Some(3_500),
            ..none()
        };
        let Exit::Sell { detail, .. } = decide(&p, Mark::new(5_000, 10)) else {
            panic!("should have sold");
        };
        assert!(detail.contains("0.50x"), "{detail}");
        assert!(detail.contains("0.65x"), "{detail}");
    }

    // --- take profit ----------------------------------------------------------------

    #[test]
    fn take_profit_fires_at_its_target_and_not_before() {
        let p = ExitPolicy {
            take_profit_bps: Some(8_000),
            ..none()
        };
        assert!(decide(&p, Mark::new(17_999, 10)).is_hold());
        let e = decide(&p, Mark::new(18_000, 10));
        assert_eq!(rule(&e), "take_profit");
        assert_eq!(sold(&e), 10_000);
    }

    // --- trailing stop --------------------------------------------------------------

    #[test]
    fn the_trailing_stop_measures_down_from_the_peak() {
        let p = ExitPolicy {
            trailing_bps: Some(2_500),
            ..none()
        };
        let m = Mark::new(10_000, 0).observe(30_000, 10);
        assert_eq!(m.peak_bps, 30_000);
        // 2500 below the 3.00x peak is 2.75x.
        assert!(decide(&p, m.observe(27_600, 20)).is_hold());
        let e = decide(&p, m.observe(27_500, 20));
        assert_eq!(rule(&e), "trailing");
    }

    #[test]
    fn a_trailing_stop_never_fires_on_a_position_that_was_never_in_front() {
        // Otherwise it is a second stop loss under a name that says otherwise, and a user
        // who set no stop loss would be stopped out anyway.
        let p = ExitPolicy {
            trailing_bps: Some(2_500),
            ..none()
        };
        let m = Mark::new(10_000, 0).observe(4_000, 30);
        assert_eq!(m.peak_bps, 10_000, "never above entry");
        assert!(decide(&p, m).is_hold());
    }

    #[test]
    fn the_trailing_stop_floor_never_drops_below_entry() {
        // A 0.60x trail from a 1.10x peak would sit at 0.50x, which is a stop loss the
        // user did not ask for.
        let p = ExitPolicy {
            trailing_bps: Some(6_000),
            ..none()
        };
        let m = Mark::new(10_000, 0).observe(11_000, 10);
        assert!(decide(&p, m.observe(10_500, 20)).is_hold());
        let e = decide(&p, m.observe(10_000, 20));
        assert_eq!(rule(&e), "trailing", "the floor is entry, not below it");
    }

    #[test]
    fn the_trailing_detail_says_the_peak_was_not_captured() {
        // §5.4 reaches into the exit log too: a line saying "fell 1.00x from a peak of
        // 3.00x" invites reading the peak as something that was held.
        let p = ExitPolicy {
            trailing_bps: Some(2_500),
            ..none()
        };
        let m = Mark::new(10_000, 0).observe(30_000, 10);
        let Exit::Sell { detail, .. } = decide(&p, m.observe(20_000, 20)) else {
            panic!("should have sold");
        };
        assert!(detail.contains("never captured"), "{detail}");
    }

    // --- max hold -------------------------------------------------------------------

    #[test]
    fn max_hold_closes_whatever_the_mark_is() {
        let p = ExitPolicy {
            max_hold_secs: Some(2_700),
            ..none()
        };
        assert!(decide(&p, Mark::new(50_000, 2_699)).is_hold());
        for mult in [0, 9_000, 50_000] {
            let e = decide(&p, Mark::new(mult, 2_700));
            assert_eq!(rule(&e), "max_hold", "at {mult}");
            assert_eq!(sold(&e), 10_000);
        }
    }

    // --- partials -------------------------------------------------------------------

    #[test]
    fn a_partial_sells_its_fraction_of_the_original_position() {
        let p = ExitPolicy {
            partials: vec![PartialExit {
                at_multiple_bps: 20_000,
                sell_bps: 5_000,
            }],
            ..none()
        };
        let e = decide(&p, Mark::new(20_000, 10));
        assert_eq!(sold(&e), 5_000, "half of the original");
        assert_eq!(rule(&e), "partial");
    }

    #[test]
    fn a_partial_fires_at_most_once() {
        let p = ExitPolicy {
            partials: vec![PartialExit {
                at_multiple_bps: 20_000,
                sell_bps: 5_000,
            }],
            ..none()
        };
        let mut m = Mark::new(20_000, 10);
        assert_eq!(sold(&decide(&p, m)), 5_000);
        // After selling half, the same rule must not sell again.
        m.remaining_bps = 5_000;
        assert!(decide(&p, m).is_hold(), "a partial must not repeat");
        // Nor at a higher mark.
        assert!(decide(&p, m.observe(90_000, 20)).is_hold());
    }

    #[test]
    fn partials_fire_in_ascending_order_and_each_leaves_the_right_remainder() {
        let p = ExitPolicy {
            // Deliberately out of order in the config.
            partials: vec![
                PartialExit {
                    at_multiple_bps: 50_000,
                    sell_bps: 2_500,
                },
                PartialExit {
                    at_multiple_bps: 20_000,
                    sell_bps: 5_000,
                },
            ],
            ..none()
        };
        let mut m = Mark::new(60_000, 10);
        // Even at 6x, the lowest untaken partial goes first.
        let first = decide(&p, m);
        assert_eq!(sold(&first), 5_000);

        m.remaining_bps = 5_000;
        let second = decide(&p, m);
        assert_eq!(
            sold(&second),
            2_500,
            "25% of the ORIGINAL, not of what is left"
        );

        m.remaining_bps = 2_500;
        assert!(decide(&p, m).is_hold(), "both partials are taken");
    }

    #[test]
    fn a_partial_tops_up_to_its_cumulative_target_rather_than_selling_its_full_size() {
        let p = ExitPolicy {
            partials: vec![PartialExit {
                at_multiple_bps: 20_000,
                sell_bps: 8_000,
            }],
            ..none()
        };
        // 70% has already gone, so this partial's job is the remaining 10% of its 80%
        // target -- not another 80%, which there is not enough position for.
        let m = Mark {
            remaining_bps: 3_000,
            ..Mark::new(20_000, 10)
        };
        assert_eq!(sold(&decide(&p, m)), 1_000);
    }

    #[test]
    fn partials_that_add_up_to_more_than_the_position_sell_what_is_left_and_stop() {
        // A user can write `60% at 2x, 60% at 5x`. Nothing rejects it, and the honest
        // behaviour is to sell what remains rather than to attempt 120% of a position.
        let p = ExitPolicy {
            partials: vec![
                PartialExit {
                    at_multiple_bps: 20_000,
                    sell_bps: 6_000,
                },
                PartialExit {
                    at_multiple_bps: 50_000,
                    sell_bps: 6_000,
                },
            ],
            ..none()
        };
        let mut m = Mark::new(60_000, 10);
        assert_eq!(sold(&decide(&p, m)), 6_000);

        m.remaining_bps = 4_000;
        assert_eq!(
            sold(&decide(&p, m)),
            4_000,
            "the second partial wants 6000 more and there are only 4000 left"
        );

        m.remaining_bps = 0;
        assert!(decide(&p, m).is_hold());
    }

    #[test]
    fn a_partial_already_covered_by_earlier_selling_does_nothing() {
        // 95% has gone; a 90% partial has no work left to do.
        let p = ExitPolicy {
            partials: vec![PartialExit {
                at_multiple_bps: 20_000,
                sell_bps: 9_000,
            }],
            ..none()
        };
        let m = Mark {
            remaining_bps: 500,
            ..Mark::new(20_000, 10)
        };
        assert!(decide(&p, m).is_hold());
    }

    #[test]
    fn a_later_partial_waits_for_its_own_trigger() {
        let p = ExitPolicy {
            partials: vec![
                PartialExit {
                    at_multiple_bps: 20_000,
                    sell_bps: 5_000,
                },
                PartialExit {
                    at_multiple_bps: 50_000,
                    sell_bps: 2_500,
                },
            ],
            ..none()
        };
        // The first is taken; the mark is well short of the second.
        let m = Mark {
            remaining_bps: 5_000,
            ..Mark::new(25_000, 10)
        };
        assert!(decide(&p, m).is_hold());
    }

    #[test]
    fn nothing_fires_on_a_fully_sold_position() {
        let p = ExitPolicy::default();
        let m = Mark {
            remaining_bps: 0,
            ..Mark::new(1, 99_999)
        };
        assert!(decide(&p, m).is_hold());
    }

    // --- precedence -----------------------------------------------------------------

    #[test]
    fn a_partial_above_the_take_profit_never_fires() {
        // Documented rather than rejected: the user is allowed to write it, and the
        // behaviour that follows should be predictable rather than clever.
        let p = ExitPolicy {
            take_profit_bps: Some(8_000), // out at 1.80x
            partials: vec![PartialExit {
                at_multiple_bps: 20_000, // never reached while holding
                sell_bps: 5_000,
            }],
            ..none()
        };
        assert_eq!(rule(&decide(&p, Mark::new(20_000, 10))), "take_profit");
        assert!(decide(&p, Mark::new(17_000, 10)).is_hold());
    }

    #[test]
    fn getting_out_wins_over_trimming() {
        // The mark is past a partial AND at the stop. Closing is never the wrong answer
        // to two rules disagreeing.
        let p = ExitPolicy {
            stop_loss_bps: Some(3_500),
            partials: vec![PartialExit {
                at_multiple_bps: 5_000,
                sell_bps: 5_000,
            }],
            ..none()
        };
        let e = decide(&p, Mark::new(6_000, 10));
        assert_eq!(rule(&e), "stop_loss");
        assert_eq!(sold(&e), 10_000);
    }

    #[test]
    fn the_stop_loss_wins_over_the_take_profit_when_both_somehow_fire() {
        // Only reachable with a contradictory config, which the user is allowed to write.
        // The one that gets them out at the worse price is the safe resolution.
        let p = ExitPolicy {
            take_profit_bps: Some(0),
            stop_loss_bps: Some(0),
            ..none()
        };
        assert_eq!(rule(&decide(&p, Mark::new(10_000, 10))), "stop_loss");
    }

    #[test]
    fn the_shipped_default_policy_behaves_as_documented() {
        // bodkin's defaults, so a user recognises the baseline: out at 1.80x, stopped at
        // 0.65x, trailing 0.25x from the peak, closed after 45 minutes.
        let p = ExitPolicy::default();
        assert_eq!(rule(&decide(&p, Mark::new(18_000, 10))), "take_profit");
        assert_eq!(rule(&decide(&p, Mark::new(6_500, 10))), "stop_loss");
        assert_eq!(rule(&decide(&p, Mark::new(12_000, 2_700))), "max_hold");
        let m = Mark::new(10_000, 0).observe(17_000, 10);
        assert_eq!(rule(&decide(&p, m.observe(14_500, 20))), "trailing");
        assert!(decide(&p, Mark::new(12_000, 10)).is_hold());
    }

    #[test]
    fn every_sell_names_a_rule_and_a_value() {
        let p = ExitPolicy::default();
        let cases = [
            Mark::new(18_000, 10),
            Mark::new(6_000, 10),
            Mark::new(11_000, 9_999),
        ];
        for m in cases {
            let Exit::Sell { rule, detail, .. } = decide(&p, m) else {
                panic!("expected a sell at {m:?}");
            };
            assert!(!rule.is_empty());
            assert!(
                detail.chars().any(|c| c.is_ascii_digit()),
                "spec §3.4 wants the values, not just the rule: {detail}"
            );
        }
    }
}
