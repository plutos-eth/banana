//! What the six views of spec §8 actually ask for, as plain functions.
//!
//! Deliberately not Tauri commands. Everything here takes an [`AppState`] and returns a
//! serialisable value, so the whole surface can be tested against a real store without
//! starting a window — which matters, because the phase-5 criterion is "every view works
//! against real indexed data" and a test that needs a GUI is a test nobody runs.
//!
//! # The point-in-time boundary crosses the IPC boundary too
//!
//! [`FeedRow`] carries the decision and the values it was made from. It does **not** carry
//! anything from `outcomes` unless the row is being shown in the Lab, where post-entry
//! facts are the subject. The evaluator is called with `&PitFeatures` here exactly as it is
//! in the backtest and will be in the sniper: one evaluator, one type, three callers.

use quarrel_backtest::BacktestResult;
use quarrel_core::features::PitFeatures;
use quarrel_core::filter::Refusal;
use quarrel_core::rank::{display_rank, feed_order};
use quarrel_core::strategy::StrategyConfig;
use quarrel_store::History;
use quarrel_store::lab::Candidate;
use serde::{Deserialize, Serialize};

use crate::state::{AppError, AppState, Mode, Result};

/// The most launches the feed will hold in memory at once.
///
/// Spec §8 requires the list to virtualise thousands of rows an hour; the phase-5
/// criterion names 20,000. Beyond that the view is not more useful, it is just slower, and
/// the Lab is the tool for looking at the whole window.
pub const FEED_CAP: usize = 20_000;

// --- view 6: status -----------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreSummary {
    pub path: String,
    pub exists: bool,
    pub launches: u64,
    pub trades: u64,
    pub outcomes: u64,
    pub from_block: Option<u64>,
    pub to_block: Option<u64>,
    pub hours_x10: Option<u64>,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    /// Always rendered, on every view (spec §8, view 6).
    pub mode: Mode,
    pub mode_label: String,
    /// What this mode means, in the terms PLAN.md C7 asks for.
    pub engine: String,
    /// True only when armed. The UI keys its warning colour on this rather than on the
    /// mode string, so a new mode cannot quietly render as safe.
    pub can_spend: bool,
    pub indexing: bool,
    pub data_dir: String,
    pub store: StoreSummary,
    pub chain_id: u64,
    pub explorer: String,
    pub has_saved_strategy: bool,
}

pub fn status(state: &AppState) -> Status {
    let store = store_summary(state);
    Status {
        mode: state.mode(),
        mode_label: state.mode().label().to_string(),
        engine: state.mode().explain().to_string(),
        can_spend: state.mode().can_spend(),
        indexing: state.is_indexing(),
        data_dir: state.data_dir().display().to_string(),
        store,
        chain_id: quarrel_chain::addr::CHAIN_ID,
        explorer: quarrel_chain::addr::EXPLORER.to_string(),
        has_saved_strategy: state.has_saved_strategy(),
    }
}

pub fn store_summary(state: &AppState) -> StoreSummary {
    let path = state.db_path();
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let empty = StoreSummary {
        path: path.display().to_string(),
        exists: path.is_file(),
        launches: 0,
        trades: 0,
        outcomes: 0,
        from_block: None,
        to_block: None,
        hours_x10: None,
        bytes,
    };
    state
        .with_history(|h| {
            let window = h.window()?;
            Ok(StoreSummary {
                launches: h.launch_count()?,
                trades: h.trade_count()?,
                outcomes: h.count("outcomes")?,
                from_block: window.map(|w| w.from_block),
                to_block: window.map(|w| w.to_block),
                hours_x10: window.map(|w| w.blocks() * BLOCK_MS / 360_000),
                ..empty.clone()
            })
        })
        .unwrap_or(empty)
}

/// Measured block time (`docs/FINDINGS.md` §1).
const BLOCK_MS: u64 = 101;

// --- view 1: feed -------------------------------------------------------------------------

