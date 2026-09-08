//! The denominator: a hit rate over the whole matured universe, never over the survivors.
//!
//! Spec §5.5 puts this first among the guards, and it is the failure mode with the most
//! flattering shape — every way of accidentally dropping a dead token raises the number.
//! The measured window says how much room there is to get this wrong: **56.6% of launches
//! never trade above their entry price at all** and the median token is finished in about
//! four minutes (`docs/FINDINGS.md` §9). A backtest that quietly excluded those would show
//! a hit rate several times the truth.
//!
//! So each test here builds a universe with a known, deliberately awful answer, and
//! asserts the reported rate is the awful one.

mod support;

use banana_core::filter::EntryFilter;
use banana_core::strategy::{StrategyConfig, SuccessTarget};
use support::{Fixture, Launch};

const TO_BLOCK: u64 = 1_000_000;

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

/// `winners` tokens that doubled, `losers` that went nowhere. All matured, all passing.
fn universe(winners: u64, losers: u64) -> banana_store::History {
    let mut f = Fixture::new(0, TO_BLOCK);
    for i in 0..winners {
        f.add(Launch::at(1_000 + i).winner());
    }
    for i in 0..losers {
        f.add(Launch {
            // The ordinary outcome: entered, went straight down, stopped trading.
            mult_5m_bps: Some(5_000),
            mult_30m_bps: Some(2_000),
            max_multiple_bps: Some(10_000),
            ..Launch::at(100_000 + i)
        });
    }
    f.finish()
}

#[test]
fn the_hit_rate_is_over_everything_that_matured_not_over_what_survived() {
    let h = universe(10, 90);
    let r = banana_backtest::run(&h, &permissive()).unwrap();
    let m = r.results.measured().expect("100 tokens is a sample");

    assert_eq!(m.hits, 10);
    assert_eq!(m.measured_over, 100, "the losers are in the denominator");
    assert_eq!(
        m.hit_rate_bps, 1_000,
        "10%. Over survivors alone it would read 10000 bps, and that is the number this \
         test exists to prevent"
    );
}

#[test]
fn a_token_with_no_post_entry_trades_is_still_counted() {
    // The most easily lost row: nobody ever bought it after entry. It is not missing data,
    // it is the answer.
    let mut f = Fixture::new(0, TO_BLOCK);
    for i in 0..10 {
        f.add(Launch::at(1_000 + i).winner());
    }
    for i in 0..30 {
        f.add(Launch {
            mult_5m_bps: Some(10_000), // never moved off entry
            mult_30m_bps: Some(10_000),
            max_multiple_bps: Some(10_000),
            ..Launch::at(100_000 + i)
        });
    }
    let h = f.finish();
    let r = banana_backtest::run(&h, &permissive()).unwrap();
    let m = r.results.measured().unwrap();

    assert_eq!(m.measured_over, 40);
    assert_eq!(m.hit_rate_bps, 2_500);
    assert_eq!(
        m.peak.never_above_entry, 30,
        "a peak of exactly 1.00x is the majority case on the real chain"
    );
}

/// The funnel is what makes the denominator visible rather than merely correct.
#[test]
fn every_launch_is_accounted_for_between_the_universe_and_the_result() {
    let h = universe(10, 90);
    let r = banana_backtest::run(&h, &permissive()).unwrap();

    let universe_size = r.funnel.stage("all_launches").unwrap().remaining;
    let measured = r.results.measured().unwrap().measured_over;
    // Only the main chain, up to the denominator. `reached_target` and `migrated` branch
    // off `priced` and are not part of the subtraction that lands on it.
    let removed: u64 = r
        .funnel
        .stages
        .iter()
        .take_while(|s| s.id != "reached_target")
        .map(|s| s.removed)
        .sum();

    assert_eq!(
        universe_size - removed,
        measured,
        "a reader must be able to subtract the funnel and land on the denominator"
    );
}

/// F9's refusal is a visible stage, not a silent drop.
#[test]
fn a_launch_that_could_not_be_priced_leaves_the_funnel_where_a_reader_can_see_it() {
    let mut f = Fixture::new(0, TO_BLOCK);
    for i in 0..30 {
        f.add(Launch::at(1_000 + i).winner());
    }
    for i in 0..5 {
        // The curve could not be replayed exactly, so no entry price was invented.
        f.add(Launch {
            has_entry: false,
            ..Launch::at(200_000 + i)
        });
    }
    let h = f.finish();
    let r = banana_backtest::run(&h, &permissive()).unwrap();

    assert_eq!(r.funnel.stage("passed_filter").unwrap().remaining, 35);
    let priced = r.funnel.stage("priced").unwrap();
    assert_eq!(priced.remaining, 30);
    assert_eq!(
        priced.removed, 5,
        "the five must be visible as a stage, not vanish between two others"
    );
    assert_eq!(r.results.measured().unwrap().measured_over, 30);
}

/// Migration is reported as a count beside its rate, because the base rate is tiny.
#[test]
fn migrations_are_reported_as_a_raw_count() {
    let mut f = Fixture::new(0, TO_BLOCK);
    for i in 0..89 {
        f.add(Launch::at(1_000 + i));
    }
    f.add(Launch {
        migrated: true,
        ..Launch::at(90_000).winner()
    });
    let h = f.finish();

    let cfg = StrategyConfig {
        success_target: SuccessTarget::ReachedMigration,
        ..permissive()
    };
    let r = banana_backtest::run(&h, &cfg).unwrap();
    let m = r.results.measured().unwrap();

    assert_eq!(m.migrations, 1);
    assert_eq!(m.hits, 1);
    assert_eq!(m.measured_over, 90);
    // 1 in 90 rounds to 111 bps. The count is what a reader should trust at this scale.
    assert_eq!(m.hit_rate_bps, 111);
}
