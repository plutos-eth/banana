//! The point-in-time regression: a filter feature must not read a block at or after the
//! launch it describes (spec §5.3).
//!
//! `quarrel-core` already makes the *boundary* structural — `evaluate` takes `PitFeatures`
//! and `PostEntryFacts` is a different type, proven by a compile-fail test. That stops a
//! rule reaching for the future. It cannot stop a value in `PitFeatures` from having been
//! computed out of the future in the first place, which is the leak this file is about.
//!
//! Each test builds the same fixture twice: once with features from the real
//! `FeatureBuilder`, and once with the numbers a leaky implementation would have produced.
//! Both go through the whole backtest. The assertion is not merely that the correct one is
//! correct — it is that the two **disagree**, which is what makes the test capable of
//! failing if the leak is ever reintroduced.

mod support;

use std::collections::HashMap;

use quarrel_core::filter::{Condition, EntryFilter};
use quarrel_core::strategy::{StrategyConfig, SuccessTarget};
use quarrel_indexer::features::{FeatureBuilder, Fingerprint, LaunchFacts};
use quarrel_store::History;
use support::{Fixture, Launch, addr_n};

const TO_BLOCK: u64 = 2_000_000;
const FIRST: u64 = 1_000;
const SECOND: u64 = 300_000;

fn strategy(filter: EntryFilter) -> StrategyConfig {
    StrategyConfig {
        entry_filter: filter,
        success_target: SuccessTarget::FixedHoldMultiple {
            minutes: 5,
            multiple_bps: 20_000,
        },
        ..StrategyConfig::default()
    }
}

/// How many launches passed. The funnel carries this whatever the sample gate decides.
fn passed(h: &History, filter: EntryFilter) -> u64 {
    quarrel_backtest::run(h, &strategy(filter))
        .unwrap()
        .funnel
        .stage("passed_filter")
        .unwrap()
        .remaining
}

/// Run the real point-in-time feature builder over two launches by one deployer.
///
/// Returns `(launches_visible_to_first, launches_visible_to_second)`.
fn real_deployer_launches(graduated: HashMap<alloy_primitives::Address, u64>) -> [(u32, u32); 2] {
    let deployer = addr_n(1);
    let mut b = FeatureBuilder::new(0, graduated);
    let mut out = Vec::new();
    for (i, block) in [FIRST, SECOND].into_iter().enumerate() {
        let r = b.push(&LaunchFacts {
            token: addr_n(500 + i as u64),
            deployer,
            block,
            fingerprint: Fingerprint::new(
                None,
                Some(100),
                quarrel_core::features::Socials::NONE,
                Some(0),
            ),
        });
        out.push((r.deployer_launches, r.deployer_graduations));
    }
    [out[0], out[1]]
}

/// Two launches by one deployer, with whatever deployer history the caller says.
fn store(first: (u32, u32), second: (u32, u32)) -> History {
    let mut f = Fixture::new(0, TO_BLOCK);
    f.add(Launch {
        deployer: 1,
        deployer_launches: first.0,
        deployer_graduations: first.1,
        depth_blocks: 10_000_000,
        ..Launch::at(FIRST)
    });
    f.add(Launch {
        deployer: 1,
        deployer_launches: second.0,
        deployer_graduations: second.1,
        depth_blocks: 10_000_000,
        ..Launch::at(SECOND)
    });
    f.finish()
}

#[test]
fn a_deployers_later_launch_is_invisible_to_its_earlier_one() {
    let real = real_deployer_launches(HashMap::new());
    assert_eq!(
        real,
        [(0, 0), (1, 0)],
        "the first launch must see no history; the second must see the first"
    );

    // What a leak looks like: every launch sees the deployer's whole career, so the first
    // one is credited with a launch that had not happened yet.
    let leaked = [(1u32, 0u32), (1, 0)];

    let only_first_timers = || EntryFilter::all_of([Condition::MaxDeployerLaunches { max: 0 }]);

    let correct = passed(&store(real[0], real[1]), only_first_timers());
    let wrong = passed(&store(leaked[0], leaked[1]), only_first_timers());

    assert_eq!(correct, 1, "exactly the first launch was a first launch");
    assert_eq!(wrong, 0);
    assert_ne!(
        correct, wrong,
        "the two must disagree, or this test could not detect the leak it exists for"
    );
}

#[test]
fn a_graduation_after_a_launch_does_not_count_toward_that_launchs_deployer_rate() {
    // The deployer's first token graduates at block 400,000 -- after the second launch.
    // At the moment of the second launch its deployer had graduated nothing.
    let mut graduated = HashMap::new();
    graduated.insert(addr_n(500), 400_000u64);
    let real = real_deployer_launches(graduated);
    assert_eq!(
        real,
        [(0, 0), (1, 0)],
        "the graduation is in the future relative to the second launch"
    );

    // The leak: counting the graduation because it happened at all.
    let leaked = [(0u32, 0u32), (1, 1)];

    let proven = || {
        EntryFilter::all_of([Condition::MinDeployerGradRateBps {
            bps: 5_000,
            allow_unproven: false,
        }])
    };

    let correct = passed(&store(real[0], real[1]), proven());
    let wrong = passed(&store(leaked[0], leaked[1]), proven());

    assert_eq!(
        correct, 0,
        "nothing had graduated yet, so nothing is proven"
    );
    assert_eq!(
        wrong, 1,
        "the leak would have shown a deployer with a perfect record"
    );
    assert_ne!(correct, wrong);
}

/// A graduation that really did precede the launch must still count.
///
/// Without this, "never read the future" could be satisfied by reading nothing at all.
#[test]
fn a_graduation_before_a_launch_does_count() {
    let mut graduated = HashMap::new();
    graduated.insert(addr_n(500), 200_000u64); // before the second launch at 300,000
    let real = real_deployer_launches(graduated);
    assert_eq!(real[1], (1, 1), "history that had happened must be visible");

    let h = store(real[0], real[1]);
    let n = passed(
        &h,
        EntryFilter::all_of([Condition::MinDeployerGradRateBps {
            bps: 5_000,
            allow_unproven: false,
        }]),
    );
    assert_eq!(n, 1, "the second launch had a proven deployer");
}

/// The other half of §5.3, restated where the backtest can see it.
///
/// `PostEntryFacts` cannot reach a filter because it is a different type and `evaluate`
/// does not take one; `crates/core/tests/ui` proves that with a compile-fail test. What
/// this asserts is the consequence: nothing the backtest reads out of `outcomes` is ever
/// handed to the evaluator, so a post-entry column cannot influence a decision.
#[test]
fn changing_a_post_entry_column_cannot_change_who_passes() {
    let real = real_deployer_launches(HashMap::new());

    let mut f = Fixture::new(0, TO_BLOCK);
    f.add(Launch {
        deployer_launches: real[0].0,
        ..Launch::at(FIRST)
    });
    f.add(Launch {
        deployer_launches: real[1].0,
        // A wildly different fate, decided entirely after entry.
        ..Launch::at(SECOND).winner()
    });
    let h = f.finish();

    let filter = EntryFilter::all_of([
        Condition::RequireTwitter,
        Condition::MaxCreatorTaxBps { bps: 200 },
    ]);
    assert_eq!(
        passed(&h, filter),
        2,
        "both launches pass on their point-in-time features, whatever became of them"
    );
}