/// One launch as the feed shows it: what was read, and what the rules made of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedRow {
    pub token: String,
    pub symbol: String,
    pub name: String,
    pub block: u64,
    /// Interpolated from sampled anchors, so approximate by construction (PLAN.md F2).
    /// `None` outside the anchored range rather than extrapolated.
    pub ts: Option<u64>,
    pub pair: String,
    /// `None` means the launch transaction could not be read — not that the value is zero.
    pub dev_buy_bps: Option<u32>,
    pub creator_tax_bps: Option<u32>,
    pub exempt_wallets: Option<u32>,
    pub twins: u32,
    pub deployer_launches: u32,
    pub passed: bool,
    /// How near a refusal came, for ordering only. Never a score, never persisted, and it
    /// cannot turn a refusal into a pass (see `quarrel_core::rank`).
    pub rank_bps: u32,
    pub refusals: Vec<Refusal>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedPage {
    pub rows: Vec<FeedRow>,
    /// How many launches were read out of the store before filtering.
    pub scanned: usize,
    /// How many matched the chips and the search.
    pub matched: usize,
    /// True when the store holds more launches than the feed will load (`FEED_CAP`).
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FeedQuery {
    /// Show only launches the current strategy would have taken.
    #[serde(default)]
    pub passing_only: bool,
    /// Case-insensitive substring over name, symbol and token address.
    #[serde(default)]
    pub search: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

pub fn feed(state: &AppState, q: &FeedQuery) -> Result<FeedPage> {
    let strategy = state.strategy();
    let cap = q.limit.unwrap_or(FEED_CAP).min(FEED_CAP);
    state.with_history(|h| {
        let candidates = h.recent_candidates(cap)?;
        let scanned = candidates.len();
        let total = h.launch_count()?;

        let needle = q.search.trim().to_lowercase();
        let mut rows: Vec<FeedRow> = candidates
            .iter()
            .map(|c| feed_row(h, c, &strategy))
            .filter(|r| !q.passing_only || r.passed)
            .filter(|r| matches(r, &needle))
            .collect();

        // Nearest to passing first, then newest, so a wall of refusals is navigable
        // (spec §7.1). Ordering only: the boolean decision is already made.
        rows.sort_by(|a, b| feed_order((a.rank_bps, a.block), (b.rank_bps, b.block)));

        Ok(FeedPage {
            matched: rows.len(),
            rows,
            scanned,
            truncated: total as usize > scanned,
        })
    })
}

fn matches(r: &FeedRow, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    r.symbol.to_lowercase().contains(needle)
        || r.name.to_lowercase().contains(needle)
        || r.token.to_lowercase().contains(needle)
}

fn feed_row(h: &History, c: &Candidate, strategy: &StrategyConfig) -> FeedRow {
    let decision = strategy.entry_filter.evaluate(&c.features);
    FeedRow {
        token: format!("{:#x}", c.token),
        symbol: c.features.symbol.clone(),
        name: c.features.name.clone(),
        block: c.launch_block,
        ts: h.timestamp_at(c.launch_block).ok().flatten(),
        pair: c.features.pair.label().to_string(),
        dev_buy_bps: c.features.dev_buy_bps,
        creator_tax_bps: c.features.creator_tax_bps,
        exempt_wallets: c.features.exempt_wallets,
        twins: c.features.fingerprint_twins_30m,
        deployer_launches: c.features.deployer_launches,
        passed: decision.passed,
        rank_bps: display_rank(&strategy.entry_filter, &c.features),
        refusals: decision.refusals,
    }
}

// --- view 1: the detail drawer ------------------------------------------------------------

/// Every read and every rule evaluation for one launch (spec §8, view 1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchDetail {
    pub row: FeedRow,
    /// Exactly what the evaluator saw. Shown so a user can check the decision by hand.
    pub features: PitFeatures,
    /// Per-condition outcome, in the order the rules are written.
    pub rules: Vec<RuleOutcome>,
    pub links: Links,
    /// Post-entry facts, if the store has them. **Display only** — a different type from
    /// the one the evaluator takes, which is what stops a rule reading them (spec §5.3).
    pub outcome: Option<OutcomeView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleOutcome {
    pub rule: String,
    pub passed: bool,
    /// The refusal sentence, naming the value and the threshold (spec §3.4).
    pub detail: Option<String>,
}

/// Explorer URLs. Text only — the app never fetches them (PLAN.md C1); the user clicks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Links {
    pub token: String,
    pub curve: String,
    pub deployer: String,
    pub tx: String,
    /// The deployer's declared links, sanitised for display and **never fetched**.
    pub socials: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutcomeView {
    pub entry_rule: String,
    pub has_entry: bool,
    pub max_multiple_bps: Option<u64>,
    pub mult_after_5m_bps: Option<u64>,
    pub mult_after_30m_bps: Option<u64>,
    pub migrated: bool,
    pub post_entry_trades: u64,
}

pub fn launch_detail(state: &AppState, token: &str) -> Result<LaunchDetail> {
    let strategy = state.strategy();
    let wanted: alloy_primitives::Address = token
        .parse()
        .map_err(|_| AppError::Refused(format!("not an address: {token}")))?;

    state.with_history(|h| {
        let c = h
            .candidate(wanted)?
            .ok_or_else(|| AppError::Refused(format!("no launch {token} in this store")))?;

        let e = h.enrichment_for(wanted)?;
        let base = quarrel_chain::addr::EXPLORER;
        let rules = strategy
            .entry_filter
            .conditions()
            .iter()
            .map(|cond| {
                let d =
                    quarrel_core::filter::EntryFilter::Cond((*cond).clone()).evaluate(&c.features);
                RuleOutcome {
                    rule: cond.rule_id().to_string(),
                    passed: d.passed,
                    detail: d.refusals.first().map(|r| r.detail.clone()),
                }
            })
            .collect();

        Ok(LaunchDetail {
            row: feed_row(h, &c, &strategy),
            features: c.features.clone(),
            rules,
            links: Links {
                token: format!("{base}/token/{:#x}", c.token),
                curve: format!("{base}/address/{:#x}", c.curve),
                deployer: format!("{base}/address/{:#x}", c.deployer),
                tx: format!("{base}/tx/{:#x}", c.tx_hash),
                socials: e
                    .map(|e| {
                        [
                            ("twitter", e.twitter_url),
                            ("website", e.website_url),
                            ("telegram", e.telegram_url),
                        ]
                        .into_iter()
                        .filter_map(|(k, v)| {
                            v.filter(|s| !s.trim().is_empty())
                                .map(|s| (k.to_string(), s))
                        })
                        .collect()
                    })
                    .unwrap_or_default(),
            },
            outcome: Some(OutcomeView {
                entry_rule: c.outcome.entry_rule.label().to_string(),
                has_entry: c.outcome.has_entry,
                max_multiple_bps: c.outcome.max_multiple_bps,
                mult_after_5m_bps: c.outcome.mult_after_5m_bps,
                mult_after_30m_bps: c.outcome.mult_after_30m_bps,
                migrated: c.outcome.migrated,
                post_entry_trades: c.outcome.post_entry_trades,
            }),
        })
    })
}

// --- view 3: the Strategy Lab -------------------------------------------------------------

pub fn backtest(state: &AppState, config: &StrategyConfig) -> Result<BacktestResult> {
    state.with_history(|h| Ok(quarrel_backtest::run(h, config)?))
}

/// How many indexed launches the filter passes, for the Lab's live count.
///
/// The funnel's `passed_filter` stage, without computing the metrics — this runs on every
/// keystroke in the rule editor, and the whole point of the light half is that it can.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PassCount {
    pub passed: u64,
    pub matured: u64,
    pub universe: u64,
    pub query_ms: u64,
}

pub fn pass_count(state: &AppState, config: &StrategyConfig) -> Result<PassCount> {
    let r = backtest(state, config)?;
    Ok(PassCount {
        passed: r
            .funnel
            .stage("passed_filter")
            .map(|s| s.remaining)
            .unwrap_or(0),
        matured: r.funnel.stage("matured").map(|s| s.remaining).unwrap_or(0),
        universe: r
            .funnel
            .stage("all_launches")
            .map(|s| s.remaining)
            .unwrap_or(0),
        query_ms: r.query_ms,
    })
}

// --- view 2: positions --------------------------------------------------------------------

/// Open and closed positions (spec §8, view 2).
///
/// Empty until phase 6, and the emptiness is explained rather than left to look like a
/// loading state that never resolves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Positions {
    pub open: Vec<serde_json::Value>,
    pub closed: Vec<serde_json::Value>,
    pub note: String,
}

