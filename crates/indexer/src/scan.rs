//! The four index phases, against the chain.
//!
//! Everything here runs at [`Priority::Bulk`], so a backfill can never delay a live entry
//! (spec §6.2). Every phase checkpoints after each chunk, so a failure at 70% resumes at
//! 70% rather than restarting.

use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolEvent;
use banana_chain::abi::{IPonsCurve, IPonsFactory};
use banana_chain::gate::{Priority, RpcError};
use banana_chain::rpc::{Client, LogFilter, RawLog};
use banana_chain::{addr, launch_tx};
use banana_store::history::{
    EnrichmentRow, History, LaunchRow, PendingCalldata, PhaseState, TradeRow,
};
use banana_store::types::Side;

use crate::chunking::{Chunker, ChunkerConfig, Range};
use crate::progress::{Phase, Progress};

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error(transparent)]
    Rpc(#[from] RpcError),
    #[error(transparent)]
    Store(#[from] banana_store::StoreError),
    #[error(
        "block {block} matches more than the endpoint will return even alone; the range cannot be split further"
    )]
    Unsplittable { block: u64 },
}

type Result<T> = std::result::Result<T, ScanError>;

/// What a chunked log scan needs to know about the window it is covering.
struct ScanSpec {
    phase: Phase,
    from: u64,
    to: u64,
    config: ChunkerConfig,
}

