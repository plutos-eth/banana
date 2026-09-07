//! Orchestration: run the four phases, then derive features and outcomes.
//!
//! Resume is the default, not a flag. Every phase records its last fully-processed block,
//! so an interrupted run picks up where it stopped and the covered ranges are re-scanned
//! only as far back as the last checkpoint — where the idempotent inserts make the overlap
//! free (spec §6.2).

use std::collections::HashSet;

use alloy_primitives::{Address, U256};
use quarrel_chain::gate::Priority;
use quarrel_chain::rpc::Client;
use quarrel_core::curve::LaunchConfig;
use quarrel_core::features::Socials;
use quarrel_store::history::{History, OutcomeRow, PitFeaturesRow};
use quarrel_store::types::EntryRule;

use crate::features::{FeatureBuilder, Fingerprint, LaunchFacts};
use crate::outcomes::{self, OutcomeInput};
use crate::progress::{Phase, Progress};
use crate::scan::{self, ScanError};

#[derive(Debug, Clone)]
pub struct IndexPlan {
    pub from_block: u64,
    pub to_block: u64,
    /// Skip the calldata phase, leaving socials and exempt-wallet counts unreadable.
    ///
    /// A fast partial index. The Lab must then refuse any filter that reads those fields,
    /// rather than treating a missing value as an absent one.
    pub skip_calldata: bool,
    /// Blocks between sampled timestamp anchors.
    pub anchor_every: u64,
    /// Concurrent transaction fetches in the calldata phase.
    pub calldata_concurrency: usize,
    /// Size the F9 reconstruction quotes when no untaxed buy exists.
    pub reference_size: U256,
}

impl IndexPlan {
    /// A 24-hour window ending at `head`. ~856,500 blocks at the measured 100.87 ms.
    pub fn last_24h(head: u64) -> Self {
        Self {
            from_block: head.saturating_sub(856_500),
            to_block: head,
            skip_calldata: false,
            anchor_every: 500,
            calldata_concurrency: 8,
            reference_size: U256::from(10_000_000_000_000_000u64), // 0.01 ETH
        }
    }

