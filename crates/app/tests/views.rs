//! Every view, against a real store.
//!
//! The phase-5 criterion is "every view works against real indexed data", and the way to
//! test that without a window is to call the same functions the commands call. A store is
//! built here from the same `quarrel-store` API the indexer writes through, so a schema
//! change breaks these rather than letting them drift.
//!
//! When `data/lab/history.db` exists the same checks also run against the indexed window,
//! which is what catches the things a hand-built fixture never contains — a launch with no
//! enrichment row, a curve that would not replay, a name with an emoji in it.

use std::path::{Path, PathBuf};

use alloy_primitives::{Address, B256, U256};
use quarrel_app::AppState;
use quarrel_app::api;
use quarrel_core::features::{Presence, Socials};
use quarrel_core::filter::{Condition, EntryFilter};
use quarrel_core::strategy::StrategyConfig;
use quarrel_store::History;
use quarrel_store::history::{EnrichmentRow, LaunchRow, OutcomeRow, PhaseState, PitFeaturesRow};
use quarrel_store::types::EntryRule;

fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("quarrel-views-{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A store with `n` launches, half of which declare a Twitter link.
fn build_store(dir: &Path, n: u64) {
    let mut h = History::open(dir.join("history.db")).unwrap();
    for i in 0..n {
        let token = addr(i + 1_000);
        h.insert_launches(&[LaunchRow {
            token,
            curve: addr(i + 5_000),
            deployer: addr(i % 7),
            pair_token: Address::ZERO,
            launch_config_id: 0,
            graduation_threshold: U256::from(4_200_000_000_000_000_000u64),
            block: 1_000 + i,
            tx_hash: B256::from(U256::from(i + 1)),
            log_index: 0,
        }])
        .unwrap();
        h.insert_enrichment(&[EnrichmentRow {
            token,
            decoded: true,
            selector: Some("0xf85f8e41".into()),
            name: Some(format!("Token {i}")),
            symbol: Some(format!("TK{i}")),
            description: Some(String::new()),
            logo: None,
            twitter_url: (i % 2 == 0).then(|| "https://x.com/example".to_string()),
            website_url: None,
            telegram_url: None,
            socials: Socials {
                twitter: if i % 2 == 0 {
                    Presence::Present
                } else {
                    Presence::Absent
                },
                website: Presence::Absent,
                telegram: Presence::Absent,
            },
            exempt_wallets: Some(0),
            creator_fee_recipient: Some(addr(i % 7)),
            creator_tax_bps: Some(100),
            declared_quote_in: None,
            dev_buy_quote: None,
            dev_buy_tokens: None,
            dev_buy_bps: Some(300),
        }])
        .unwrap();
        h.upsert_pit_features(&PitFeaturesRow {
            token,
            deployer_launches: 0,
            deployer_graduations: 0,
            deployer_grad_rate_bps: None,
            fingerprint: format!("300|100|1 00|0-{i}"),
            fingerprint_twins_30m: 0,
            deployer_history_depth_blocks: 10_000_000,
        })
        .unwrap();
        h.upsert_outcome(&OutcomeRow {
            token,
            entry_rule: EntryRule::ObservedUntaxedBuy,
            entry_block: Some(1_000 + i),
            entry_price: Some(U256::from(1_000_000_000u64)),
            entry_tokens: Some(U256::from(1_000u64)),
            ath_price: None,
            ath_block: None,
            max_multiple_bps: Some(10_000),
            time_to_ath_s: None,
            mult_after_5m_bps: Some(9_600),
            mult_after_30m_bps: Some(9_000),
            migrated: false,
            died: false,
            distinct_buyers_1m: None,
            every_early_buy_taxed: None,
            post_entry_trades: 4,
            last_trade_block: Some(2_000 + i),
            observed_blocks: 1_000,
        })
        .unwrap();
    }
    h.checkpoint(
        "launches",
        PhaseState {
            from_block: 0,
            last_block: 1_000_000,
            target_block: 1_000_000,
            rows_written: n,
        },
    )
    .unwrap();
}

fn addr(n: u64) -> Address {
    let mut b = [0u8; 20];
    b[12..].copy_from_slice(&n.to_be_bytes());
    Address::from(b)
}

/// Only twitter, so exactly half the fixture passes.
fn twitter_only() -> StrategyConfig {
    StrategyConfig {
        entry_filter: EntryFilter::all_of([Condition::RequireTwitter]),
        ..StrategyConfig::default()
    }
}

// --- view 6: status -----------------------------------------------------------------------

#[test]
fn status_reads_a_real_store() {
    let d = temp_dir("status");
    build_store(&d, 40);
    let s = api::status(&AppState::new(&d, false));

    assert!(s.store.exists);
    assert_eq!(s.store.launches, 40);
    assert!(s.store.bytes > 0);
    assert_eq!(s.mode_label, "DRY RUN", "the indicator is on every view");
    assert_eq!(s.chain_id, 4663);
    assert!(!s.indexing);
}

// --- view 1: feed -------------------------------------------------------------------------

#[test]
fn the_feed_shows_every_launch_with_its_decision_and_its_reasons() {
    let d = temp_dir("feed");
    build_store(&d, 40);
    let state = AppState::new(&d, false);
    state.save_strategy(&twitter_only()).unwrap();

    let page = api::feed(&state, &api::FeedQuery::default()).unwrap();
    assert_eq!(page.rows.len(), 40);
    assert_eq!(page.rows.iter().filter(|r| r.passed).count(), 20);

    // Spec §3.4: a refusal names the rule and the values.
    let refused = page.rows.iter().find(|r| !r.passed).unwrap();
    assert_eq!(refused.refusals.len(), 1);
    assert_eq!(refused.refusals[0].rule, "require_twitter");
    assert!(refused.refusals[0].detail.contains("twitter"));

    // Nearest to passing first (spec §7.1). Ordering only -- the boolean already decided.
    assert!(page.rows[0].passed);
    assert!(page.rows.windows(2).all(|w| w[0].rank_bps >= w[1].rank_bps));
}

#[test]
fn the_passing_chip_and_the_search_box_narrow_the_same_list() {
    let d = temp_dir("feedfilter");
    build_store(&d, 40);
    let state = AppState::new(&d, false);
    state.save_strategy(&twitter_only()).unwrap();

    let passing = api::feed(
        &state,
        &api::FeedQuery {
            passing_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(passing.matched, 20);
    assert!(passing.rows.iter().all(|r| r.passed));

    let searched = api::feed(
        &state,
        &api::FeedQuery {
            search: "TK7".into(),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(searched.matched, 1);
    assert_eq!(searched.rows[0].symbol, "TK7");
}

#[test]
fn the_feed_says_when_it_is_showing_less_than_the_store_holds() {
    let d = temp_dir("feedcap");
    build_store(&d, 40);
    let state = AppState::new(&d, false);

    let page = api::feed(
        &state,
        &api::FeedQuery {
            limit: Some(10),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(page.scanned, 10);
    assert!(
        page.truncated,
        "a truncated view must say so, or it reads as the whole chain"
    );
}

#[test]
fn the_detail_drawer_shows_every_rule_and_its_verdict() {
    let d = temp_dir("detail");
    build_store(&d, 10);
    let state = AppState::new(&d, false);
    state.save_strategy(&twitter_only()).unwrap();

    // Token 1 has no twitter (odd index), so it is refused and says why.
    let token = format!("{:#x}", addr(1_001));
    let detail = api::launch_detail(&state, &token).unwrap();

    assert!(!detail.row.passed);
    assert_eq!(detail.rules.len(), 1);
    assert!(!detail.rules[0].passed);
    assert!(detail.rules[0].detail.is_some());
    // Every read the decision rested on, so the user can check it by hand.
    assert_eq!(detail.features.creator_tax_bps, Some(100));
    assert_eq!(detail.features.dev_buy_bps, Some(300));
    // Explorer links are text; nothing here is fetched (PLAN.md C1).
    assert!(
        detail
            .links
            .token
            .starts_with("https://robinhoodchain.blockscout.com/")
    );
    assert!(detail.links.tx.contains("/tx/"));
    // Post-entry facts are present for display and are a different type from the
    // features the evaluator saw (spec §5.3).
    assert!(detail.outcome.unwrap().has_entry);
}

#[test]
fn asking_for_a_launch_that_is_not_there_says_so() {
    let d = temp_dir("missing");
    build_store(&d, 3);
    let state = AppState::new(&d, false);
    let e = api::launch_detail(&state, &format!("{:#x}", addr(999_999)));
    assert!(e.is_err());
    // And a value that is not an address at all is refused before it reaches the store.
    assert!(api::launch_detail(&state, "not-an-address").is_err());
}

// --- view 3: the Strategy Lab -------------------------------------------------------------

#[test]
fn the_lab_runs_over_the_same_store_and_keeps_its_guards() {
    let d = temp_dir("lab");
    build_store(&d, 40);
    let state = AppState::new(&d, false);

    let r = api::backtest(&state, &twitter_only()).unwrap();
    assert_eq!(r.funnel.stage("all_launches").unwrap().remaining, 40);
    assert_eq!(r.funnel.stage("passed_filter").unwrap().remaining, 20);
    assert!(r.regime_warning.contains("one market regime"));

    // The live count the rule editor calls on every keystroke.
    let c = api::pass_count(&state, &twitter_only()).unwrap();
    assert_eq!(c.passed, 20);
    assert_eq!(c.universe, 40);
}

#[test]
fn the_sample_gate_survives_the_trip_through_the_ipc_layer() {
    // The guard is in the crate, not the UI (spec §5.5), so it must still be there after
    // the value has been serialised for the window.
    let d = temp_dir("gate");
    build_store(&d, 29);
    let state = AppState::new(&d, false);

    let r = api::backtest(
        &state,
        &StrategyConfig {
            entry_filter: EntryFilter::All(vec![]),
            ..StrategyConfig::default()
        },
    )
    .unwrap();
    let json = serde_json::to_string(&r).unwrap();
    assert!(json.contains("sample too small"), "{json}");
    for forbidden in ["hit_rate", "p50"] {
        assert!(!json.contains(forbidden), "leaked `{forbidden}`");
    }
}

// --- view 2: positions --------------------------------------------------------------------

#[test]
fn positions_is_empty_and_explains_itself() {
    let d = temp_dir("positions");
    build_store(&d, 3);
    let p = api::positions(&AppState::new(&d, false));
    assert!(p.open.is_empty() && p.closed.is_empty());
    assert!(!p.note.is_empty(), "an empty view must say why it is empty");
}

// --- view 4: index ------------------------------------------------------------------------

#[test]
fn the_index_view_reports_phase_state_so_a_resume_is_visible() {
    let d = temp_dir("index");
    build_store(&d, 12);
    let s = api::index_status(&AppState::new(&d, false)).unwrap();

    assert_eq!(s.store.launches, 12);
    let launches = s.phases.iter().find(|p| p.phase == "launches").unwrap();
    assert!(launches.complete);
    assert_eq!(launches.target_block, 1_000_000);
    assert!(!s.indexing);
}

// --- view 5: rules ------------------------------------------------------------------------

#[test]
fn saving_a_strategy_writes_the_file_the_sniper_will_arm_from() {
    let d = temp_dir("rules");
    build_store(&d, 3);
    let state = AppState::new(&d, false);

    let mut cfg = StrategyConfig::default();
    cfg.entry_model.max_tax_bps = 250;
    state.save_strategy(&cfg).unwrap();

    // Spec §7.2: one file, no translation step. What the Rules view writes is what the
    // Lab reads and what the sniper will load.
    let raw = std::fs::read_to_string(d.join("strategy.json")).unwrap();
    let back: StrategyConfig = serde_json::from_str(&raw).unwrap();
    assert_eq!(back, cfg);
    assert_eq!(AppState::new(&d, false).strategy(), cfg);
}

// --- against the real indexed window ------------------------------------------------------

/// The same views, over whatever has actually been indexed.
///
/// Skipped rather than failed when there is no store: a fresh clone has none, and a test
/// that cannot run is better than one that pretends a missing file is a pass.
#[test]
fn every_view_works_against_the_indexed_window() {
    let dir = Path::new("../../data/lab");
    if !dir.join("history.db").is_file() {
        println!(
            "no indexed store at {}; fixture coverage only",
            dir.display()
        );
        return;
    }
    if !quarrel_store::Lock::is_free(dir.join("history.db")) {
        println!("an index holds the writer lock; skipping");
        return;
    }
    let state = AppState::new(dir, false);

    let status = api::status(&state);
    assert!(status.store.launches > 0);
    assert!(status.store.to_block > status.store.from_block);

    let feed = api::feed(&state, &api::FeedQuery::default()).unwrap();
    assert!(!feed.rows.is_empty());
    // Real data contains launches whose calldata never decoded. They must be in the feed
    // with unknowns, not missing from it.
    let unknown = feed.rows.iter().filter(|r| r.dev_buy_bps.is_none()).count();
    println!(
        "feed: {} rows, {} passing, {unknown} with an unreadable dev buy",
        feed.rows.len(),
        feed.rows.iter().filter(|r| r.passed).count()
    );

    let detail = api::launch_detail(&state, &feed.rows[0].token).unwrap();
    assert_eq!(detail.row.token, feed.rows[0].token);

    let r = api::backtest(&state, &StrategyConfig::default()).unwrap();
    assert!(r.funnel.stage("all_launches").unwrap().remaining > 0);
    println!(
        "lab: {} matured, {} passed, query {} ms",
        r.funnel.stage("matured").unwrap().remaining,
        r.funnel.stage("passed_filter").unwrap().remaining,
        r.query_ms
    );

    let idx = api::index_status(&state).unwrap();
    assert!(!idx.phases.is_empty(), "a real index leaves phase state");
}