/// Run a chunked log scan, splitting on the result cap and checkpointing as it goes.
///
/// The closure receives each chunk's logs and returns how many rows it wrote.
async fn scan_logs<F>(
    client: &Client,
    history: &mut History,
    progress: &mut Progress,
    spec: ScanSpec,
    build_filter: impl Fn(Range) -> LogFilter,
    mut handle: F,
) -> Result<u64>
where
    F: FnMut(&mut History, &[RawLog]) -> Result<u64>,
{
    let ScanSpec {
        phase,
        from,
        to,
        config,
    } = spec;
    let mut chunker = Chunker::new(from, to, config);
    let mut rows_total = 0u64;

    while let Some(range) = chunker.next_range() {
        let filter = build_filter(range);
        match client.get_logs(&filter, Priority::Bulk).await {
            Ok(logs) => {
                let wrote = handle(history, &logs)?;
                rows_total += wrote;
                chunker.succeeded(range);
                progress.advance(phase, range.blocks(), wrote);
                history.checkpoint(
                    phase.key(),
                    PhaseState {
                        from_block: from,
                        last_block: range.to,
                        target_block: to,
                        rows_written: rows_total,
                    },
                )?;
            }
            // Two different ways of saying "this range is too expensive", handled
            // identically: waiting cannot fix either, only a smaller range can.
            Err(RpcError::TooManyResults { .. }) | Err(RpcError::QueryTimedOut { .. }) => {
                tracing::debug!(
                    from = range.from,
                    to = range.to,
                    "range too expensive for the endpoint, splitting"
                );
                if !chunker.too_many_results(range) {
                    return Err(ScanError::Unsplittable { block: range.from });
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(rows_total)
}

// --- phase A: launches, graduations, sweeps ---------------------------------------------

/// Factory-address-filtered, so cheap: measured ~10-45 requests and ~20 MB for 24 hours.
pub async fn scan_launches(
    client: &Client,
    history: &mut History,
    progress: &mut Progress,
    from: u64,
    to: u64,
) -> Result<u64> {
    progress.begin(Phase::Launches, to.saturating_sub(from) + 1);

    let topics = [
        IPonsFactory::TokenLaunched::SIGNATURE_HASH,
        IPonsFactory::PoolGraduated::SIGNATURE_HASH,
        IPonsFactory::LaunchSwept::SIGNATURE_HASH,
    ];

    let rows = scan_logs(
        client,
        history,
        progress,
        ScanSpec {
            phase: Phase::Launches,
            from,
            to,
            // Launches are ~25 per 1,000 blocks, so far bigger chunks are safe here than
            // for trades. The cap is on matched logs, not blocks.
            config: ChunkerConfig {
                initial: 50_000,
                max: 200_000,
                ..Default::default()
            },
        },
        |r| {
            LogFilter::new(r.from, r.to)
                .address(addr::PONS_FACTORY)
                .topics(topics)
        },
        |h, logs| {
            let mut launches = Vec::new();
            let mut grads = Vec::new();
            let mut sweeps = Vec::new();

            for l in logs {
                let Some(t0) = l.topic0() else { continue };
                if t0 == IPonsFactory::TokenLaunched::SIGNATURE_HASH {
                    if let Some(row) = decode_launch_log(l) {
                        launches.push(row);
                    }
                } else if t0 == IPonsFactory::PoolGraduated::SIGNATURE_HASH {
                    if let Some(token) = topic_address(l, 1) {
                        grads.push((token, l.block_number, l.tx_hash, l.log_index));
                    }
                } else if t0 == IPonsFactory::LaunchSwept::SIGNATURE_HASH
                    && let Some(token) = topic_address(l, 1)
                {
                    sweeps.push((token, l.block_number, l.tx_hash, l.log_index));
                }
            }

            let mut n = h.insert_launches(&launches)? as u64;
            n += h.insert_graduations(&grads)? as u64;
            n += h.insert_sweeps(&sweeps)? as u64;
            Ok(n)
        },
    )
    .await?;

    progress.finish(Phase::Launches);
    Ok(rows)
}

fn topic_address(l: &RawLog, i: usize) -> Option<Address> {
    l.topics.get(i).map(|t| Address::from_slice(&t.0[12..]))
}

/// Adapt the shared decoder to the store's row type.
///
/// The decoding itself lives in `banana_chain::launch_log` because the sniper reads the
/// same event, and one event decoded two ways is a bug waiting for whichever half is
/// exercised less.
fn decode_launch_log(l: &RawLog) -> Option<LaunchRow> {
    let d = banana_chain::launch_log::decode(l)?;
    Some(LaunchRow {
        token: d.token,
        curve: d.curve,
        deployer: d.deployer,
        pair_token: d.pair_token,
        launch_config_id: d.launch_config_id,
        graduation_threshold: d.graduation_threshold,
        block: d.block,
        tx_hash: d.tx_hash,
        log_index: d.log_index,
    })
}

// --- phase B: trades ---------------------------------------------------------------------

/// Topic-filtered across **all** curve addresses, because every curve is its own contract.
///
/// The three events go in one query as a topic0 OR group, so `SnipeTaxCharged` costs
/// nothing extra — measured at 44 logs per 2,000 blocks against 2,031 trade logs, about 2%
/// of the volume. That event is what identifies the end of the opening-tax window, so
/// fetching it alongside is what makes the entry price computable without timestamps.
pub async fn scan_trades(
    client: &Client,
    history: &mut History,
    progress: &mut Progress,
    from: u64,
    to: u64,
) -> Result<u64> {
    progress.begin(Phase::Trades, to.saturating_sub(from) + 1);

    let topics = [
        IPonsCurve::CurveBuy::SIGNATURE_HASH,
        IPonsCurve::CurveSell::SIGNATURE_HASH,
        IPonsCurve::SnipeTaxCharged::SIGNATURE_HASH,
    ];

    let rows = scan_logs(
        client,
        history,
        progress,
        ScanSpec {
            phase: Phase::Trades,
            from,
            to,
            config: ChunkerConfig::default(),
        },
        |r| LogFilter::new(r.from, r.to).topics(topics),
        |h, logs| {
            let mut trades = Vec::new();
            // (tx_hash, curve) -> amount, matched to its buy after the batch is written.
            let mut snipe: Vec<(B256, Address, U256)> = Vec::new();

            for l in logs {
                let Some(t0) = l.topic0() else { continue };
                if t0 == IPonsCurve::SnipeTaxCharged::SIGNATURE_HASH {
                    if l.data.len() >= 32 {
                        snipe.push((l.tx_hash, l.address, U256::from_be_slice(&l.data[..32])));
                    }
                    continue;
                }
                let is_buy = t0 == IPonsCurve::CurveBuy::SIGNATURE_HASH;
                if !is_buy && t0 != IPonsCurve::CurveSell::SIGNATURE_HASH {
                    continue;
                }
                if let Some(row) = decode_trade_log(l, is_buy) {
                    trades.push(row);
                }
            }

            let n = h.insert_trades(&trades)? as u64;
            for (tx, curve, amount) in snipe {
                h.set_snipe_tax(tx, curve, amount)?;
            }
            Ok(n)
        },
    )
    .await?;

    progress.finish(Phase::Trades);
    Ok(rows)
}

fn decode_trade_log(l: &RawLog, is_buy: bool) -> Option<TradeRow> {
    let actor = topic_address(l, 1)?;
    let recipient = topic_address(l, 2)?;
    let words: Vec<U256> = l
        .data
        .as_chunks::<32>()
        .0
        .iter()
        .map(|w| U256::from_be_bytes::<32>(*w))
        .collect();
    if words.len() < 4 {
        return None;
    }
    Some(TradeRow {
        tx_hash: l.tx_hash,
        log_index: l.log_index,
        curve: l.address,
        block: l.block_number,
        tx_index: l.tx_index,
        side: if is_buy { Side::Buy } else { Side::Sell },
        actor,
        recipient,
        amount_in: words[0],
        amount_out: words[1],
        fee: words[2],
        tax: words[3],
        snipe_tax: None,
    })
}

// --- phase C: launch calldata ------------------------------------------------------------

/// One `eth_getTransactionByHash` per launch.
///
/// **Spec §6.1 does not cost this phase at all**, yet §5.1 and §5.3 require it: exempt
/// wallets and point-in-time socials exist nowhere else. It is ~20,000 requests a day,
/// which is roughly a 45x increase on the specification's estimate (PLAN.md F1).
///
/// It is affordable because it goes to publicnode, which took 8-way concurrency at 35
/// ms/request with zero refusals, and never touches the metered logs endpoint.
pub async fn scan_calldata(
    client: &Client,
    history: &mut History,
    progress: &mut Progress,
    concurrency: usize,
) -> Result<u64> {
    let pending = history.launches_needing_calldata()?;
    progress.begin(Phase::Calldata, pending.len() as u64);

    let mut written = 0u64;
    let supply = banana_core::curve::LaunchConfig::live_id_0().supply;

    for batch in pending.chunks(concurrency.max(1)) {
        // Real concurrency, not a sequential loop: this phase is ~20,000 requests and its
        // whole affordability rests on publicnode taking 8 at a time (35 ms/req effective
        // against 319 ms sequential). `Client` is cheap to clone -- the gate is shared --
        // so each fetch runs as its own task.
        let mut set = tokio::task::JoinSet::new();
        for pending in batch {
            let client = client.clone();
            let pending = *pending;
            set.spawn(async move {
                let tx = client
                    .get_transaction(pending.tx_hash, Priority::Bulk)
                    .await;
                (pending, tx)
            });
        }
        let mut results = Vec::with_capacity(batch.len());
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(r) => results.push(r),
                // A panicked fetch must not take the whole index with it.
                Err(e) => tracing::warn!(error = %e, "calldata fetch task failed"),
            }
        }

        let mut rows = Vec::new();
        for (pending, tx) in results {
            let tx = match tx {
                Ok(Some(tx)) => tx,
                // A launch whose transaction cannot be read still gets a row, with
                // everything Unknown. Dropping it would remove it from the universe for a
                // reason that is about our reader rather than about the token.
                _ => {
                    rows.push(unknown_enrichment(pending.token, "unreadable transaction"));
                    continue;
                }
            };
            rows.push(build_enrichment(history, pending, &tx.input, supply)?);
        }
        written += history.insert_enrichment(&rows)? as u64;
        progress.advance(Phase::Calldata, batch.len() as u64, rows.len() as u64);
    }

    progress.finish(Phase::Calldata);
    Ok(written)
}

fn unknown_enrichment(token: Address, why: &str) -> EnrichmentRow {
    EnrichmentRow {
        token,
        decoded: false,
        selector: Some(why.to_string()),
        name: None,
        symbol: None,
        description: None,
        logo: None,
        twitter_url: None,
        website_url: None,
        telegram_url: None,
        socials: banana_core::features::Socials::UNKNOWN,
        exempt_wallets: None,
        creator_fee_recipient: None,
        creator_tax_bps: None,
        declared_quote_in: None,
        dev_buy_quote: None,
        dev_buy_tokens: None,
        dev_buy_bps: None,
    }
}

fn build_enrichment(
    history: &History,
    p: PendingCalldata,
    input: &[u8],
    supply: U256,
) -> Result<EnrichmentRow> {
    use banana_chain::launch_tx::{LaunchMeta, sanitise_for_display};

    let PendingCalldata {
        token,
        tx_hash: launch_tx,
        curve,
        ordinal,
        total,
    } = p;

    // Ground truth for the dev buy, whichever route created the token: the CurveBuy the
    // launch transaction itself emitted. Independent of whether the calldata decoded.
    //
    // Restricted to the launch transaction, which is what makes this point-in-time. The
    // first buy *on the curve* is not the same thing: when the deployer launches without
    // buying, that first buy belongs to a sniper, in a later block, and using it here
    // would feed a filter a fact from after the launch (spec §5.3).
    let dev = history.dev_buy(curve, launch_tx)?;
    // No buy in the launch transaction is a real, knowable zero: the deployer launched
    // without buying. It is a different signal from `None`, which means unreadable.
    let (dev_quote, dev_tokens) = match &dev {
        Some(t) => (Some(t.amount_in), Some(t.amount_out)),
        None => (Some(U256::ZERO), Some(U256::ZERO)),
    };
    let dev_bps = dev_tokens.and_then(|tk| {
        if supply.is_zero() {
            None
        } else {
            (tk * U256::from(banana_core::BPS)).checked_div(supply)
        }
        .and_then(|v| v.try_into().ok())
    });

    // `ordinal` and `total` matter only for a bundler that launched several tokens at
    // once. The decoder refuses to guess when the frames it finds do not match the events.
    let meta = launch_tx::decode_launch_at(input, ordinal, total);
    // The transaction's OWN selector, whether or not the launch call was nested inside it.
    // It used to be hard-coded to launchAndBuy for anything that decoded, which made a
    // bundler-routed launch indistinguishable from a direct one -- and going through a
    // bundler is a fact about the launch worth keeping (spec §11's farm detection).
    let outer = input
        .get(..4)
        .map(|b| format!("0x{}", alloy_primitives::hex::encode(b)));

    Ok(match meta {
        LaunchMeta::Decoded(c) => EnrichmentRow {
            token,
            decoded: true,
            selector: outer,
            // Attacker-chosen strings: strip anything that can misrepresent itself before
            // it is stored, let alone rendered.
            name: Some(sanitise_for_display(&c.name)),
            symbol: Some(sanitise_for_display(&c.symbol)),
            description: Some(sanitise_for_display(&c.description)),
            // Stored as text, never fetched (PLAN.md C1).
            logo: Some(sanitise_for_display(&c.logo)),
            twitter_url: Some(sanitise_for_display(&c.social_urls.twitter)),
            website_url: Some(sanitise_for_display(&c.social_urls.website)),
            telegram_url: Some(sanitise_for_display(&c.social_urls.telegram)),
            socials: c.socials,
            exempt_wallets: Some(c.exempt_wallets.len() as u32),
            creator_fee_recipient: Some(c.creator_fee_recipient),
            creator_tax_bps: Some(c.creator_tax_bps),
            declared_quote_in: Some(c.declared_quote_in),
            dev_buy_quote: dev_quote,
            dev_buy_tokens: dev_tokens,
            dev_buy_bps: dev_bps,
        },
        LaunchMeta::Undecodable { selector, .. } => EnrichmentRow {
            selector: Some(format!("0x{}", alloy_primitives::hex::encode(selector))),
            dev_buy_quote: dev_quote,
            dev_buy_tokens: dev_tokens,
            dev_buy_bps: dev_bps,
            ..unknown_enrichment(token, "")
        },
    })
}

/// Read `pairTokenEconomics` for every pair token seen, once each.
///
/// The phantom reserve is per pair token and cannot be inferred from the graduation
/// threshold: the protocol derives the threshold from the phantom, not the reverse, so the
/// division is not invertible. Measured, deriving it gets 98.9% of replayed buys exact and
/// no rounding choice gets the rest -- reading it is the only way to be exact.
///
/// Cheap: one call per distinct pair token (42 in a 20,000-block window), not per launch.
pub async fn scan_pair_economics(client: &Client, history: &mut History) -> Result<u64> {
    use banana_chain::abi::IPonsFactory;
    let pending = history.pair_tokens_needing_economics()?;
    let mut n = 0;
    for pair in pending {
        match client
            .call(
                addr::PONS_FACTORY,
                &IPonsFactory::pairTokenEconomicsCall { pairToken: pair },
                Priority::Bulk,
            )
            .await
        {
            Ok(e) => {
                history.upsert_pair_economics(
                    pair,
                    e.phantomQuote,
                    e.graduationThreshold,
                    e.decimals,
                )?;
                n += 1;
            }
            // A pair whose economics cannot be read leaves its curves on the derived
            // fallback, which is right 98.9% of the time and is reported as such rather
            // than silently trusted.
            Err(err) => tracing::warn!(%pair, error = %err, "pairTokenEconomics read failed"),
        }
    }
    Ok(n)
}

// --- phase D: block timestamp anchors ----------------------------------------------------

/// Sampled block headers, for interpolating timestamps.
///
/// Block production measured extremely regular — 100.87 ms mean over 800,000 blocks — so a
/// sample every few hundred blocks holds error well under a second. That is fine for the
/// 5m/30m holds and the 6h maturity cutoff, and the entry point never uses a timestamp at
/// all (PLAN.md F2).
pub async fn scan_anchors(
    client: &Client,
    history: &mut History,
    progress: &mut Progress,
    from: u64,
    to: u64,
    every: u64,
) -> Result<u64> {
    let every = every.max(1);
    let mut blocks: Vec<u64> = (from..=to).step_by(every as usize).collect();
    // Always anchor both ends, so no indexed block falls outside the interpolable range.
    if blocks.last() != Some(&to) {
        blocks.push(to);
    }
    progress.begin(Phase::Anchors, blocks.len() as u64);

    let mut written = 0u64;
    for batch in blocks.chunks(8) {
        let mut anchors = Vec::new();
        for b in batch {
            if let Some(h) = client.get_block_header(*b, Priority::Bulk).await? {
                anchors.push((h.number, h.timestamp));
            }
        }
        written += history.insert_block_anchors(&anchors)? as u64;
        progress.advance(Phase::Anchors, batch.len() as u64, anchors.len() as u64);
        // Checkpoint like every other phase. The resume test caught this missing: anchors
        // are cheap enough to redo (41 calls for 20k blocks) that the omission was
        // invisible at that size, but a 24-hour window is ~1,700 calls and an interrupted
        // run would have redone all of them.
        if let Some(last) = batch.last() {
            history.checkpoint(
                Phase::Anchors.key(),
                PhaseState {
                    from_block: from,
                    last_block: *last,
                    target_block: to,
                    rows_written: written,
                },
            )?;
        }
    }

    progress.finish(Phase::Anchors);
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::hex;

    /// A real `TokenLaunched` log, from the launch used as a fixture throughout.
    #[test]
    fn a_real_launch_log_decodes() {
        let log = RawLog {
            address: addr::PONS_FACTORY,
            topics: vec![
                IPonsFactory::TokenLaunched::SIGNATURE_HASH,
                "0x0000000000000000000000008067b3b37a1e360ace63604f37cc6ca6a6734d4e"
                    .parse()
                    .unwrap(),
                "0x000000000000000000000000912f61576f387e8fc5cd81434fd047c35cd95552"
                    .parse()
                    .unwrap(),
                "0x000000000000000000000000192de377d718c13d3bb4e48dd2a7675b66521a47"
                    .parse()
                    .unwrap(),
            ],
            data: hex::decode(concat!(
                "0000000000000000000000000000000000000000000000000000000000000000",
                "0000000000000000000000000000000000000000000000000000000000000000",
                "0000000000000000000000000000000000000000000000003a4965bf58a40000"
            ))
            .unwrap()
            .into(),
            block_number: 56_700_694,
            tx_hash: B256::repeat_byte(1),
            tx_index: 0,
            log_index: 3,
        };

        let row = decode_launch_log(&log).expect("must decode");
        assert_eq!(
            row.token.to_string().to_lowercase(),
            "0x8067b3b37a1e360ace63604f37cc6ca6a6734d4e"
        );
        assert_eq!(
            row.curve.to_string().to_lowercase(),
            "0x912f61576f387e8fc5cd81434fd047c35cd95552"
        );
        assert!(row.pair_token.is_zero(), "native ETH pair");
        assert_eq!(row.launch_config_id, 0);
        assert_eq!(
            row.graduation_threshold,
            U256::from(4_200_000_000_000_000_000u64),
            "4.2 ETH, as doctor confirmed"
        );
    }

    /// A real `CurveBuy`, checked against the values read off the chain.
    #[test]
    fn a_real_curve_buy_log_decodes() {
        let mut data = Vec::new();
        for v in [
            U256::from(88_421_000_000_000_000u64),
            U256::from_str_radix("49524734362106262014495324", 10).unwrap(),
            U256::from(884_210_000_000_000u64),
            U256::ZERO,
        ] {
            data.extend_from_slice(&v.to_be_bytes::<32>());
        }
        let log = RawLog {
            address: "0x912f61576f387e8fc5cd81434fd047c35cd95552"
                .parse()
                .unwrap(),
            topics: vec![
                IPonsCurve::CurveBuy::SIGNATURE_HASH,
                B256::left_padding_from(
                    &hex::decode("192de377d718c13d3bb4e48dd2a7675b66521a47").unwrap(),
                ),
                B256::left_padding_from(
                    &hex::decode("192de377d718c13d3bb4e48dd2a7675b66521a47").unwrap(),
                ),
            ],
            data: data.into(),
            block_number: 56_700_694,
            tx_hash: B256::repeat_byte(2),
            tx_index: 0,
            log_index: 4,
        };

        let t = decode_trade_log(&log, true).expect("must decode");
        assert_eq!(t.side, Side::Buy);
        assert_eq!(t.amount_in, U256::from(88_421_000_000_000_000u64));
        assert_eq!(
            t.amount_out,
            U256::from_str_radix("49524734362106262014495324", 10).unwrap()
        );
        assert_eq!(t.fee, U256::from(884_210_000_000_000u64));
        assert_eq!(t.tax, U256::ZERO);
        assert_eq!(t.snipe_tax, None, "attached separately from its own event");
    }

    #[test]
    fn a_truncated_log_is_skipped_rather_than_panicking() {
        let log = RawLog {
            address: addr::PONS_FACTORY,
            topics: vec![IPonsFactory::TokenLaunched::SIGNATURE_HASH],
            data: vec![0u8; 16].into(),
            block_number: 1,
            tx_hash: B256::ZERO,
            tx_index: 0,
            log_index: 0,
        };
        assert!(decode_launch_log(&log).is_none());
        assert!(decode_trade_log(&log, true).is_none());
    }

    #[test]
    fn an_indexed_address_topic_takes_the_low_twenty_bytes() {
        let log = RawLog {
            address: Address::ZERO,
            topics: vec![
                B256::ZERO,
                "0x000000000000000000000000192de377d718c13d3bb4e48dd2a7675b66521a47"
                    .parse()
                    .unwrap(),
            ],
            data: Vec::new().into(),
            block_number: 1,
            tx_hash: B256::ZERO,
            tx_index: 0,
            log_index: 0,
        };
        assert_eq!(
            topic_address(&log, 1).unwrap().to_string().to_lowercase(),
            "0x192de377d718c13d3bb4e48dd2a7675b66521a47"
        );
        assert_eq!(
            topic_address(&log, 5),
            None,
            "a missing topic is not a zero address"
        );
    }
}
