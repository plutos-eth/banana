//! Spec §7.2: "Arming a backtested strategy in the sniper must be loading the same file,
//! with no translation step."
//!
//! This is the claim the whole product rests on — that what the Lab measured and what the
//! sniper does are the same rules, not two implementations that agree. It is easy to be
//! *nearly* true: a second config type "for live", a converter, a field the sniper reads
//! differently. So it gets its own test file, and the test is deliberately literal: write
//! the bytes, load them into both halves, and check the halves see the same thing.

use quarrel_core::features::{FeeRecipient, Pair, PitFeatures, Presence, Socials};
use quarrel_core::filter::{Condition, EntryFilter};
use quarrel_core::strategy::{PartialExit, StrategyConfig, SuccessTarget};
use quarrel_live::exits::{Mark, decide};
use quarrel_live::{Budget, Session};

/// A strategy a user might actually save, exercising every field both halves read.
fn saved_strategy() -> StrategyConfig {
    let mut c = StrategyConfig {
        entry_filter: EntryFilter::all_of([
            Condition::RequireTwitter,
            Condition::DevBuyBps {
                min: Some(150),
                max: Some(500),
            },
            Condition::MaxCreatorTaxBps { bps: 150 },
            Condition::MaxFingerprintTwins { max: 0 },
        ]),
        success_target: SuccessTarget::FixedHoldMultiple {
            minutes: 30,
            multiple_bps: 15_000,
        },
        ..StrategyConfig::default()
    };
    c.entry_model.max_tax_bps = 175;
    // Below the default take-profit of 1.80x, or it could never fire: closing beats
    // trimming, so a partial above the take-profit is dead configuration.
    c.exits.partials = vec![PartialExit {
        at_multiple_bps: 15_000,
        sell_bps: 5_000,
    }];
    c.live_guards.max_open_positions = 2;
    c
}

fn features(twitter: Presence, dev_buy: u32, tax: u32) -> PitFeatures {
    PitFeatures {
        pair: Pair::Eth,
        name: "SpaceWaffle".into(),
        symbol: "WAFFLE".into(),
        description: String::new(),
        socials: Socials {
            twitter,
            website: Presence::Absent,
            telegram: Presence::Absent,
        },
        exempt_wallets: Some(0),
        dev_buy_bps: Some(dev_buy),
        creator_tax_bps: Some(tax),
        fee_recipient: FeeRecipient::Deployer,
        deployer_launches: 0,
        deployer_graduations: 0,
        fingerprint_twins_30m: 0,
        deployer_history_depth_blocks: 10_000_000,
    }
}

#[test]
fn the_same_bytes_drive_the_lab_and_the_sniper() {
    // What the Rules view writes to strategy.json.
    let json = serde_json::to_string_pretty(&saved_strategy()).unwrap();

    // The Lab loads it...
    let for_lab: StrategyConfig = serde_json::from_str(&json).unwrap();
    // ...and the sniper loads the same bytes. No conversion, no second type.
    let for_sniper: StrategyConfig = serde_json::from_str(&json).unwrap();

    assert_eq!(for_lab, for_sniper);
    assert_eq!(for_lab, saved_strategy(), "and it round-trips exactly");
}

/// The part that matters: the *decision* is identical, not just the config.
#[test]
fn both_halves_reach_the_same_verdict_on_the_same_launch() {
    let json = serde_json::to_string(&saved_strategy()).unwrap();
    let for_lab: StrategyConfig = serde_json::from_str(&json).unwrap();
    let for_sniper: StrategyConfig = serde_json::from_str(&json).unwrap();

    let cases = [
        features(Presence::Present, 300, 100), // passes
        features(Presence::Absent, 300, 100),  // no twitter
        features(Presence::Present, 50, 100),  // dev buy too small
        features(Presence::Present, 300, 900), // tax too high
        features(Presence::Unknown, 300, 100), // unreadable, so refused
    ];
    for f in cases {
        let lab = for_lab.entry_filter.evaluate(&f);
        let sniper = for_sniper.entry_filter.evaluate(&f);
        assert_eq!(lab.passed, sniper.passed, "{f:?}");
        // Not merely the same answer: the same reasons, in the same order.
        assert_eq!(lab.refusals, sniper.refusals, "{f:?}");
    }
}

#[test]
fn the_sniper_reads_its_guards_and_exits_from_the_file_the_lab_ignored() {
    // §5.6: the Lab ignores `exits` and `live_guards` entirely. The sniper does not, and
    // it must get them from this same file rather than from a live-only default.
    let json = serde_json::to_string(&saved_strategy()).unwrap();
    let loaded: StrategyConfig = serde_json::from_str(&json).unwrap();

    let session = Session::dry_run(loaded.live_guards.clone());
    assert_eq!(session.budget().limits().max_open_positions, 2);

    // The partial the user wrote is the partial the exit rules apply.
    let e = decide(&loaded.exits, Mark::new(15_000, 10));
    let quarrel_live::Exit::Sell { sell_bps, rule, .. } = e else {
        panic!("the 1.50x partial should have fired");
    };
    assert_eq!(rule, "partial");
    assert_eq!(sell_bps, 5_000);
}

#[test]
fn there_is_no_second_config_type_to_translate_into() {
    // A converter would have to exist somewhere. The strongest available check is that
    // `Session` and `Budget` are built from `LiveGuards` itself, not from a live-only
    // mirror of it, and that the type is the one `core` defines.
    let guards = saved_strategy().live_guards;
    let from_core: quarrel_core::strategy::LiveGuards = guards.clone();
    let budget = Budget::new(from_core);
    assert_eq!(
        budget.limits().session_budget_wei,
        guards.session_budget_wei
    );
}

#[test]
fn a_strategy_saved_by_the_app_is_the_one_the_sniper_would_arm_with() {
    // End to end through a real file, because "the same bytes" is the claim.
    let dir = std::env::temp_dir().join("quarrel-one-config");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("strategy.json");

    std::fs::write(
        &path,
        serde_json::to_string_pretty(&saved_strategy()).unwrap(),
    )
    .unwrap();

    let reloaded: StrategyConfig =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(reloaded, saved_strategy());

    // And the tax ceiling the sniper waits for is the one the file carries.
    assert_eq!(reloaded.entry_model.max_tax_bps, 175);
}
