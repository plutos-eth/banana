//! Checks the indexed data against itself, and reports the distributions that the
//! provisional thresholds should be derived from.
//!
//! Two jobs:
//!
//! 1. **Replay verification at scale.** The F9 entry reconstruction is only trustworthy if
//!    the curve replay is exact, so this walks every curve's real trade history and checks
//!    each buy's `tokensOut` against what the replayed state would have produced. A single
//!    mismatch means the reconstruction is quietly wrong for some fraction of the universe.
//!
//! 2. **Threshold evidence.** The 6-hour maturity cutoff, the `died` definition and the
//!    deployer-depth floor were picked by reasoning, not from data. A threshold derived
//!    from the indexed window is worth more than one guessed in advance, so this reports
//!    the distributions they should come from.

use banana_core::curve::LaunchConfig;
use banana_store::History;
use banana_store::types::{EntryRule, Side};

use crate::outcomes::{ReplayCheck, verify_replay};

#[derive(Debug, Clone, Default)]
pub struct VerifyReport {
    pub curves_checked: u64,
    pub replay: ReplayCheck,
    /// Curves where at least one buy did not reproduce exactly.
    pub curves_with_mismatch: Vec<String>,
}

impl VerifyReport {
    pub fn is_exact(&self) -> bool {
        self.replay.is_exact()
    }
}

/// Replay every curve and compare against the events.
pub fn verify_all(history: &History, limit: Option<usize>) -> banana_store::Result<VerifyReport> {
    let base = history
        .get_launch_config(0)?
        .unwrap_or_else(LaunchConfig::live_id_0);
    let launches = history.all_launches_lite()?;
    let mut report = VerifyReport::default();

    for l in launches.iter().take(limit.unwrap_or(usize::MAX)) {
        let trades = history.trades_for_curve(l.curve)?;
        if trades.is_empty() {
            continue;
        }
        // Derived from the graduation threshold the launch event carried.
        //
        // `pairTokenEconomics` was tried and is WORSE: it returns the pair's economics
        // *now*, not what the curve launched with, so replay exactness fell from 98.9% to
        // 60%. That is the same current-state trap as socials, in a place it was not
        // expected. Deriving from the per-launch threshold is point-in-time by
        // construction.
        let config =
            LaunchConfig::for_threshold(base.supply, base.curve_fee_bps, l.graduation_threshold);
        let check = verify_replay(&config, &trades);
        report.curves_checked += 1;
        report.replay.checked += check.checked;
        report.replay.exact += check.exact;
        report.replay.mismatched += check.mismatched;
        report.replay.errored += check.errored;
        if !check.is_exact() && report.curves_with_mismatch.len() < 20 {
            report.curves_with_mismatch.push(format!("{:#x}", l.curve));
        }
    }
    Ok(report)
}

/// The distributions the provisional thresholds should be derived from.
#[derive(Debug, Clone, Default)]
pub struct Distributions {
    pub launches: u64,
    pub observed_entries: u64,
    pub reconstructed_entries: u64,
    pub undecodable: u64,
    /// Decoded, but only by finding the launch call nested inside a bundler's calldata.
    ///
    /// Its own number because it is a fact about the launch, not about our decoder: going
    /// through a bundler is the behaviour spec §11's farm detection is looking for.
    pub bundled: u64,
    pub migrated: u64,
    /// Launches with no post-entry trade at all.
    pub no_post_entry_trades: u64,
    /// Launches where no entry price could be established at all.
    pub no_entry: u64,
    /// Percentiles of post-entry trade counts.
    pub post_entry_trades_p: [u64; 5],
    /// Percentiles of `max_multiple_bps`.
    pub max_multiple_p: [u64; 5],
    /// Percentiles of the 5-minute hold multiple.
    pub mult_5m_p: [u64; 5],
    /// How many blocks pass between a launch and its last trade.
    pub lifespan_blocks_p: [u64; 5],
    /// Percentiles of `deployer_history_depth_blocks`.
    pub depth_p: [u64; 5],
    pub twins_nonzero: u64,
    /// Percentiles of `time_to_ath_s`, for deriving the maturity cutoff: a token cannot
    /// be judged before it has had time to peak.
    pub time_to_ath_p: [u64; 5],
    /// Percentiles of blocks of silence at the end of the window, for deriving `died`.
    pub quiet_blocks_p: [u64; 5],
    /// Tokens whose peak is exactly their entry: they never traded above it.
    pub never_above_entry: u64,
    /// Tokens whose 5-minute multiple is below entry.
    pub below_entry_at_5m: u64,
}

/// 10th, 25th, 50th, 75th, 90th.
const PCTS: [f64; 5] = [0.10, 0.25, 0.50, 0.75, 0.90];

fn percentiles(mut v: Vec<u64>) -> [u64; 5] {
    if v.is_empty() {
        return [0; 5];
    }
    v.sort_unstable();
    let mut out = [0u64; 5];
    for (i, p) in PCTS.iter().enumerate() {
        let idx = ((v.len() - 1) as f64 * p).round() as usize;
        out[i] = v[idx];
    }
    out
}

