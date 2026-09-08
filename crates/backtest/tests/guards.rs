//! The §5.5 honesty guards and the PLAN.md C2 funnel stage, as running checks.
//!
//! These are the phase-4 acceptance criteria. Each one is written so that it fails if the
//! guard is removed, not merely if it is misconfigured — a test that passes when the code
//! under it is deleted is decoration.

mod support;

use banana_backtest::metrics::Results;
use banana_backtest::run::{DEPLOYER_DEPTH_HOURS, MATURITY_HOURS, maturity_blocks};
use banana_core::features::Pair;
use banana_core::filter::{Condition, EntryFilter};
use banana_core::strategy::{StrategyConfig, SuccessTarget};
use support::{Fixture, Launch};

/// Everything passes this, so a test can isolate one guard at a time.
fn permissive() -> StrategyConfig {
    StrategyConfig {
        entry_filter: EntryFilter::All(vec![]),
        success_target: SuccessTarget::FixedHoldMultiple {
            minutes: 5,
            multiple_bps: 20_000,
        },
        ..StrategyConfig::default()
    }
}

fn with_filter(filter: EntryFilter) -> StrategyConfig {
    StrategyConfig {
        entry_filter: filter,
        ..permissive()
    }
}

/// A window long enough that a launch at block 1,000 has matured.
const TO_BLOCK: u64 = 1_000_000;

// --- §5.5 maturity cutoff -----------------------------------------------------------------

#[test]
fn launches_younger_than_the_cutoff_are_excluded_and_counted_separately() {
    let mut f = Fixture::new(0, TO_BLOCK);
    for i in 0..40 {
        f.add(Launch::at(1_000 + i));
    }
    // Six of these launched too close to the end of the window to be judged.
    for i in 0..6 {
        f.add(Launch::at(TO_BLOCK - 1_000 + i));
    }
    let h = f.finish();

    let r = banana_backtest::run(&h, &permissive()).unwrap();
    let all = r.funnel.stage("all_launches").unwrap();
    let matured = r.funnel.stage("matured").unwrap();

    assert_eq!(all.remaining, 46, "every launch is in the universe");
    assert_eq!(matured.remaining, 40);
    assert_eq!(
        matured.removed, 6,
        "the immature ones are counted, not dropped"
    );
    assert_eq!(r.window.maturity_cutoff_hours, MATURITY_HOURS);
}

/// The distinction that stops the cutoff reintroducing survivorship bias.
#[test]
fn maturity_is_about_the_window_not_about_how_much_the_token_traded() {
    // A token that died instantly has almost no trade history of its own. It is still
    // mature: the window watched it for six hours and it did nothing, which is a result.
    let mut f = Fixture::new(0, TO_BLOCK);
    for i in 0..30 {
        f.add(Launch {
            mult_5m_bps: Some(0),
            mult_30m_bps: Some(0),
            max_multiple_bps: Some(10_000),
            ..Launch::at(1_000 + i)
        });
    }
    let h = f.finish();
    let r = banana_backtest::run(&h, &permissive()).unwrap();

    assert_eq!(
        r.funnel.stage("matured").unwrap().remaining,
        30,
        "a token that died must stay in the denominator"
    );
}

// --- §5.5 sample gate ---------------------------------------------------------------------

#[test]
fn twenty_nine_passing_tokens_yield_no_percentage_field_in_the_response() {
    let h = passing_n(29);
    let r = banana_backtest::run(&h, &permissive()).unwrap();

    assert!(matches!(r.results, Results::InsufficientSample { .. }));

    // Asserted on the serialised response, not on the UI: there is no number to hide
    // because the struct that holds numbers was never built.
    let json = serde_json::to_string(&r.results).unwrap();
    assert!(json.contains("sample too small — 29 tokens passed"));
    for forbidden in ["hit_rate", "bps", "p50", "percent"] {
        assert!(!json.contains(forbidden), "leaked `{forbidden}`: {json}");
    }
}

#[test]
fn thirty_passing_tokens_is_the_threshold_at_which_numbers_appear() {
    let r = banana_backtest::run(&passing_n(30), &permissive()).unwrap();
    let json = serde_json::to_string(&r.results).unwrap();
    assert!(json.contains("hit_rate_bps"), "{json}");
}

