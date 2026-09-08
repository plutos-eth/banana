//! Running a [`StrategyConfig`] over the store.
//!
//! The shape is deliberately dull: narrow in SQL, decide in Rust, count. There is no
//! selling, no position sizing and no path simulation — spec §5.6 — because the Lab
//! measures what became of the tokens a filter selected, and the moment it starts
//! simulating an exit it is measuring the exit instead.
//!
//! # The maturity cutoff measures the window, not the token
//!
//! This is the single easiest place to reintroduce survivorship bias, so it is worth being
//! explicit. A token is mature when **the index window kept watching it** for six hours
//! after it launched — `to_block - launch_block >= MATURITY_BLOCKS`. It is emphatically
//! *not* about how much trade history the token itself produced: a token that died in
//! ninety seconds has almost no trades, and gating on its own activity would drop exactly
//! the failures, leaving a universe of survivors and a hit rate that means nothing. The
//! regression test in `tests/denominator.rs` pins that distinction.

use std::time::Instant;

use banana_core::features::PitFeatures;
use banana_core::filter::EntryFilter;
use banana_core::strategy::{StrategyConfig, SuccessTarget};
use banana_core::{BPS, Bps};
use banana_store::History;
use banana_store::lab::{Candidate, Outcome};
use serde::{Deserialize, Serialize};

use crate::funnel::Funnel;
use crate::metrics::{
    HoldStats, Measured, PeakStats, Percentiles, REGIME_WARNING, Results, WindowInfo,
};

/// Measured block time, 100.87 ms (`docs/FINDINGS.md` §1).
const BLOCK_MS: u64 = 101;

/// Spec §5.5's six hours, in blocks.
///
/// Confirmed against the window rather than assumed: p90 time-to-peak is 101 seconds and
/// the slowest of 57 observed migrations took 3.1 hours, so six hours is about twice the
/// binding horizon (`docs/FINDINGS.md` §9).
pub const MATURITY_HOURS: u64 = 6;

pub fn maturity_blocks() -> u64 {
    MATURITY_HOURS * 3_600 * 1_000 / BLOCK_MS
}

/// PLAN.md C2's deployer-history floor, in blocks.
///
/// **Still a guess.** It could not be derived from the 5.6-hour window that raised the
/// question — the relationship between depth and visible deployer history was still rising
/// at 200,000 blocks with no plateau, and a window shorter than the threshold cannot settle
/// the threshold. The funnel therefore reports what it costs, so the guess is at least
/// visible rather than silent.
pub const DEPLOYER_DEPTH_HOURS: u64 = 12;

pub fn deployer_depth_blocks() -> u64 {
    DEPLOYER_DEPTH_HOURS * 3_600 * 1_000 / BLOCK_MS
}

#[derive(Debug, thiserror::Error)]
pub enum BacktestError {
    #[error(transparent)]
    Store(#[from] banana_store::StoreError),
    #[error("the store is empty; run `banana index` first")]
    EmptyStore,
    #[error(
        "a {minutes}-minute hold is not precomputed; the store holds 5 and 30 minutes, and \
         adding another means re-running the index"
    )]
    UnsupportedHold { minutes: u32 },
}

type Result<T> = std::result::Result<T, BacktestError>;

/// A complete backtest result. Every §5.5 guard is a required field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BacktestResult {
    pub window: WindowInfo,
    /// Spec §5.5: mandatory, not an optional chart.
    pub funnel: Funnel,
    pub results: Results,
    /// Spec §5.5: always present, in the primary reading path.
    pub regime_warning: String,
    /// PLAN.md D2: the light half's query time, reported rather than asserted.
    pub query_ms: u64,
    /// True when the strategy reads a deployer feature, so the C2 depth floor applied.
    pub deployer_depth_applied: bool,
}

