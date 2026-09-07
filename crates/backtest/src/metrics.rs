//! What a backtest reports, and the shapes that stop it lying.
//!
//! Two spec sections are enforced by the *types* here rather than by the code that fills
//! them in, because a guard a caller can forget is not a guard:
//!
//! * **§5.4 — peak is not profit.** The fixed-hold multiple is a required field and is
//!   always computed; the peak is a separate type that carries its own label and cannot be
//!   serialised without it. A test asserts the words "profit", "return", "earned" and
//!   "gain" appear nowhere in a serialised result.
//! * **§5.5 — sample-size gate.** Below thirty passing tokens the result is a *different
//!   variant*, not a populated one with a flag set. There is no percentage field to hide,
//!   because at that sample size the struct that holds percentages is never constructed.
//!
//! Measured on the indexed window, the median launch's peak is exactly 1.00x and 56.6% of
//! launches never trade above entry (`docs/FINDINGS.md` §9). That is the distribution these
//! numbers describe, and it is why the framing is not decoration.

use quarrel_core::Bps;
use serde::{Deserialize, Serialize};

/// Spec §5.5: below this many passing tokens, no percentage is reported at all.
pub const MIN_SAMPLE: u64 = 30;

/// Five-number summary, in basis points of the entry price. 10000 = 1.00x.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Percentiles {
    pub p10: u64,
    pub p25: u64,
    pub p50: u64,
    pub p75: u64,
    pub p90: u64,
}

impl Percentiles {
    /// Integer percentiles by nearest rank. No floating point in the reported figures.
    ///
    /// Rounds to nearest rather than up, which is the convention `indexer::verify` already
    /// uses, so a figure here and the same figure in `docs/FINDINGS.md` are comparable.
    pub fn of(mut v: Vec<u64>) -> Option<Self> {
        if v.is_empty() {
            return None;
        }
        v.sort_unstable();
        let at = |num: u64, den: u64| -> u64 {
            let last = (v.len() - 1) as u64;
            v[((last * num + den / 2) / den) as usize]
        };
        Some(Self {
            p10: at(10, 100),
            p25: at(25, 100),
            p50: at(50, 100),
            p75: at(75, 100),
            p90: at(90, 100),
        })
    }
}

/// What a fixed holding period would have produced.
///
/// This is the §5.4 headline: the number a simple, executable rule actually returns to a
/// person who buys and sells on a clock, as opposed to one who sells at a peak they could
/// not have identified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoldStats {
    /// The holding assumption in words. §5.4: every result panel states the one it rests on.
    pub assumption: String,
    /// Launches for which this hold could be measured at all.
    pub measured_over: u64,
    pub multiple: Percentiles,
    /// How many were above their entry price at the end of the hold.
    pub above_entry: u64,
    pub below_entry: u64,
}

/// The best multiple that *existed*. Secondary, and labelled (§5.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeakStats {
    /// Fixed wording, always serialised beside the numbers.
    pub label: String,
    pub measured_over: u64,
    pub multiple: Percentiles,
    /// Launches whose peak was their entry: they never traded above it.
    pub never_above_entry: u64,
}

impl PeakStats {
    /// The one phrasing this figure is allowed to carry.
    pub const LABEL: &'static str = "max achievable / peak — the best multiple that existed, which nobody captured; \
         the peak lasts seconds and is unidentifiable in the moment";
}

/// The measurable part of a result. Only ever constructed above [`MIN_SAMPLE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Measured {
    /// What the hit rate counts, in words, including any holding assumption.
    pub target: String,
    /// True when the target is a peak figure, so the UI must not present it as achieved.
    pub target_is_peak_based: bool,
    pub hits: u64,
    /// The denominator. Spec §5.5 requires it beside every rate.
    pub measured_over: u64,
    pub hit_rate_bps: Bps,
    /// Always computed, whatever the target is: §5.4's headline. Both cost the same.
    pub hold_5m: HoldStats,
    pub hold_30m: HoldStats,
    pub peak: PeakStats,
    /// Raw count, always shown beside any rate: at ~1 launch in 89 a percentage alone
    /// invites reading noise as signal (PLAN.md F4).
    pub migrations: u64,
}

/// The result of a backtest, or the reason there is no number to show.
///
/// An enum rather than a struct with `Option` fields: at a sample below [`MIN_SAMPLE`]
/// there is no percentage field in the serialised form to accidentally render.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Results {
    /// Fewer than [`MIN_SAMPLE`] tokens passed. Spec §5.5: the number does not render.
    InsufficientSample {
        passed: u64,
        required: u64,
        /// Exactly what the UI shows in place of the figures.
        message: String,
    },
    /// Boxed because it is an order of magnitude larger than the other variant, and every
    /// caller holds a `Results` whether or not the sample was big enough.
    Measured(Box<Measured>),
}