pub fn distributions(history: &History) -> banana_store::Result<Distributions> {
    let conn = history.conn();

    let launches = history.launch_count()?;
    let observed_entries = conn.query_row(
        "SELECT count(*) FROM outcomes WHERE entry_rule = ?1",
        [EntryRule::ObservedUntaxedBuy as i64],
        |r| r.get::<_, i64>(0),
    )? as u64;
    let reconstructed_entries = conn.query_row(
        "SELECT count(*) FROM outcomes WHERE entry_rule = ?1",
        [EntryRule::ReconstructedAtWindowEnd as i64],
        |r| r.get::<_, i64>(0),
    )? as u64;
    let undecodable = conn.query_row(
        "SELECT count(*) FROM enrichment WHERE decoded = 0",
        [],
        |r| r.get::<_, i64>(0),
    )? as u64;
    // Anything that decoded but whose transaction selector is not itself a launch route
    // got there through a nested frame. The route list comes from the ABI rather than from
    // hex literals here: writing them out is how the three-argument `launchToken` -- 23% of
    // launches -- ended up counted as a bundler on the first attempt at this number.
    let routes = banana_chain::launch_tx::launch_selectors_hex();
    let holes = vec!["?"; routes.len()].join(", ");
    let bundled = conn.query_row(
        &format!(
            "SELECT count(*) FROM enrichment
             WHERE decoded = 1 AND selector NOT IN ({holes})"
        ),
        rusqlite::params_from_iter(routes.iter()),
        |r| r.get::<_, i64>(0),
    )? as u64;
    let migrated = conn.query_row(
        "SELECT count(*) FROM outcomes WHERE migrated = 1",
        [],
        |r| r.get::<_, i64>(0),
    )? as u64;
    let no_post_entry_trades = conn.query_row(
        "SELECT count(*) FROM outcomes WHERE post_entry_trades = 0",
        [],
        |r| r.get::<_, i64>(0),
    )? as u64;
    let no_entry = conn.query_row(
        "SELECT count(*) FROM outcomes WHERE entry_price IS NULL",
        [],
        |r| r.get::<_, i64>(0),
    )? as u64;
    let twins_nonzero = conn.query_row(
        "SELECT count(*) FROM pit_features WHERE fingerprint_twins_30m > 0",
        [],
        |r| r.get::<_, i64>(0),
    )? as u64;

    // `Option<i64>`, because several of these columns are an arithmetic expression over a
    // nullable one. `(SELECT max(block) FROM trades) - last_trade_block` is NULL on a store
    // that has outcomes but no trades, and reading it as `i64` made the whole report fail
    // rather than report an empty distribution.
    let col = |sql: &str| -> banana_store::Result<Vec<u64>> {
        let mut stmt = conn.prepare(sql)?;
        let v = stmt
            .query_map([], |r| r.get::<_, Option<i64>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v.into_iter().flatten().map(|x| x.max(0) as u64).collect())
    };

    Ok(Distributions {
        launches,
        observed_entries,
        reconstructed_entries,
        undecodable,
        bundled,
        migrated,
        no_post_entry_trades,
        no_entry,
        twins_nonzero,
        post_entry_trades_p: percentiles(col("SELECT post_entry_trades FROM outcomes")?),
        max_multiple_p: percentiles(col(
            "SELECT max_multiple_bps FROM outcomes WHERE max_multiple_bps IS NOT NULL",
        )?),
        mult_5m_p: percentiles(col(
            "SELECT mult_after_5m_bps FROM outcomes WHERE mult_after_5m_bps IS NOT NULL",
        )?),
        lifespan_blocks_p: percentiles(col("SELECT o.last_trade_block - l.block FROM outcomes o
             JOIN launches l ON l.token = o.token
             WHERE o.last_trade_block IS NOT NULL")?),
        depth_p: percentiles(col(
            "SELECT deployer_history_depth_blocks FROM pit_features",
        )?),
        time_to_ath_p: percentiles(col(
            "SELECT time_to_ath_s FROM outcomes WHERE time_to_ath_s IS NOT NULL",
        )?),
        quiet_blocks_p: percentiles(col(
            "SELECT (SELECT max(block) FROM trades) - last_trade_block FROM outcomes
             WHERE last_trade_block IS NOT NULL",
        )?),
        never_above_entry: conn.query_row(
            "SELECT count(*) FROM outcomes WHERE max_multiple_bps = 10000",
            [],
            |r| r.get::<_, i64>(0),
        )? as u64,
        below_entry_at_5m: conn.query_row(
            "SELECT count(*) FROM outcomes WHERE mult_after_5m_bps < 10000",
            [],
            |r| r.get::<_, i64>(0),
        )? as u64,
    })
}

/// How many trades a curve saw, split by side, for a sanity read on the trade scan.
pub fn side_counts(history: &History) -> banana_store::Result<(u64, u64, u64)> {
    let conn = history.conn();
    let buys = conn.query_row(
        "SELECT count(*) FROM trades WHERE side = ?1",
        [Side::Buy as i64],
        |r| r.get::<_, i64>(0),
    )? as u64;
    let sells = conn.query_row(
        "SELECT count(*) FROM trades WHERE side = ?1",
        [Side::Sell as i64],
        |r| r.get::<_, i64>(0),
    )? as u64;
    let taxed = conn.query_row(
        "SELECT count(*) FROM trades WHERE snipe_tax IS NOT NULL",
        [],
        |r| r.get::<_, i64>(0),
    )? as u64;
    Ok((buys, sells, taxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_of_an_empty_column_are_zero_not_a_panic() {
        assert_eq!(percentiles(vec![]), [0; 5]);
    }

    #[test]
    fn percentiles_pick_the_expected_positions() {
        let v: Vec<u64> = (1..=100).collect();
        let p = percentiles(v);
        assert_eq!(p[2], 51, "median of 1..100");
        assert!(p[0] < p[1] && p[1] < p[2] && p[2] < p[3] && p[3] < p[4]);
    }

    #[test]
    fn a_single_value_is_every_percentile() {
        assert_eq!(percentiles(vec![7]), [7; 5]);
    }

    #[test]
    fn verifying_an_empty_store_reports_exact_rather_than_failing() {
        let h = History::in_memory().unwrap();
        let r = verify_all(&h, None).unwrap();
        assert_eq!(r.curves_checked, 0);
        assert!(r.is_exact(), "nothing to check is not a failure");
    }
}