/// Run `config` over everything in `history`.
pub fn run(history: &History, config: &StrategyConfig) -> Result<BacktestResult> {
    let started = Instant::now();

    let window = history.window()?.ok_or(BacktestError::EmptyStore)?;
    // The pre-filter is sound by construction: it may return extra rows, never fewer, so
    // the evaluator below still sees every launch that could pass. Its only job is to keep
    // SQLite from handing us rows nothing will want.
    let prefilter = banana_store::push_down(&config.entry_filter);
    let candidates = history.candidates(Some(&prefilter))?;
    let query_ms = started.elapsed().as_millis() as u64;

    // The universe is every launch in the window, not every launch the pre-filter kept.
    // Counting the narrowed set here would quietly turn the funnel's first number into
    // "launches SQL bothered to return", which is not a fact about the chain.
    let all_launches = history.launch_count()?;
    let mut funnel = Funnel::new(all_launches);

    // --- §5.5 maturity, and PLAN.md C2 depth ------------------------------------------
    //
    // The early stages are counted in SQL over the *whole* store, not over what the
    // pre-filter returned. Counting the narrowed set would make "matured" mean "matured,
    // among the rows SQL bothered to return", and the funnel's job is to let a reader add
    // up the column and land back on the universe.
    let cutoff = maturity_blocks();
    let uses_deployer = config.entry_filter.uses_deployer_history();
    let floor = deployer_depth_blocks();
    let counts = history.universe_counts(window.to_block, cutoff, floor)?;

    let mature: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| window.to_block.saturating_sub(c.launch_block) >= cutoff)
        .collect();
    funnel.narrow(
        "matured",
        format!("had at least {MATURITY_HOURS} h of subsequent window to be judged in"),
        counts.matured,
    );

    let deep_enough: Vec<&Candidate> = if uses_deployer {
        funnel.narrow(
            "deployer_depth",
            format!(
                "had at least {DEPLOYER_DEPTH_HOURS} h of visible deployer history \
                 (applied because this strategy reads a deployer feature)"
            ),
            counts.matured_and_deep,
        );
        mature
            .into_iter()
            .filter(|c| c.features.deployer_history_depth_blocks >= floor)
            .collect()
    } else {
        mature
    };

    // --- the filter --------------------------------------------------------------------
    let passed: Vec<&Candidate> = deep_enough
        .into_iter()
        .filter(|c| decide(&config.entry_filter, &c.features))
        .collect();
    funnel.narrow(
        "passed_filter",
        "passed every entry rule",
        passed.len() as u64,
    );

    // --- PLAN.md F9: priced ------------------------------------------------------------
    let priced: Vec<&Candidate> = passed
        .iter()
        .copied()
        .filter(|c| c.outcome.has_entry)
        .collect();
    funnel.narrow(
        "priced",
        "had an entry price that could be established or replayed exactly",
        priced.len() as u64,
    );

    // Both of these narrow `priced`, and neither narrows the other. A token can migrate
    // without having doubled inside five minutes, so chaining them would assert a
    // containment that does not hold (see `funnel::Stage::of`).
    let target = &config.success_target;
    let hits = count_hits(&priced, target)?;
    funnel.branch("reached_target", describe(target)?, "priced", hits);

    let migrations = priced.iter().filter(|c| c.outcome.migrated).count() as u64;
    funnel.branch("migrated", "graduated to a v4 pool", "priced", migrations);

    // The gate counts what *passed the filter*, not what could be priced. A strategy that
    // selects 31 tokens of which one cannot be priced has still selected 31, and hiding
    // behind the smaller number would be the flattering direction.
    let sample = passed.len() as u64;
    let denominator = priced.len() as u64;
    let results = Results::gate(sample, || Measured {
        target: describe(target).unwrap_or_default(),
        target_is_peak_based: target.is_peak_based(),
        hits,
        measured_over: denominator,
        hit_rate_bps: rate_bps(hits, denominator),
        hold_5m: hold_stats(&priced, 5),
        hold_30m: hold_stats(&priced, 30),
        peak: peak_stats(&priced),
        migrations,
    });

    Ok(BacktestResult {
        window: WindowInfo {
            from_block: window.from_block,
            to_block: window.to_block,
            hours_x10: window.blocks() * BLOCK_MS / 360_000,
            maturity_cutoff_hours: MATURITY_HOURS,
        },
        funnel,
        results,
        regime_warning: REGIME_WARNING.to_owned(),
        query_ms,
        deployer_depth_applied: uses_deployer,
    })
}

/// The one evaluator. Both halves of the product call this same function on this same type.
fn decide(filter: &EntryFilter, f: &PitFeatures) -> bool {
    filter.evaluate(f).passed
}

fn rate_bps(hits: u64, over: u64) -> Bps {
    if over == 0 {
        return 0;
    }
    ((hits * BPS as u64) / over) as Bps
}

/// The store precomputes two holding periods; anything else needs a re-index.
fn check_hold(minutes: u32) -> Result<()> {
    match minutes {
        5 | 30 => Ok(()),
        other => Err(BacktestError::UnsupportedHold { minutes: other }),
    }
}