/// The gate counts what passed the filter, not what could be priced.
#[test]
fn a_token_that_passed_but_could_not_be_priced_still_counts_toward_the_sample() {
    let mut f = Fixture::new(0, TO_BLOCK);
    for i in 0..29 {
        f.add(Launch::at(1_000 + i));
    }
    f.add(Launch {
        has_entry: false,
        ..Launch::at(2_000)
    });
    let h = f.finish();
    let r = banana_backtest::run(&h, &permissive()).unwrap();

    assert_eq!(r.funnel.stage("passed_filter").unwrap().remaining, 30);
    assert_eq!(r.funnel.stage("priced").unwrap().remaining, 29);
    assert!(
        r.results.measured().is_some(),
        "30 tokens were selected; hiding behind the 29 that could be priced would be the \
         flattering direction"
    );
    let m = r.results.measured().unwrap();
    assert_eq!(
        m.measured_over, 29,
        "but the denominator is what was measurable"
    );
}

// --- §5.5 mandatory funnel and regime warning ---------------------------------------------

#[test]
fn the_funnel_and_the_regime_warning_are_always_present() {
    let r = banana_backtest::run(&passing_n(30), &permissive()).unwrap();
    for id in [
        "all_launches",
        "matured",
        "passed_filter",
        "priced",
        "reached_target",
        "migrated",
    ] {
        assert!(r.funnel.stage(id).is_some(), "funnel is missing `{id}`");
    }
    assert!(r.regime_warning.contains("one market regime"));
}

#[test]
fn the_funnel_arithmetic_adds_up_to_the_universe() {
    // A reader must be able to take any stage, look up the one it narrows, and have the
    // subtraction work. `of` is what makes that possible when the funnel branches.
    let r = banana_backtest::run(&passing_n(35), &permissive()).unwrap();
    for s in &r.funnel.stages {
        let Some(of) = &s.of else {
            assert_eq!(s.id, "all_launches");
            continue;
        };
        let base = r
            .funnel
            .stage(of)
            .unwrap_or_else(|| panic!("stage `{}` names a base `{of}` that is not there", s.id));
        assert_eq!(
            s.removed,
            base.remaining - s.remaining,
            "stage `{}` does not account for what it removed from `{of}`",
            s.id
        );
    }
}

/// The claim `migrated` makes must be about the priced set, not about the target set.
#[test]
fn migrations_are_counted_against_the_priced_set_not_against_the_target() {
    let mut f = Fixture::new(0, TO_BLOCK);
    // Thirty tokens that migrated but never doubled inside five minutes. Chained, the
    // funnel would have had to claim these were a subset of the four that hit the target.
    for i in 0..30 {
        f.add(Launch {
            migrated: true,
            mult_5m_bps: Some(9_000),
            ..Launch::at(1_000 + i)
        });
    }
    for i in 0..4 {
        f.add(Launch::at(50_000 + i).winner());
    }
    let h = f.finish();
    let r = banana_backtest::run(&h, &permissive()).unwrap();

    let target = r.funnel.stage("reached_target").unwrap();
    let migrated = r.funnel.stage("migrated").unwrap();
    assert_eq!(target.remaining, 4);
    assert_eq!(migrated.remaining, 30);
    assert_eq!(migrated.of.as_deref(), Some("priced"));
    assert!(
        migrated.remaining > target.remaining,
        "a token can migrate without doubling in five minutes, and the funnel must survive it"
    );
}

// --- PLAN.md C2: the deployer-depth floor -------------------------------------------------

#[test]
fn the_depth_floor_applies_only_when_the_strategy_reads_a_deployer_feature() {
    let mut f = Fixture::new(0, TO_BLOCK);
    for i in 0..30 {
        // Shallow: these launches saw almost no deployer history.
        f.add(Launch {
            depth_blocks: 1_000,
            ..Launch::at(1_000 + i)
        });
    }
    let h = f.finish();

    // A filter that never looks at the deployer must not pay the C2 cost.
    let plain = banana_backtest::run(
        &h,
        &with_filter(EntryFilter::all_of([Condition::PairIn {
            pairs: vec![Pair::Eth],
        }])),
    )
    .unwrap();
    assert!(!plain.deployer_depth_applied);
    assert!(plain.funnel.stage("deployer_depth").is_none());
    assert_eq!(plain.funnel.stage("passed_filter").unwrap().remaining, 30);

    // One that does look must, and must say so as its own stage.
    let deployer = banana_backtest::run(
        &h,
        &with_filter(EntryFilter::all_of([Condition::MaxDeployerLaunches {
            max: 3,
        }])),
    )
    .unwrap();
    assert!(deployer.deployer_depth_applied);
    let stage = deployer
        .funnel
        .stage("deployer_depth")
        .expect("C2 requires its own funnel stage");
    assert_eq!(stage.remaining, 0, "every launch here is too shallow");
    assert_eq!(stage.removed, 30);
    assert!(
        stage.label.contains(&DEPLOYER_DEPTH_HOURS.to_string()),
        "the stage must name the floor it applied: {}",
        stage.label
    );
}

