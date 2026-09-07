//! Waiting for the opening tax to decay, and measuring what was actually paid.
//!
//! Spec §7: "poll `currentSnipeTaxBps(your_address)` and buy under `max_tax_bps`.
//! Adaptive interval: ~150ms baseline, tighten toward ~50ms as the decay approaches the
//! ceiling, relax outside the window. Target KPI: entry tax ≈ 0.19%."
//!
//! # This does not assume how the decay is shaped
//!
//! The factory reports the tax starting at 9900 bps and reaching zero over 3 seconds
//! (`doctor --probe`, `docs/FINDINGS.md` §1), but nothing published says the curve between
//! those points is a straight line, and the reference implementation polls rather than
//! computing — which suggests they did not know either. So [`Schedule`] extrapolates from
//! the two most recent **observations** and never from a model. If the decay turns out to
//! be a step, a curve, or something that changes with a factory upgrade, this still works;
//! it just polls slightly more.
//!
//! The one thing it will not do is sleep past the crossing on an extrapolation. Waking up
//! early costs a request. Waking up late costs the entry.
//!
//! # The KPI is measured, not asserted
//!
//! PLAN.md F7, confirmed with the user: report the achieved entry tax rather than gating
//! on it. The number bodkin quotes — 0.19% against a 3% ceiling — cannot come from buying
//! at the moment the tax crosses the ceiling, because that would pay 3%. It comes from
//! the gap between deciding and landing: the transaction arrives a block or two later and
//! the tax has kept decaying. That makes the achieved figure a property of the chain and
//! the endpoint as much as of this code, so the honest thing is to record what each entry
//! actually paid and let the number speak. [`Achieved`] is that record.

use std::time::Duration;

use alloy_primitives::U256;
use quarrel_core::{BPS, Bps};
use serde::{Deserialize, Serialize};

/// Poll spacing bounds.
///
/// The baseline and floor are the specification's, and they are consistent with the
/// measured 101 ms block time: polling faster than a block produces the same answer twice.
/// The floor sits just under one block so a crossing is never missed by a whole block.
const FLOOR: Duration = Duration::from_millis(50);
const BASELINE: Duration = Duration::from_millis(150);
const CEILING: Duration = Duration::from_millis(400);

/// How long before the projected crossing to be awake. One block plus a margin.
const LEAD_MS: u64 = 150;

/// One reading of `currentSnipeTaxBps`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    /// Milliseconds since the sniper started watching this launch.
    pub at_ms: u64,
    pub tax_bps: Bps,
}

/// What to do after an observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// The tax is at or under the ceiling. Buy now.
    Buy { tax_bps: Bps },
    /// Not yet; look again after this long.
    Wait { for_: Duration },
    /// The window has been open longer than `max_wait_ms` and the tax never came down.
    GiveUp { last_bps: Bps, waited_ms: u64 },
}

/// Decides when to look again, from observations alone.
#[derive(Debug, Clone, Default)]
pub struct Schedule {
    previous: Option<Observation>,
}

impl Schedule {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold in an observation and say what to do next.
    pub fn step(&mut self, obs: Observation, ceiling_bps: Bps, max_wait_ms: u64) -> Step {
        let previous = self.previous.replace(obs);

        if obs.tax_bps <= ceiling_bps {
            return Step::Buy {
                tax_bps: obs.tax_bps,
            };
        }
        if obs.at_ms >= max_wait_ms {
            return Step::GiveUp {
                last_bps: obs.tax_bps,
                waited_ms: obs.at_ms,
            };
        }

        // With two readings, project when the tax will reach the ceiling at the rate it
        // has actually been falling, and be awake `LEAD_MS` before that.
        let wait = match previous {
            Some(prev) => match projected_crossing(prev, obs, ceiling_bps) {
                Some(ms_until) => Duration::from_millis(ms_until.saturating_sub(LEAD_MS)),
                // Flat or rising: nothing to extrapolate from, so fall back.
                None => BASELINE,
            },
            None => BASELINE,
        };

        // Never sleep past the deadline either.
        let remaining = Duration::from_millis(max_wait_ms.saturating_sub(obs.at_ms));
        Step::Wait {
            for_: wait.clamp(FLOOR, CEILING).min(remaining.max(FLOOR)),
        }
    }
}