fn hold_multiple(o: &Outcome, minutes: u32) -> Result<Option<u64>> {
    check_hold(minutes)?;
    Ok(match minutes {
        5 => o.mult_after_5m_bps,
        _ => o.mult_after_30m_bps,
    })
}

fn count_hits(rows: &[&Candidate], target: &SuccessTarget) -> Result<u64> {
    let mut n = 0u64;
    for c in rows {
        let hit = match target {
            SuccessTarget::FixedHoldMultiple {
                minutes,
                multiple_bps,
            } => hold_multiple(&c.outcome, *minutes)?.is_some_and(|m| m >= *multiple_bps as u64),
            SuccessTarget::ReachedMigration => c.outcome.migrated,
            SuccessTarget::MaxMultiple { multiple_bps } => c
                .outcome
                .max_multiple_bps
                .is_some_and(|m| m >= *multiple_bps as u64),
        };
        n += hit as u64;
    }
    Ok(n)
}

/// The target in words, including the holding assumption it rests on (§5.4).
fn describe(target: &SuccessTarget) -> Result<String> {
    Ok(match target {
        SuccessTarget::FixedHoldMultiple {
            minutes,
            multiple_bps,
        } => {
            // Validate the hold here too, so an unsupported one fails before it is
            // described rather than being described and then silently counted as zero.
            check_hold(*minutes)?;
            format!(
                "reached {} when held for exactly {minutes} minutes from entry",
                x(*multiple_bps)
            )
        }
        SuccessTarget::ReachedMigration => "graduated to a v4 pool".into(),
        SuccessTarget::MaxMultiple { multiple_bps } => format!(
            "peaked at {} at some point — a peak nobody captured, not a result of holding",
            x(*multiple_bps)
        ),
    })
}

/// Basis points as a multiple: 20000 -> "2.00x". Integer arithmetic only (spec §12).
fn x(bps: Bps) -> String {
    format!("{}.{:02}x", bps / 10_000, (bps % 10_000) / 100)
}

fn hold_stats(rows: &[&Candidate], minutes: u32) -> HoldStats {
    let values: Vec<u64> = rows
        .iter()
        .filter_map(|c| hold_multiple(&c.outcome, minutes).ok().flatten())
        .collect();
    HoldStats {
        assumption: format!(
            "bought at the entry price and sold {minutes} minutes later, whatever happened in between"
        ),
        measured_over: values.len() as u64,
        above_entry: values.iter().filter(|m| **m > BPS as u64).count() as u64,
        below_entry: values.iter().filter(|m| **m < BPS as u64).count() as u64,
        multiple: Percentiles::of(values).unwrap_or_default(),
    }
}

fn peak_stats(rows: &[&Candidate]) -> PeakStats {
    let values: Vec<u64> = rows
        .iter()
        .filter_map(|c| c.outcome.max_multiple_bps)
        .collect();
    PeakStats {
        label: PeakStats::LABEL.to_owned(),
        measured_over: values.len() as u64,
        never_above_entry: values.iter().filter(|m| **m <= BPS as u64).count() as u64,
        multiple: Percentiles::of(values).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn six_hours_is_about_two_hundred_thousand_blocks() {
        assert_eq!(maturity_blocks(), 213_861);
    }

    #[test]
    fn multiples_render_without_floating_point() {
        assert_eq!(x(10_000), "1.00x");
        assert_eq!(x(20_000), "2.00x");
        assert_eq!(x(9_600), "0.96x");
        assert_eq!(x(1_500), "0.15x");
    }

    #[test]
    fn a_rate_over_nothing_is_zero_rather_than_a_division_by_zero() {
        assert_eq!(rate_bps(0, 0), 0);
        assert_eq!(rate_bps(1, 4), 2_500);
    }

    #[test]
    fn an_unsupported_hold_is_an_error_not_a_silent_zero() {
        let e = describe(&SuccessTarget::FixedHoldMultiple {
            minutes: 7,
            multiple_bps: 20_000,
        });
        assert!(matches!(
            e,
            Err(BacktestError::UnsupportedHold { minutes: 7 })
        ));
    }

    #[test]
    fn a_peak_target_describes_itself_as_a_peak() {
        let d = describe(&SuccessTarget::MaxMultiple {
            multiple_bps: 30_000,
        })
        .unwrap();
        assert!(d.contains("nobody captured"));
        assert!(!d.contains("profit"));
    }
}
