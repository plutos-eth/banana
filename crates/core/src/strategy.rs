//! `StrategyConfig` — one type, two consumers.
//!
//! Spec §7.2. This struct serialised to JSON **is** a saved strategy. Arming a backtested
//! strategy in the sniper is loading the same file, with no translation step: that is the
//! whole point of the product, and it only stays true if nothing here is duplicated into a
//! second "live" type.
//!
//! Which half reads what:
//!
//! | field | backtest | live sniper |
//! |---|---|---|
//! | `entry_filter` | yes | yes |
//! | `entry_model` | yes (entry timing) | yes |
//! | `success_target` | yes | **ignored** |
//! | `exits` | **ignored entirely** (§5.6) | yes |
//! | `live_guards` | **ignored** | yes |
//!
//! `exits` being ignored by the backtest is deliberate and load-bearing: the Lab measures
//! the objective fate of tokens that passed the filter and does not simulate selling, which
//! keeps its light half a filter-plus-groupby over precomputed columns.

use alloy_primitives::U256;
use serde::{Deserialize, Serialize};

use crate::Bps;
use crate::features::Pair;
use crate::filter::{Condition, EntryFilter};

/// One ETH in wei.
const ONE_ETH: u64 = 1_000_000_000_000_000_000;

/// How and when to enter. Shared by both halves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryModel {
    /// Buy only once the opening tax has decayed to at or below this.
    ///
    /// The tax opens at 9900 bps and decays to zero over ~3 seconds, so racing block one
    /// hands ~97% of the spend to the creator. The whole edge is *when*, not how much gas
    /// (there is no mempool and no priority fee on this chain).
    pub max_tax_bps: Bps,
    /// Size of one entry, in wei of the pair asset.
    pub size_wei: U256,
    pub slippage_bps: Bps,
    /// Give up waiting for the decay after this long.
    pub max_wait_ms: u64,
}

impl Default for EntryModel {
    fn default() -> Self {
        Self {
            max_tax_bps: 300,
            size_wei: U256::from(ONE_ETH / 100), // 0.01 ETH
            slippage_bps: 300,
            max_wait_ms: 12_000,
        }
    }
}

/// What counts as success in the Lab. Backtest only.
///
/// The default is a **fixed-hold multiple**, and that is not an arbitrary choice: it is the
/// number a simple, executable rule would actually have produced (§5.4).
/// [`SuccessTarget::MaxMultiple`] is available but must always be presented as a *peak*,
/// never as profit, return, or anything anybody earned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SuccessTarget {
    /// Reached at least `multiple_bps` when held for exactly `minutes`.
    FixedHoldMultiple { minutes: u32, multiple_bps: Bps },
    /// `PoolGraduated` fired.
    ///
    /// Measured 2026-09-07 at roughly 1 launch in 78, so a filter passing 500 tokens
    /// yields about 6 of these. The funnel shows the raw count beside any rate for exactly
    /// this reason (PLAN.md F4).
    ReachedMigration,
    /// Peak multiple crossed a threshold. **Label as peak, never as profit** (§5.4).
    MaxMultiple { multiple_bps: Bps },
}

impl Default for SuccessTarget {
    fn default() -> Self {
        // 2x held for 5 minutes: executable, and it is what the headline reports.
        SuccessTarget::FixedHoldMultiple {
            minutes: 5,
            multiple_bps: 20_000,
        }
    }
}

impl SuccessTarget {
    /// True when this target is a peak figure and must carry the §5.4 labelling.
    pub fn is_peak_based(&self) -> bool {
        matches!(self, SuccessTarget::MaxMultiple { .. })
    }
}

/// Sell part of a position when a multiple is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartialExit {
    /// Trigger, as a multiple of entry in basis points. 20000 = 2x.
    pub at_multiple_bps: Bps,
    /// Fraction of the *original* position to sell, in basis points.
    pub sell_bps: Bps,
}

/// When to close. **Live only — `quarrel-backtest` ignores this field entirely** (§5.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitPolicy {
    pub take_profit_bps: Option<Bps>,
    pub stop_loss_bps: Option<Bps>,
    pub trailing_bps: Option<Bps>,
    pub max_hold_secs: Option<u64>,
    /// Applied in ascending `at_multiple_bps` order, each firing at most once.
    pub partials: Vec<PartialExit>,
}

impl Default for ExitPolicy {
    fn default() -> Self {
        // bodkin's defaults, so the baseline is recognisable.
        Self {
            take_profit_bps: Some(8_000),
            stop_loss_bps: Some(3_500),
            trailing_bps: Some(2_500),
            max_hold_secs: Some(45 * 60),
            partials: Vec::new(),
        }
    }
}