/// Milliseconds until the tax is projected to reach `ceiling`, at the observed rate.
///
/// `None` when the tax is not falling, which is the only case where a projection would be
/// meaningless rather than merely imprecise.
fn projected_crossing(prev: Observation, now: Observation, ceiling: Bps) -> Option<u64> {
    let dt = now.at_ms.checked_sub(prev.at_ms)?;
    if dt == 0 {
        return None;
    }
    let fallen = prev.tax_bps.checked_sub(now.tax_bps)?;
    if fallen == 0 {
        return None;
    }
    let to_go = now.tax_bps.checked_sub(ceiling)?;
    // bps per ms, kept as a ratio so there is no floating point: ms = to_go * dt / fallen.
    Some((to_go as u64).saturating_mul(dt) / fallen as u64)
}

/// What one entry actually paid (PLAN.md F7).
///
/// Read from the transaction's own events, not from what was intended: `quote_in` is what
/// was sent and `snipe_tax` is what the curve took, both from the `CurveBuy` and
/// `SnipeTaxCharged` logs the buy emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Achieved {
    pub quote_in: U256,
    pub snipe_tax: U256,
    /// What the tax was when the decision to buy was made.
    pub decided_at_bps: Bps,
}

impl Achieved {
    /// The tax actually paid, in basis points of the amount sent.
    ///
    /// `None` when nothing was sent, which is not a zero-tax entry but a non-entry.
    pub fn paid_bps(&self) -> Option<Bps> {
        if self.quote_in.is_zero() {
            return None;
        }
        let scaled = self.snipe_tax.checked_mul(U256::from(BPS))?;
        (scaled / self.quote_in).try_into().ok()
    }

    /// How much better the landed transaction did than the moment it was decided.
    ///
    /// Positive means the tax kept decaying between the decision and the block — which is
    /// where bodkin's 0.19% against a 3% ceiling comes from, and the reason this is
    /// reported rather than assumed.
    pub fn decay_gain_bps(&self) -> Option<i64> {
        Some(self.decided_at_bps as i64 - self.paid_bps()? as i64)
    }
}

/// Running KPI across a session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaxReport {
    pub entries: u64,
    total_quote: U256,
    total_tax: U256,
    pub worst_bps: Bps,
}

impl TaxReport {
    pub fn record(&mut self, a: Achieved) {
        self.entries += 1;
        self.total_quote = self.total_quote.saturating_add(a.quote_in);
        self.total_tax = self.total_tax.saturating_add(a.snipe_tax);
        if let Some(bps) = a.paid_bps() {
            self.worst_bps = self.worst_bps.max(bps);
        }
    }

    /// Tax paid across the session, weighted by size rather than averaged per entry.
    ///
    /// A mean of per-entry rates would let a dust buy at 90% weigh as much as the real
    /// one at 0.1%. What the user lost is the ratio of the totals.
    pub fn weighted_bps(&self) -> Option<Bps> {
        if self.total_quote.is_zero() {
            return None;
        }
        let scaled = self.total_tax.checked_mul(U256::from(BPS))?;
        (scaled / self.total_quote).try_into().ok()
    }

    /// The reference the specification asks not to regress on: 0.19%, 19 bps.
    pub const BODKIN_BPS: Bps = 19;