    pub fn blocks(&self) -> u64 {
        self.to_block.saturating_sub(self.from_block) + 1
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexReport {
    pub launch_rows: u64,
    pub trade_rows: u64,
    pub enrichment_rows: u64,
    pub anchor_rows: u64,
    pub outcomes_written: u64,
    pub pit_rows: u64,
    pub observed_entries: u64,
    pub reconstructed_entries: u64,
    pub undecodable_launches: u64,
    pub elapsed_secs: u64,
}

/// Run the index. Emits progress through `sink`, on every advance rather than once a phase.
pub async fn run(
    client: &Client,
    history: &mut History,
    plan: &IndexPlan,
    sink: crate::progress::Sink,
) -> Result<IndexReport, ScanError> {
    let started = std::time::Instant::now();
    let mut progress = Progress::new().with_sink(sink);
    if plan.skip_calldata {
        progress.skip(Phase::Calldata);
    }
    let mut report = IndexReport::default();

    // The launch config is read once per id, not once per launch.
    if history.get_launch_config(0)?.is_none() {
        use quarrel_chain::abi::IPonsFactory;
        match client
            .call(
                quarrel_chain::addr::PONS_FACTORY,
                &IPonsFactory::getLaunchConfigCall { id: U256::ZERO },
                Priority::Bulk,
            )
            .await
        {
            Ok(c) => history.upsert_launch_config(
                0,
                &LaunchConfig {
                    supply: c.supply,
                    curve_fee_bps: c.curveFeeBps.try_into().unwrap_or(100),
                    phantom_quote: c.phantomQuote,
                    graduation_threshold: c.graduationThreshold,
                },
            )?,
            // The recorded constants are re-verified by `doctor`; falling back keeps an
            // index runnable when the factory read fails, and the fallback is stated.
            Err(e) => {
                tracing::warn!(error = %e, "launch config read failed; using recorded constants");
                history.upsert_launch_config(0, &LaunchConfig::live_id_0())?;
            }
        }
    }

    // --- A ---------------------------------------------------------------------------
    let resume = |h: &History, p: Phase, default_from: u64| -> u64 {
        h.phase_state(p.key())
            .ok()
            .flatten()
            .filter(|s| s.target_block == plan.to_block && s.from_block == plan.from_block)
            // Resume from the last checkpointed block, not after it: the overlap costs
            // nothing because the inserts are idempotent, and it removes any doubt about
            // whether a partially-handled chunk was written.
            .map(|s| s.last_block)
            .unwrap_or(default_from)
    };

    let a_from = resume(history, Phase::Launches, plan.from_block);
    report.launch_rows =
        scan::scan_launches(client, history, &mut progress, a_from, plan.to_block).await?;

    // --- D before C, so timestamps exist while outcomes are computed -------------------
    let d_from = resume(history, Phase::Anchors, plan.from_block);
    report.anchor_rows = scan::scan_anchors(
        client,
        history,
        &mut progress,
        d_from,
        plan.to_block,
        plan.anchor_every,
    )
    .await?;

    // --- B ---------------------------------------------------------------------------
    let b_from = resume(history, Phase::Trades, plan.from_block);
    report.trade_rows =
        scan::scan_trades(client, history, &mut progress, b_from, plan.to_block).await?;

    // Pair economics: one read per distinct pair token, needed before outcomes because a
    // curve cannot be replayed without its pair's phantom reserve.
    scan::scan_pair_economics(client, history).await?;

    // --- C ---------------------------------------------------------------------------
    if !plan.skip_calldata {
        report.enrichment_rows =
            scan::scan_calldata(client, history, &mut progress, plan.calldata_concurrency).await?;
    }

    // --- derive ------------------------------------------------------------------------
    let derived = derive(history, plan)?;
    report.pit_rows = derived.0;
    report.outcomes_written = derived.1;
    report.observed_entries = derived.2;
    report.reconstructed_entries = derived.3;
    report.undecodable_launches = derived.4;
    report.elapsed_secs = started.elapsed().as_secs();

    Ok(report)
}

/// Compute point-in-time features and outcomes for every launch.
///
/// Returns `(pit_rows, outcomes, observed, reconstructed, undecodable)`.
fn derive(history: &mut History, plan: &IndexPlan) -> Result<(u64, u64, u64, u64, u64), ScanError> {
    let graduated = history.graduation_blocks()?;
    // Supply and curve fee come from the launch config; the phantom reserve does NOT.
    // It is per pair token, so it is derived per launch from the graduation threshold the
    // event carried. Only 40% of launches are ETH-paired, so using the ETH config for all
    // of them made 87% of replayed buys wrong.
    let base = history
        .get_launch_config(0)?
        .unwrap_or_else(LaunchConfig::live_id_0);

    // Launches in ascending block order: the point-in-time guarantee of `FeatureBuilder`
    // depends on this ordering, not merely benefits from it.
    let launches = history.all_launches_lite()?;
    let mut builder = FeatureBuilder::new(plan.from_block, graduated.clone());

    let mut pit_rows = 0u64;
    let mut outcomes_written = 0u64;
    let mut observed = 0u64;
    let mut reconstructed = 0u64;
    let mut undecodable = 0u64;

    for l in &launches {
        let enrichment = history.enrichment_for(l.token)?;
        if enrichment.as_ref().is_none_or(|e| !e.decoded) {
            undecodable += 1;
        }

        let fingerprint = match &enrichment {
            Some(e) => Fingerprint::new(
                e.dev_buy_quote,
                e.creator_tax_bps,
                e.socials,
                e.exempt_wallets,
            ),
            None => Fingerprint::new(None, None, Socials::UNKNOWN, None),
        };

        let pit = builder.push(&LaunchFacts {
            token: l.token,
            deployer: l.deployer,
            block: l.block,
            fingerprint,
        });
        history.upsert_pit_features(&PitFeaturesRow {
            token: pit.token,
            deployer_launches: pit.deployer_launches,
            deployer_graduations: pit.deployer_graduations,
            deployer_grad_rate_bps: pit.deployer_grad_rate_bps,
            fingerprint: pit.fingerprint.clone(),
            fingerprint_twins_30m: pit.fingerprint_twins_30m,
            deployer_history_depth_blocks: pit.deployer_history_depth_blocks,
        })?;
        pit_rows += 1;

        // Outcome.
        let trades = history.trades_for_curve(l.curve)?;
        // Derived from the graduation threshold the launch event carried.
        //
        // `pairTokenEconomics` was tried and is WORSE: it returns the pair's economics
        // *now*, not what the curve launched with, so replay exactness fell from 98.9% to
        // 60%. That is the same current-state trap as socials, in a place it was not
        // expected. Deriving from the per-launch threshold is point-in-time by
        // construction.
        let config =
            LaunchConfig::for_threshold(base.supply, base.curve_fee_bps, l.graduation_threshold);
        let exempt: HashSet<Address> = HashSet::new(); // declared wallets, filled below
        let ts_at = |b: u64| history.timestamp_at(b).ok().flatten();
        let input = OutcomeInput {
            launch_block: l.block,
            launch_tx: l.tx_hash,
            config: &config,
            trades: &trades,
            exempt: &exempt,
            migrated: graduated.contains_key(&l.token),
            reference_size: plan.reference_size,
            ts_at: &ts_at,
            head_block: plan.to_block,
        };
        let o = outcomes::compute(&input);
        match o.entry_rule {
            EntryRule::ObservedUntaxedBuy => observed += 1,
            EntryRule::ReconstructedAtWindowEnd => reconstructed += 1,
        }
        history.upsert_outcome(&OutcomeRow {
            token: l.token,
            entry_rule: o.entry_rule,
            entry_block: o.entry_block,
            entry_price: o.entry_price,
            entry_tokens: o.entry_tokens,
            ath_price: o.ath_price,
            ath_block: o.ath_block,
            max_multiple_bps: o.max_multiple_bps,
            time_to_ath_s: o.time_to_ath_s,
            mult_after_5m_bps: o.mult_after_5m_bps,
            mult_after_30m_bps: o.mult_after_30m_bps,
            migrated: o.migrated,
            died: o.died,
            distinct_buyers_1m: o.distinct_buyers_1m,
            every_early_buy_taxed: o.every_early_buy_taxed,
            post_entry_trades: o.post_entry_trades,
            last_trade_block: o.last_trade_block,
            observed_blocks: o.observed_blocks,
        })?;
        outcomes_written += 1;
    }

    Ok((
        pit_rows,
        outcomes_written,
        observed,
        reconstructed,
        undecodable,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_24h_plan_covers_the_measured_block_count() {
        let p = IndexPlan::last_24h(56_760_595);
        assert_eq!(p.blocks(), 856_501, "~24 h at the measured 100.87 ms");
        assert_eq!(p.to_block, 56_760_595);
        assert!(!p.skip_calldata, "the calldata phase is on by default");
    }

    #[test]
    fn the_default_reference_size_matches_the_default_entry_size() {
        // The F9 reconstruction must quote the size the sniper would actually have bought,
        // or the reconstructed entry price is for a trade nobody would have made.
        let p = IndexPlan::last_24h(1_000_000);
        assert_eq!(
            p.reference_size,
            quarrel_core::strategy::EntryModel::default().size_wei
        );
    }
}