/// The walls between a bad hour and a drained wallet (spec §7.3).
///
/// Every one of these is enforced in code and individually tested. They are not advisory
/// and the UI cannot relax them past what is set here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveGuards {
    /// Most that may be spent on any single entry.
    pub size_per_buy_wei: U256,
    /// Most that may be held in one position, across adds.
    pub position_cap_wei: U256,
    /// Once this much has gone into entries this session, nothing fires regardless of
    /// signal quality.
    pub session_budget_wei: U256,
    pub max_open_positions: u32,
}

impl Default for LiveGuards {
    fn default() -> Self {
        Self {
            size_per_buy_wei: U256::from(ONE_ETH / 100), // 0.01 ETH
            position_cap_wei: U256::from(ONE_ETH / 100),
            session_budget_wei: U256::from(ONE_ETH / 20), // 0.05 ETH
            max_open_positions: 3,
        }
    }
}

/// A complete strategy. Serialised to JSON, this file is the strategy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrategyConfig {
    /// Read by **both** the backtest and the live sniper, from this same field.
    pub entry_filter: EntryFilter,
    pub entry_model: EntryModel,
    /// Backtest only.
    pub success_target: SuccessTarget,
    /// Live only. Ignored entirely by `quarrel-backtest` (§5.6).
    pub exits: ExitPolicy,
    /// Live only.
    pub live_guards: LiveGuards,
}

impl Default for StrategyConfig {
    /// The bodkin-derived baseline of spec §7.1, so a user recognises where they started.
    ///
    /// `require_twitter` is here as a genuine point-in-time rule: socials are read from
    /// launch calldata, not from current contract state. See `docs/FINDINGS.md` §4.
    fn default() -> Self {
        Self {
            entry_filter: EntryFilter::all_of([
                Condition::RequireTwitter,
                Condition::DevBuyBps {
                    min: Some(100),
                    max: Some(600),
                },
                Condition::MaxCreatorTaxBps { bps: 200 },
                Condition::MaxExemptWallets { max: 2 },
                Condition::MaxFingerprintTwins { max: 1 },
                Condition::PairIn {
                    pairs: vec![Pair::Eth],
                },
            ]),
            entry_model: EntryModel::default(),
            success_target: SuccessTarget::default(),
            exits: ExitPolicy::default(),
            live_guards: LiveGuards::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_strategy_round_trips_through_json() {
        let c = StrategyConfig::default();
        let json = serde_json::to_string_pretty(&c).unwrap();
        let back: StrategyConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c, "a saved strategy must reload byte-identically");
    }

    /// Spec §7.2: arming a backtested strategy must be loading the same file. This test is
    /// what stops a second, divergent "live config" type appearing later.
    #[test]
    fn one_file_serves_both_halves_with_no_translation() {
        let saved = serde_json::to_string(&StrategyConfig::default()).unwrap();

        // The Lab reads the filter from the file...
        let for_lab: StrategyConfig = serde_json::from_str(&saved).unwrap();
        // ...and the sniper reads the same field from the same bytes.
        let for_sniper: StrategyConfig = serde_json::from_str(&saved).unwrap();

        assert_eq!(for_lab.entry_filter, for_sniper.entry_filter);
        assert_eq!(for_lab.entry_model, for_sniper.entry_model);
    }

    #[test]
    fn defaults_match_the_documented_money_guards() {
        let g = LiveGuards::default();
        assert_eq!(
            g.session_budget_wei,
            U256::from(50_000_000_000_000_000u64),
            "0.05 ETH"
        );
        assert_eq!(
            g.size_per_buy_wei,
            U256::from(10_000_000_000_000_000u64),
            "0.01 ETH"
        );
        assert_eq!(g.max_open_positions, 3);
    }

    #[test]
    fn the_default_success_target_is_a_fixed_hold_not_a_peak() {
        let t = SuccessTarget::default();
        assert!(
            !t.is_peak_based(),
            "the headline must be what an executable rule produced, never the peak (§5.4)"
        );
        assert!(matches!(t, SuccessTarget::FixedHoldMultiple { .. }));
    }

    #[test]
    fn peak_based_targets_are_flagged_for_labelling() {
        assert!(
            SuccessTarget::MaxMultiple {
                multiple_bps: 50_000
            }
            .is_peak_based()
        );
        assert!(!SuccessTarget::ReachedMigration.is_peak_based());
    }

    #[test]
    fn the_default_entry_model_waits_for_the_tax_to_decay() {
        let m = EntryModel::default();
        assert_eq!(m.max_tax_bps, 300);
        assert!(
            m.max_tax_bps < 9_900,
            "buying at full draw is the mistake the whole design exists to avoid"
        );
    }

    #[test]
    fn the_default_filter_does_not_need_deployer_history() {
        // If it did, every backtest would silently inherit the C2 depth restriction.
        assert!(
            !StrategyConfig::default()
                .entry_filter
                .uses_deployer_history()
        );
    }
}