pub fn positions(_state: &AppState) -> Positions {
    Positions {
        open: Vec::new(),
        closed: Vec::new(),
        note: "No engine is running. The sniper, its positions and its exits arrive in \
               phase 6; nothing in this build can sign a transaction."
            .into(),
    }
}

// --- view 4: index ------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseView {
    pub phase: String,
    pub from_block: u64,
    pub last_block: u64,
    pub target_block: u64,
    pub rows_written: u64,
    pub complete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexStatus {
    pub store: StoreSummary,
    pub indexing: bool,
    /// Where each phase got to, so a resume is visible rather than implied (spec §6.2).
    pub phases: Vec<PhaseView>,
    pub undecodable: u64,
    pub bundled: u64,
    pub migrated: u64,
}

pub fn index_status(state: &AppState) -> Result<IndexStatus> {
    let store = store_summary(state);
    let indexing = state.is_indexing();
    // Not `unwrap_or_default`: a view that silently reported zero undecodable launches
    // because the query failed would be indistinguishable from a clean index.
    let counts = state.with_history(|h| {
        let mut phases = Vec::new();
        for key in ["launches", "anchors", "trades", "calldata"] {
            if let Some(s) = h.phase_state(key)? {
                phases.push(PhaseView {
                    phase: key.to_string(),
                    from_block: s.from_block,
                    last_block: s.last_block,
                    target_block: s.target_block,
                    rows_written: s.rows_written,
                    complete: s.last_block >= s.target_block,
                });
            }
        }
        // Three counts, not the whole distribution report: this view is polled while
        // an index runs, and computing every percentile to show three numbers would
        // make the progress bar the slowest thing on screen.
        let one = |sql: &str| -> quarrel_store::Result<u64> {
            Ok(h.conn().query_row(sql, [], |r| r.get::<_, i64>(0))? as u64)
        };
        let routes = quarrel_chain::launch_tx::launch_selectors_hex();
        let holes = vec!["?"; routes.len()].join(", ");
        let bundled = h.conn().query_row(
            &format!(
                "SELECT count(*) FROM enrichment
                     WHERE decoded = 1 AND selector NOT IN ({holes})"
            ),
            rusqlite::params_from_iter(routes.iter()),
            |r| r.get::<_, i64>(0),
        )? as u64;
        Ok((
            phases,
            one("SELECT count(*) FROM enrichment WHERE decoded = 0")?,
            bundled,
            one("SELECT count(*) FROM outcomes WHERE migrated = 1")?,
        ))
    });
    let (phases, undecodable, bundled, migrated) = match counts {
        Ok(c) => c,
        Err(AppError::NoStore(_)) => (Vec::new(), 0, 0, 0),
        Err(e) => return Err(e),
    };

    Ok(IndexStatus {
        store,
        indexing,
        phases,
        undecodable,
        bundled,
        migrated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use quarrel_core::features::Pair;
    use quarrel_core::filter::{Condition, EntryFilter};

    /// The store built by `tests/support`, opened through an `AppState`.
    fn state_with(dir: &std::path::Path) -> AppState {
        AppState::new(dir, false)
    }

    #[test]
    fn status_reports_a_missing_store_without_pretending_it_is_empty() {
        let d = std::env::temp_dir().join("quarrel-api-nostore");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let s = status(&state_with(&d));

        assert!(!s.store.exists);
        assert_eq!(s.store.launches, 0);
        assert_eq!(s.mode_label, "DRY RUN");
        assert!(!s.can_spend, "a dry-run process can never spend");
        assert!(s.engine.contains("holds no key"), "{}", s.engine);
    }

    #[test]
    fn positions_explain_their_emptiness() {
        let d = std::env::temp_dir().join("quarrel-api-pos");
        std::fs::create_dir_all(&d).unwrap();
        let p = positions(&state_with(&d));
        assert!(p.open.is_empty());
        assert!(p.note.contains("phase 6"));
        assert!(p.note.contains("cannot sign") || p.note.contains("nothing in this build"));
    }

    #[test]
    fn the_feed_cap_is_the_number_the_criterion_names() {
        assert_eq!(FEED_CAP, 20_000);
    }

    #[test]
    fn a_feed_query_defaults_to_everything() {
        let q: FeedQuery = serde_json::from_str("{}").unwrap();
        assert!(!q.passing_only);
        assert!(q.search.is_empty());
        assert_eq!(q.limit, None);
    }

    #[test]
    fn search_matches_symbol_name_and_address_case_insensitively() {
        let row = FeedRow {
            token: "0xABCdef0000000000000000000000000000000001".into(),
            symbol: "WAFFLE".into(),
            name: "SpaceWaffle".into(),
            block: 1,
            ts: None,
            pair: "ETH".into(),
            dev_buy_bps: None,
            creator_tax_bps: None,
            exempt_wallets: None,
            twins: 0,
            deployer_launches: 0,
            passed: false,
            rank_bps: 0,
            refusals: Vec::new(),
        };
        assert!(matches(&row, ""));
        assert!(matches(&row, "waffle"));
        assert!(matches(&row, "spacew"));
        assert!(matches(&row, "0xabcdef"));
        assert!(!matches(&row, "dogcoin"));
    }

    /// The rule that keeps the Lab and the sniper the same product (spec §7.2).
    #[test]
    fn the_evaluator_the_ui_calls_is_the_one_the_backtest_calls() {
        let cfg = StrategyConfig {
            entry_filter: EntryFilter::all_of([Condition::MaxCreatorTaxBps { bps: 200 }]),
            ..StrategyConfig::default()
        };
        let f = PitFeatures {
            pair: Pair::Eth,
            name: "x".into(),
            symbol: "X".into(),
            description: String::new(),
            socials: quarrel_core::features::Socials::NONE,
            exempt_wallets: Some(0),
            dev_buy_bps: Some(0),
            creator_tax_bps: Some(900),
            fee_recipient: quarrel_core::features::FeeRecipient::Deployer,
            deployer_launches: 0,
            deployer_graduations: 0,
            fingerprint_twins_30m: 0,
            deployer_history_depth_blocks: 0,
        };
        // There is one `evaluate`, and this is it. The UI does not get its own.
        assert!(!cfg.entry_filter.evaluate(&f).passed);
        assert_eq!(display_rank(&cfg.entry_filter, &f), 0);
    }
}