    /// A sentence for the status view. States the comparison rather than passing a test:
    /// this is a measurement, and a session of three entries proves very little.
    pub fn summary(&self) -> String {
        match self.weighted_bps() {
            None => "no entries yet, so no entry tax to report".into(),
            Some(bps) => format!(
                "entry tax {}.{:02}% across {} entries, worst {}.{:02}% \
                 (the reference implementation measured 0.19%; {} entries is a small sample)",
                bps / 100,
                bps % 100,
                self.entries,
                self.worst_bps / 100,
                self.worst_bps % 100,
                self.entries
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(at_ms: u64, tax_bps: Bps) -> Observation {
        Observation { at_ms, tax_bps }
    }

    #[test]
    fn a_tax_already_under_the_ceiling_buys_immediately() {
        let mut s = Schedule::new();
        assert_eq!(s.step(obs(0, 250), 300, 12_000), Step::Buy { tax_bps: 250 });
    }

    #[test]
    fn the_first_observation_falls_back_to_the_baseline_interval() {
        // Nothing to extrapolate from yet.
        let mut s = Schedule::new();
        assert_eq!(
            s.step(obs(0, 9_900), 300, 12_000),
            Step::Wait { for_: BASELINE }
        );
    }

    #[test]
    fn the_wait_is_projected_from_the_rate_the_tax_is_actually_falling() {
        // 9900 -> 6600 in 1000 ms is 3.3 bps/ms. From 6600 to the 300 ceiling is 6300
        // bps, so about 1909 ms, less the 150 ms lead.
        let mut s = Schedule::new();
        s.step(obs(0, 9_900), 300, 12_000);
        let Step::Wait { for_ } = s.step(obs(1_000, 6_600), 300, 12_000) else {
            panic!("should still be waiting");
        };
        // Clamped to the 400 ms ceiling, because a long sleep on an extrapolation is how
        // a decay that is not a straight line gets missed.
        assert_eq!(for_, CEILING);
    }

    #[test]
    fn the_interval_tightens_as_the_crossing_approaches() {
        let mut s = Schedule::new();
        s.step(obs(0, 9_900), 300, 12_000);
        // 400 bps to go, falling 3.3 bps/ms: ~121 ms, minus the lead, floored at 50.
        let Step::Wait { for_ } = s.step(obs(2_879, 700), 300, 12_000) else {
            panic!("should still be waiting");
        };
        assert_eq!(for_, FLOOR, "close to the ceiling, poll at the floor");
    }

    #[test]
    fn the_interval_never_goes_below_the_floor_or_above_the_ceiling() {
        let mut s = Schedule::new();
        for (prev, now) in [
            ((0, 9_900), (10, 9_899)), // barely moving: would project hours
            ((0, 9_900), (10, 400)),   // plummeting: would project microseconds
        ] {
            let mut s2 = Schedule::new();
            s2.step(obs(prev.0, prev.1), 300, 12_000);
            if let Step::Wait { for_ } = s2.step(obs(now.0, now.1), 300, 12_000) {
                assert!((FLOOR..=CEILING).contains(&for_), "{for_:?}");
            }
        }
        s.step(obs(0, 9_900), 300, 12_000);
    }

    #[test]
    fn a_flat_or_rising_tax_falls_back_rather_than_projecting_nonsense() {
        let mut s = Schedule::new();
        s.step(obs(0, 5_000), 300, 12_000);
        // Flat: no rate to extrapolate.
        assert_eq!(
            s.step(obs(200, 5_000), 300, 12_000),
            Step::Wait { for_: BASELINE }
        );
        // Rising: the projection would point backwards.
        let mut s = Schedule::new();
        s.step(obs(0, 5_000), 300, 12_000);
        assert_eq!(
            s.step(obs(200, 6_000), 300, 12_000),
            Step::Wait { for_: BASELINE }
        );
    }

    #[test]
    fn waiting_stops_at_max_wait_rather_than_forever() {
        let mut s = Schedule::new();
        s.step(obs(0, 9_900), 300, 3_000);
        assert_eq!(
            s.step(obs(3_000, 9_900), 300, 3_000),
            Step::GiveUp {
                last_bps: 9_900,
                waited_ms: 3_000
            }
        );
    }

    #[test]
    fn the_sleep_never_runs_past_the_deadline() {
        let mut s = Schedule::new();
        s.step(obs(0, 9_900), 300, 12_000);
        // 80 ms left before giving up; the projection would happily sleep 400.
        let Step::Wait { for_ } = s.step(obs(11_920, 9_000), 300, 12_000) else {
            panic!("should be waiting");
        };
        assert!(for_ <= Duration::from_millis(80).max(FLOOR), "{for_:?}");
    }

    /// The whole point, simulated against the measured decay.
    #[test]
    fn the_schedule_enters_within_a_poll_of_the_crossing() {
        // A linear 9900 -> 0 over 3000 ms. Not an assumption the code makes -- it is a
        // stand-in so the polling behaviour can be measured against something.
        let tax_at = |ms: u64| -> Bps {
            if ms >= 3_000 {
                0
            } else {
                (9_900 * (3_000 - ms) / 3_000) as Bps
            }
        };
        let ceiling = 300;
        let mut s = Schedule::new();
        let mut now = 0u64;
        let mut polls = 0;
        let bought_at = loop {
            polls += 1;
            match s.step(obs(now, tax_at(now)), ceiling, 12_000) {
                Step::Buy { tax_bps } => break (now, tax_bps),
                Step::Wait { for_ } => now += for_.as_millis() as u64,
                Step::GiveUp { .. } => panic!("gave up on a decay that reaches zero"),
            }
        };
        // The crossing is at 2909 ms. Entering within one floor-interval of it, and never
        // above the ceiling.
        assert!(bought_at.1 <= ceiling, "bought above the ceiling");
        assert!(
            bought_at.0 >= 2_909 && bought_at.0 <= 2_909 + FLOOR.as_millis() as u64,
            "entered at {} ms, expected within 50 ms of 2909",
            bought_at.0
        );
        assert!(polls < 30, "{polls} polls to cover 3 seconds is too many");
    }

    // --- the achieved tax -----------------------------------------------------------

    #[test]
    fn the_paid_tax_is_read_from_what_the_transaction_actually_did() {
        let a = Achieved {
            quote_in: U256::from(10_000_000u64),
            snipe_tax: U256::from(19_000u64),
            decided_at_bps: 300,
        };
        assert_eq!(a.paid_bps(), Some(19), "0.19%");
        // The gap between deciding and landing, which is where the figure comes from.
        assert_eq!(a.decay_gain_bps(), Some(281));
    }

    #[test]
    fn an_entry_that_sent_nothing_has_no_tax_rate_rather_than_a_zero_one() {
        let a = Achieved {
            quote_in: U256::ZERO,
            snipe_tax: U256::ZERO,
            decided_at_bps: 300,
        };
        assert_eq!(a.paid_bps(), None, "a non-entry, not a free one");
    }

    #[test]
    fn the_session_figure_is_weighted_by_size_not_averaged_per_entry() {
        let mut r = TaxReport::default();
        // A tiny buy that paid 90%, and a real one that paid nothing.
        r.record(Achieved {
            quote_in: U256::from(100u64),
            snipe_tax: U256::from(90u64),
            decided_at_bps: 9_000,
        });
        r.record(Achieved {
            quote_in: U256::from(1_000_000u64),
            snipe_tax: U256::ZERO,
            decided_at_bps: 0,
        });
        // A per-entry mean would report 45%. What was actually lost is 90 of 1,000,100.
        assert_eq!(r.weighted_bps(), Some(0));
        assert_eq!(r.worst_bps, 9_000, "and the worst case is still reported");
        assert_eq!(r.entries, 2);
    }

    #[test]
    fn the_summary_states_the_comparison_and_the_sample_size() {
        let mut r = TaxReport::default();
        assert!(r.summary().contains("no entries yet"));

        r.record(Achieved {
            quote_in: U256::from(10_000u64),
            snipe_tax: U256::from(19u64),
            decided_at_bps: 300,
        });
        let s = r.summary();
        assert!(s.contains("0.19%"), "{s}");
        assert!(s.contains("reference implementation"), "{s}");
        assert!(
            s.contains("small sample"),
            "a KPI from one entry is not a KPI: {s}"
        );
        assert_eq!(TaxReport::BODKIN_BPS, 19);
    }
}