impl Results {
    /// Build the right variant for the sample size. The only way to get a [`Measured`].
    pub fn gate(passed: u64, measured: impl FnOnce() -> Measured) -> Self {
        if passed < MIN_SAMPLE {
            Results::InsufficientSample {
                passed,
                required: MIN_SAMPLE,
                message: format!("sample too small — {passed} tokens passed"),
            }
        } else {
            Results::Measured(Box::new(measured()))
        }
    }

    pub fn measured(&self) -> Option<&Measured> {
        match self {
            Results::Measured(m) => Some(m),
            Results::InsufficientSample { .. } => None,
        }
    }
}

/// The window the result describes, and the cutoff applied to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub from_block: u64,
    pub to_block: u64,
    pub hours_x10: u64,
    /// The maturity cutoff in hours (spec §5.5).
    pub maturity_cutoff_hours: u64,
}

/// Spec §5.5's regime warning, as a required field.
///
/// Not a `Vec<Warning>` a caller might leave empty, and not a footnote: it is one string
/// that is always serialised beside the numbers, in the primary reading path.
pub const REGIME_WARNING: &str = "This is one window, and therefore one market regime. If the whole chain was rising \
     across it, every strategy looks good; if it was falling, every strategy looks bad. \
     A result here is evidence about this window, not about the strategy in general.";

#[cfg(test)]
mod tests {
    use super::*;

    fn hold(assumption: &str) -> HoldStats {
        HoldStats {
            assumption: assumption.into(),
            measured_over: 40,
            multiple: Percentiles::default(),
            above_entry: 10,
            below_entry: 30,
        }
    }

    fn measured() -> Measured {
        Measured {
            target: "reached 2.00x when held for exactly 5 minutes".into(),
            target_is_peak_based: false,
            hits: 10,
            measured_over: 40,
            hit_rate_bps: 2_500,
            hold_5m: hold("held for exactly 5 minutes from entry"),
            hold_30m: hold("held for exactly 30 minutes from entry"),
            peak: PeakStats {
                label: PeakStats::LABEL.into(),
                measured_over: 40,
                multiple: Percentiles::default(),
                never_above_entry: 22,
            },
            migrations: 1,
        }
    }

    #[test]
    fn percentiles_of_nothing_are_absent_rather_than_zero() {
        assert_eq!(Percentiles::of(vec![]), None);
    }

    #[test]
    fn percentiles_are_monotonic_and_integer() {
        let p = Percentiles::of((1..=100).collect()).unwrap();
        assert!(p.p10 <= p.p25 && p.p25 <= p.p50 && p.p50 <= p.p75 && p.p75 <= p.p90);
        assert_eq!(p.p50, 51);
    }

    #[test]
    fn one_value_is_every_percentile() {
        let p = Percentiles::of(vec![7]).unwrap();
        assert_eq!((p.p10, p.p50, p.p90), (7, 7, 7));
    }

    // --- §5.5: the sample gate ----------------------------------------------------------

    /// The acceptance criterion, asserted on the serialised response rather than the UI.
    #[test]
    fn at_twenty_nine_passing_tokens_no_percentage_field_is_serialised_at_all() {
        let r = Results::gate(29, measured);
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("sample too small — 29 tokens passed"));
        for forbidden in ["hit_rate", "bps", "percent", "p50", "multiple"] {
            assert!(
                !json.contains(forbidden),
                "a below-sample result must not carry `{forbidden}`: {json}"
            );
        }
        assert!(r.measured().is_none());
    }

    #[test]
    fn at_thirty_the_numbers_appear() {
        let r = Results::gate(30, measured);
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("hit_rate_bps"));
        assert!(r.measured().is_some());
    }

    // --- §5.4: peak is not profit -------------------------------------------------------

    /// The words that must never appear near these numbers, checked on the serialised form.
    #[test]
    fn a_serialised_result_never_uses_the_language_of_profit() {
        let json = serde_json::to_string(&Results::gate(30, measured))
            .unwrap()
            .to_lowercase();
        for word in ["profit", "return", "earned", "gain", "would have made"] {
            assert!(
                !json.contains(word),
                "§5.4 forbids `{word}` anywhere near these figures"
            );
        }
    }

    #[test]
    fn the_peak_cannot_be_serialised_without_its_label() {
        let json = serde_json::to_string(&measured().peak).unwrap();
        assert!(json.contains("nobody captured"));
        assert!(json.contains("unidentifiable in the moment"));
    }

    #[test]
    fn every_hold_states_its_assumption() {
        let m = measured();
        assert!(m.hold_5m.assumption.contains("5 minutes"));
        assert!(m.hold_30m.assumption.contains("30 minutes"));
    }

    #[test]
    fn the_regime_warning_says_what_it_means_without_hedging() {
        assert!(REGIME_WARNING.contains("one market regime"));
        assert!(REGIME_WARNING.contains("not about the strategy in general"));
    }

    #[test]
    fn results_round_trip_through_json() {
        for r in [Results::gate(29, measured), Results::gate(30, measured)] {
            let s = serde_json::to_string(&r).unwrap();
            assert_eq!(serde_json::from_str::<Results>(&s).unwrap(), r);
        }
    }
}