#[test]
fn deep_enough_launches_survive_the_depth_floor() {
    let mut f = Fixture::new(0, 10_000_000);
    for i in 0..30 {
        f.add(Launch {
            depth_blocks: 900_000, // well past 12 h
            ..Launch::at(1_000 + i)
        });
    }
    let h = f.finish();
    let r = banana_backtest::run(
        &h,
        &with_filter(EntryFilter::all_of([Condition::MaxDeployerLaunches {
            max: 3,
        }])),
    )
    .unwrap();
    assert_eq!(r.funnel.stage("deployer_depth").unwrap().removed, 0);
    assert_eq!(r.funnel.stage("passed_filter").unwrap().remaining, 30);
}

// --- §5.4 framing -------------------------------------------------------------------------

#[test]
fn the_result_carries_the_fixed_hold_figures_even_when_the_target_is_a_peak() {
    let cfg = StrategyConfig {
        success_target: SuccessTarget::MaxMultiple {
            multiple_bps: 20_000,
        },
        ..permissive()
    };
    let r = banana_backtest::run(&passing_n(30), &cfg).unwrap();
    let m = r.results.measured().unwrap();

    assert!(m.target_is_peak_based);
    assert!(m.target.contains("nobody captured"));
    // §5.4: both numbers cost the same to compute, so there is no reason to show only the
    // flattering one.
    assert!(m.hold_5m.assumption.contains("5 minutes"));
    assert!(m.hold_30m.assumption.contains("30 minutes"));
    assert!(m.peak.label.contains("nobody captured"));
}

#[test]
fn a_serialised_backtest_never_uses_the_language_of_profit() {
    let r = banana_backtest::run(&passing_n(30), &permissive()).unwrap();
    let json = serde_json::to_string(&r).unwrap().to_lowercase();
    for word in ["profit", "return", "earned", "gain", "would have made"] {
        assert!(!json.contains(word), "§5.4 forbids `{word}`");
    }
}

/// The distribution measured on the real window: the median token's peak is 1.00x.
#[test]
fn a_filter_that_selects_nothing_special_reports_a_median_at_or_below_one() {
    let r = banana_backtest::run(&passing_n(40), &permissive()).unwrap();
    let m = r.results.measured().unwrap();
    assert!(
        m.hold_5m.multiple.p50 <= 10_000,
        "the fixture is the ordinary case: a round trip, not a win"
    );
    assert_eq!(m.peak.never_above_entry, 40);
}

// --- the store must be there --------------------------------------------------------------

#[test]
fn an_empty_store_says_so_rather_than_reporting_a_zero_hit_rate() {
    let h = banana_store::History::in_memory().unwrap();
    let e = banana_backtest::run(&h, &permissive());
    assert!(matches!(e, Err(banana_backtest::BacktestError::EmptyStore)));
}

#[test]
fn a_holding_period_the_store_does_not_precompute_is_refused() {
    let cfg = StrategyConfig {
        success_target: SuccessTarget::FixedHoldMultiple {
            minutes: 7,
            multiple_bps: 20_000,
        },
        ..permissive()
    };
    let e = banana_backtest::run(&passing_n(30), &cfg);
    assert!(matches!(
        e,
        Err(banana_backtest::BacktestError::UnsupportedHold { minutes: 7 })
    ));
}

// --- helpers ------------------------------------------------------------------------------

/// A store in which exactly `n` matured launches pass a permissive filter.
fn passing_n(n: u64) -> banana_store::History {
    let mut f = Fixture::new(0, TO_BLOCK);
    for i in 0..n {
        f.add(Launch::at(1_000 + i));
    }
    assert!(
        TO_BLOCK - 1_000 - n > maturity_blocks(),
        "the fixture must be inside the window"
    );
    f.finish()
}
