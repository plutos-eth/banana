//! The precomputed fate of each token, from logs alone.
//!
//! Spec §5.2: `CurveBuy` and `CurveSell` carry the price of every trade directly in the
//! event payload, so the entire price path of every token is reconstructible without an
//! archive node, a historical state read, or an `eth_call` at a past block.
//!
//! # Where the entry comes from
//!
//! §5.2 defines entry as "the first trade after `launch_ts + tax_window_seconds`". The
//! endpoint does not give timestamps with logs (`blockTimestamp` is present but always
//! `0x0`), so that definition needs data the chain will not hand over cheaply. There is a
//! better signal: the opening tax is charged through a separate `SnipeTaxCharged` event,
//! so a buy that carries **no** snipe tax is a buy that happened after the window closed.
//! That is exact, needs no clock, and is closer to what the sniper actually does — it
//! polls `currentSnipeTaxBps` and fires (PLAN.md F2).
//!
//! # Tokens with no untaxed buy at all (PLAN.md F9)
//!
//! Some curves never see an untaxed buy: every buy landed inside the window, or there were
//! no post-launch buys at all. Under the rule above those tokens would have no entry and
//! would drop silently out of the universe — **and that is the worst possible bias**,
//! because the live sniper *would* have entered them. It waits for the decay and fires, and
//! its own buy would have been that first untaxed trade. Dropping them removes exactly the
//! outcomes that went nowhere, which is the survivorship bias §5.5 exists to prevent.
//!
//! So they are reconstructed instead: the curve is replayed to the end of the tax window
//! and a reference-size buy is quoted against it, which is precisely the fill the sniper
//! would have received. Every row records which rule produced it, and the split is surfaced
//! as its own funnel stage rather than averaged in, because it changes what `entry_price`
//! *means* for that row.

use std::collections::HashSet;

use alloy_primitives::{Address, B256, U256};
use quarrel_core::curve::{CurveState, LaunchConfig, quote_buy};
use quarrel_store::history::TradeRow;
use quarrel_store::types::{EntryRule, Side, multiple_bps, price_of};

/// How long the opening tax lasts, in blocks.
///
/// `snipeTaxSeconds` reads 3 from the live factory and block time measured 100.87 ms, so
/// the window is ~30 blocks. Used **only** to bound the reconstruction replay; the
/// observed-entry path needs no such approximation because it reads the tax event itself.
pub const TAX_WINDOW_BLOCKS: u64 = 30;

/// Everything needed to decide one token's fate.
///
/// No `Debug`: `ts_at` is a closure, and deriving it here would force every caller to
/// supply a nameable function type for what is naturally a lookup into the anchor table.
pub struct OutcomeInput<'a> {
    pub launch_block: u64,
    pub launch_tx: B256,
    pub config: &'a LaunchConfig,
    /// Every trade on this curve, in chain order.
    pub trades: &'a [TradeRow],
    /// Wallets the launch declared exempt from the opening tax.
    ///
    /// An exempt wallet pays no snipe tax, so its buy emits no `SnipeTaxCharged` and would
    /// otherwise look exactly like a post-window entry.
    pub exempt: &'a HashSet<Address>,
    pub migrated: bool,
    /// Size the reconstruction quotes, when it is needed.
    pub reference_size: U256,
    /// Block timestamps, for the fixed-hold multiples. Approximate, and never used for the
    /// entry itself.
    pub ts_at: &'a dyn Fn(u64) -> Option<u64>,
    /// Highest block covered by the index, for maturity accounting.
    pub head_block: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub entry_rule: EntryRule,
    pub entry_block: Option<u64>,
    pub entry_price: Option<U256>,
    pub entry_tokens: Option<U256>,

    pub ath_price: Option<U256>,
    pub ath_block: Option<u64>,
    pub max_multiple_bps: Option<u64>,
    pub time_to_ath_s: Option<u64>,

    pub mult_after_5m_bps: Option<u64>,
    pub mult_after_30m_bps: Option<u64>,

    pub migrated: bool,
    pub died: bool,

    // Post-entry facts. Display only; a different Rust type from the filterable features,
    // so a rule cannot read them (spec §5.2, §5.3).
    pub distinct_buyers_1m: Option<u32>,
    pub every_early_buy_taxed: Option<bool>,

    pub post_entry_trades: u64,
    pub last_trade_block: Option<u64>,
    pub observed_blocks: u64,
}

/// Quote per 10^18 tokens, whichever side the trade is.
pub fn trade_price(t: &TradeRow) -> Option<U256> {
    match t.side {
        Side::Buy => price_of(t.amount_in, t.amount_out),
        Side::Sell => price_of(t.amount_out, t.amount_in),
    }
}

/// Whether a buy is a candidate entry: not in the launch transaction, not snipe-taxed, and
/// not by a wallet the launch exempted.
fn is_entry_candidate(t: &TradeRow, launch_tx: B256, exempt: &HashSet<Address>) -> bool {
    t.side == Side::Buy
        // The dev buy rides in the launch transaction itself and is not an entry.
        && t.tx_hash != launch_tx
        // A snipe tax means the window was still open.
        && t.snipe_tax.is_none()
        // An exempt wallet pays no snipe tax even at full draw, so its buy carries no
        // SnipeTaxCharged and would masquerade as a post-window fill.
        && !exempt.contains(&t.actor)
        && !exempt.contains(&t.recipient)
}

pub fn compute(input: &OutcomeInput<'_>) -> Outcome {
    let entry = find_entry(input);

    let Some((rule, entry_block, entry_price, entry_tokens)) = entry else {
        // No entry could be established at all, even by reconstruction. This happens only
        // when the curve config is unusable; the row still exists, with nothing claimed.
        return Outcome {
            entry_rule: EntryRule::ReconstructedAtWindowEnd,
            entry_block: None,
            entry_price: None,
            entry_tokens: None,
            ath_price: None,
            ath_block: None,
            max_multiple_bps: None,
            time_to_ath_s: None,
            mult_after_5m_bps: None,
            mult_after_30m_bps: None,
            migrated: input.migrated,
            died: false,
            distinct_buyers_1m: None,
            every_early_buy_taxed: None,
            post_entry_trades: 0,
            last_trade_block: input.trades.last().map(|t| t.block),
            observed_blocks: 0,
        };
    };

    let after: Vec<&TradeRow> = input
        .trades
        .iter()
        .filter(|t| t.block >= entry_block && t.tx_hash != input.launch_tx)
        .collect();

    // --- peak -------------------------------------------------------------------------
    //
    // This is `max_multiple`, and it is the best multiple that EXISTED, not one anybody
    // captured. Everything downstream must label it as a peak (spec §5.4).
    let mut ath_price = entry_price;
    let mut ath_block = entry_block;
    for t in &after {
        if let Some(p) = trade_price(t)
            && p > ath_price
        {
            ath_price = p;
            ath_block = t.block;
        }
    }

    let entry_ts = (input.ts_at)(entry_block);
    let time_to_ath_s = match (entry_ts, (input.ts_at)(ath_block)) {
        (Some(a), Some(b)) => Some(b.saturating_sub(a)),
        _ => None,
    };

    // --- fixed holds ------------------------------------------------------------------
    //
    // The headline number (spec §5.4): what a simple executable rule would have produced.
    let mult_after_5m_bps = hold_multiple(&after, entry_price, entry_ts, 5 * 60, input.ts_at);
    let mult_after_30m_bps = hold_multiple(&after, entry_price, entry_ts, 30 * 60, input.ts_at);

    // --- post-entry facts, display only -----------------------------------------------
    let one_minute_end = entry_ts.map(|t| t + 60);
    let mut buyers_1m: HashSet<Address> = HashSet::new();
    let mut early_buys = 0u32;
    let mut early_taxed = 0u32;
    for t in &after {
        if t.side != Side::Buy {
            continue;
        }
        let within = match (one_minute_end, (input.ts_at)(t.block)) {
            (Some(end), Some(ts)) => ts <= end,
            _ => false,
        };
        if within {
            buyers_1m.insert(t.recipient);
        }
        if t.block <= entry_block + TAX_WINDOW_BLOCKS {
            early_buys += 1;
            if t.snipe_tax.is_some() {
                early_taxed += 1;
            }
        }
    }

    let last_trade_block = input.trades.last().map(|t| t.block);
    let observed_blocks = input.head_block.saturating_sub(entry_block);

    // `died` is a documented, reproducible function of stored counts rather than a
    // judgement (PLAN.md C4). Its thresholds are provisional and get re-derived from the
    // real distribution once a full window is indexed.
    let quiet = last_trade_block
        .map(|b| input.head_block.saturating_sub(b) > QUIET_BLOCKS)
        .unwrap_or(true);
    let collapsed = multiple_bps(
        after
            .last()
            .and_then(|t| trade_price(t))
            .unwrap_or(entry_price),
        entry_price,
    )
    .is_some_and(|m| m < DEAD_MULTIPLE_BPS);
    let died = quiet && (collapsed || after.is_empty());

    Outcome {
        entry_rule: rule,
        entry_block: Some(entry_block),
        entry_price: Some(entry_price),
        entry_tokens: Some(entry_tokens),
        ath_price: Some(ath_price),
        ath_block: Some(ath_block),
        max_multiple_bps: multiple_bps(ath_price, entry_price),
        time_to_ath_s,
        mult_after_5m_bps,
        mult_after_30m_bps,
        migrated: input.migrated,
        died,
        distinct_buyers_1m: Some(buyers_1m.len() as u32),
        every_early_buy_taxed: (early_buys > 0).then_some(early_buys == early_taxed),
        post_entry_trades: after.len() as u64,
        last_trade_block,
        observed_blocks,
    }
}

/// No trade for this many blocks (~60 minutes at ~100 ms) counts as silent.
const QUIET_BLOCKS: u64 = 36_000;
/// Below 10% of entry counts as collapsed.
const DEAD_MULTIPLE_BPS: u64 = 1_000;

/// The multiple at a fixed hold, using the last trade at or before the horizon.
fn hold_multiple(
    after: &[&TradeRow],
    entry_price: U256,
    entry_ts: Option<u64>,
    hold_secs: u64,
    ts_at: &dyn Fn(u64) -> Option<u64>,
) -> Option<u64> {
    let entry_ts = entry_ts?;
    let horizon = entry_ts + hold_secs;
    let mut price = entry_price;
    let mut saw_horizon = false;
    for t in after {
        let Some(ts) = ts_at(t.block) else { continue };
        if ts > horizon {
            saw_horizon = true;
            break;
        }
        if let Some(p) = trade_price(t) {
            price = p;
        }
        saw_horizon = true;
    }
    // Without a single timestamped trade there is nothing to mark against.
    if !saw_horizon && after.is_empty() {
        // Nothing traded after entry: the position is worth what the curve pays back,
        // which is the round trip, not zero (spec §1). Held at entry price here; the
        // round-trip cost is applied by the caller's sell quote.
        return multiple_bps(entry_price, entry_price);
    }
    multiple_bps(price, entry_price)
}

/// Find the entry, observed if one exists and reconstructed otherwise.
fn find_entry(input: &OutcomeInput<'_>) -> Option<(EntryRule, u64, U256, U256)> {
    if let Some(t) = input
        .trades
        .iter()
        .find(|t| is_entry_candidate(t, input.launch_tx, input.exempt))
    {
        let price = trade_price(t)?;
        return Some((EntryRule::ObservedUntaxedBuy, t.block, price, t.amount_out));
    }

    // Nothing untaxed ever traded. Replay to the end of the window and quote.
    let entry_block = input.launch_block + TAX_WINDOW_BLOCKS;
    let mut state = CurveState::opening(input.config).ok()?;
    for t in input.trades {
        if t.block > entry_block {
            break;
        }
        apply(&mut state, t).ok()?;
    }
    // The window has closed, so the opening tax is zero by definition.
    state.opening_tax_bps = 0;
    let q = quote_buy(&state, input.reference_size).ok()?;
    let price = price_of(q.spent, q.tokens_out)?;
    Some((
        EntryRule::ReconstructedAtWindowEnd,
        entry_block,
        price,
        q.tokens_out,
    ))
}

/// Advance a replayed curve by one observed trade.
pub fn apply(state: &mut CurveState, t: &TradeRow) -> Result<(), quarrel_core::curve::CurveError> {
    match t.side {
        Side::Buy => state.apply_buy(
            t.amount_in,
            t.amount_out,
            t.fee,
            t.tax,
            t.snipe_tax.unwrap_or(U256::ZERO),
        ),
        Side::Sell => state.apply_sell(t.amount_in, t.amount_out, t.fee, t.tax),
    }
}

/// How well a replay reproduces reality.
///
/// The reconstruction of F9 is only trustworthy if the replay is exact, so this checks it
/// against the chain rather than assuming it: for every buy, the tokens the curve would
/// have produced from the replayed state must equal the `tokensOut` the event actually
/// recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReplayCheck {
    pub checked: u64,
    pub exact: u64,
    pub mismatched: u64,
    pub errored: u64,
}

impl ReplayCheck {
    pub fn is_exact(&self) -> bool {
        self.mismatched == 0 && self.errored == 0
    }
}

/// Replay a curve's whole trade history, verifying each buy against the event.
pub fn verify_replay(config: &LaunchConfig, trades: &[TradeRow]) -> ReplayCheck {
    let mut check = ReplayCheck::default();
    let Ok(mut state) = CurveState::opening(config) else {
        check.errored += 1;
        return check;
    };

    for t in trades {
        if t.side == Side::Buy {
            // Reproduce the fee split the event recorded, so the comparison isolates the
            // curve arithmetic rather than the tax parameters.
            let mut s = state;
            s.opening_tax_bps = 0;
            s.creator_tax_bps = 0;
            s.fee_bps = 0;
            let deducted = t.fee + t.tax + t.snipe_tax.unwrap_or(U256::ZERO);
            if let Some(net) = t.amount_in.checked_sub(deducted) {
                match quote_buy(&s, net) {
                    Ok(q) => {
                        check.checked += 1;
                        if q.tokens_out == t.amount_out {
                            check.exact += 1;
                        } else {
                            check.mismatched += 1;
                        }
                    }
                    Err(_) => check.errored += 1,
                }
            }
        }
        if apply(&mut state, t).is_err() {
            check.errored += 1;
            break;
        }
    }
    check
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(n: u8) -> Address {
        Address::repeat_byte(n)
    }

    fn cfg() -> LaunchConfig {
        LaunchConfig::live_id_0()
    }

    fn buy(block: u64, tx: u8, quote: u64, tokens: u128, snipe: Option<u64>) -> TradeRow {
        TradeRow {
            tx_hash: B256::repeat_byte(tx),
            log_index: 0,
            curve: addr(9),
            block,
            tx_index: 0,
            side: Side::Buy,
            actor: addr(tx),
            recipient: addr(tx),
            amount_in: U256::from(quote),
            amount_out: U256::from(tokens),
            fee: U256::from(quote / 100),
            tax: U256::ZERO,
            snipe_tax: snipe.map(U256::from),
        }
    }

    fn input<'a>(
        trades: &'a [TradeRow],
        exempt: &'a HashSet<Address>,
        ts: &'a dyn Fn(u64) -> Option<u64>,
        cfg: &'a LaunchConfig,
    ) -> OutcomeInput<'a> {
        OutcomeInput {
            launch_block: 100,
            launch_tx: B256::repeat_byte(0xFF),
            config: cfg,
            trades,
            exempt,
            migrated: false,
            reference_size: U256::from(10_000_000_000_000_000u64), // 0.01 ETH
            ts_at: ts,
            head_block: 200_000,
        }
    }

    /// ~100 ms blocks, as measured.
    fn ts(block: u64) -> Option<u64> {
        Some(1_000_000 + block / 10)
    }

    #[test]
    fn the_entry_is_the_first_buy_that_paid_no_snipe_tax() {
        let c = cfg();
        let ex = HashSet::new();
        let trades = vec![
            buy(101, 1, 1_000, 500, Some(900)), // inside the window
            buy(105, 2, 1_000, 400, Some(500)), // still inside
            buy(140, 3, 1_000, 300, None),      // the window has closed
            buy(200, 4, 1_000, 200, None),
        ];
        let o = compute(&input(&trades, &ex, &ts, &c));
        assert_eq!(o.entry_rule, EntryRule::ObservedUntaxedBuy);
        assert_eq!(o.entry_block, Some(140));
        assert_eq!(o.entry_tokens, Some(U256::from(300u64)));
    }

    #[test]
    fn the_dev_buy_in_the_launch_transaction_is_never_the_entry() {
        let c = cfg();
        let ex = HashSet::new();
        let mut dev = buy(100, 0xFF, 1_000, 5_000, None);
        dev.tx_hash = B256::repeat_byte(0xFF); // the launch tx
        let trades = vec![dev, buy(150, 3, 1_000, 300, None)];
        let o = compute(&input(&trades, &ex, &ts, &c));
        assert_eq!(o.entry_block, Some(150), "the dev buy is not an entry");
    }

    /// An exempt wallet pays no snipe tax even at full draw, so without this check its buy
    /// would masquerade as the first post-window fill and set an entry price from inside
    /// the window.
    #[test]
    fn an_exempt_wallets_buy_is_not_mistaken_for_a_post_window_entry() {
        let c = cfg();
        let mut ex = HashSet::new();
        ex.insert(addr(2));

        let trades = vec![
            buy(101, 2, 1_000, 900, None), // exempt, inside the window, untaxed
            buy(150, 3, 1_000, 300, None), // the real entry
        ];
        let o = compute(&input(&trades, &ex, &ts, &c));
        assert_eq!(o.entry_block, Some(150));
        assert_eq!(o.entry_rule, EntryRule::ObservedUntaxedBuy);
    }

    // --- F9: the tokens that must not vanish -------------------------------------------

    #[test]
    fn a_curve_with_only_taxed_buys_is_reconstructed_not_dropped() {
        // Every buy landed inside the window and then the token went silent. The sniper
        // would have entered this; dropping it removes a real bad outcome from the
        // denominator.
        let c = cfg();
        let ex = HashSet::new();
        let trades = vec![
            buy(101, 1, 1_000, 500, Some(900)),
            buy(102, 2, 1_000, 400, Some(800)),
        ];
        let o = compute(&input(&trades, &ex, &ts, &c));
        assert_eq!(o.entry_rule, EntryRule::ReconstructedAtWindowEnd);
        assert_eq!(o.entry_block, Some(130), "launch block + the tax window");
        assert!(
            o.entry_price.is_some(),
            "a reconstructed entry still has a price"
        );
        assert!(o.entry_tokens.unwrap() > U256::ZERO);
    }

    #[test]
    fn a_curve_with_no_post_launch_trades_at_all_is_still_reconstructed() {
        // Probably the larger of the two cases: only the dev buy, then nothing.
        let c = cfg();
        let ex = HashSet::new();
        let o = compute(&input(&[], &ex, &ts, &c));
        assert_eq!(o.entry_rule, EntryRule::ReconstructedAtWindowEnd);
        assert!(o.entry_price.is_some());
        assert_eq!(o.post_entry_trades, 0);
    }

    #[test]
    fn every_token_gets_an_entry_rule_so_none_can_silently_disappear() {
        // The guard against the bias returning: whatever the trade history looks like,
        // there is always exactly one rule.
        let c = cfg();
        let ex = HashSet::new();
        let cases: Vec<Vec<TradeRow>> = vec![
            vec![],
            vec![buy(101, 1, 1_000, 500, Some(900))],
            vec![buy(150, 1, 1_000, 500, None)],
            vec![
                buy(101, 1, 1_000, 500, Some(900)),
                buy(150, 2, 1_000, 400, None),
            ],
        ];
        for trades in cases {
            let o = compute(&input(&trades, &ex, &ts, &c));
            assert!(
                o.entry_price.is_some(),
                "no launch may end up with no entry: {trades:?}"
            );
        }
    }

    #[test]
    fn a_reconstructed_entry_prices_against_the_moved_curve_not_the_fresh_one() {
        // The taxed buys inside the window really did move the curve, so a reconstruction
        // that ignored them would quote too cheaply and overstate every later multiple.
        let c = cfg();
        let ex = HashSet::new();

        let empty = compute(&input(&[], &ex, &ts, &c));
        let big = U256::from(500_000_000_000_000_000u64); // 0.5 ETH inside the window
        let moved = vec![TradeRow {
            amount_in: big,
            amount_out: U256::from(20_000_000_000_000_000_000_000_000u128),
            fee: big / U256::from(100u64),
            snipe_tax: Some(U256::from(1u64)),
            ..buy(101, 1, 0, 0, Some(1))
        }];
        let after = compute(&input(&moved, &ex, &ts, &c));

        assert!(
            after.entry_price.unwrap() > empty.entry_price.unwrap(),
            "a curve that was bought into must price higher"
        );
    }

    // --- outcomes ----------------------------------------------------------------------

    #[test]
    fn the_peak_is_the_highest_price_after_entry() {
        let c = cfg();
        let ex = HashSet::new();
        let trades = vec![
            buy(150, 1, 1_000, 1_000, None), // entry: price 1
            buy(160, 2, 1_000, 250, None),   // price 4
            buy(170, 3, 1_000, 500, None),   // price 2
        ];
        let o = compute(&input(&trades, &ex, &ts, &c));
        assert_eq!(o.ath_block, Some(160));
        assert_eq!(o.max_multiple_bps, Some(40_000), "4x peak");
    }

    #[test]
    fn a_token_that_only_fell_has_a_peak_of_exactly_one_x() {
        // Never below 1x: the entry price itself is always achievable at entry.
        let c = cfg();
        let ex = HashSet::new();
        let trades = vec![
            buy(150, 1, 1_000, 1_000, None),
            buy(160, 2, 1_000, 10_000, None), // price 0.1
        ];
        let o = compute(&input(&trades, &ex, &ts, &c));
        assert_eq!(o.max_multiple_bps, Some(10_000), "1x, not below");
    }

    #[test]
    fn the_fixed_hold_multiple_uses_the_last_trade_before_the_horizon() {
        let c = cfg();
        let ex = HashSet::new();
        // 100 ms blocks: 5 minutes is 3,000 blocks.
        let trades = vec![
            buy(150, 1, 1_000, 1_000, None),  // entry, price 1
            buy(1_000, 2, 1_000, 500, None),  // +85s,  price 2
            buy(2_000, 3, 1_000, 250, None),  // +185s, price 4
            buy(50_000, 4, 1_000, 100, None), // way past 5m, price 10
        ];
        let o = compute(&input(&trades, &ex, &ts, &c));
        assert_eq!(
            o.mult_after_5m_bps,
            Some(40_000),
            "4x at 5 minutes, not the later 10x"
        );
    }

    #[test]
    fn a_silent_token_marks_at_its_entry_price_not_at_zero() {
        // Spec §1: a position in an untraded curve is worth what the curve pays back. The
        // round-trip cost is a sell quote, not a write-down to nothing.
        let c = cfg();
        let ex = HashSet::new();
        let trades = vec![buy(150, 1, 1_000, 1_000, None)];
        let o = compute(&input(&trades, &ex, &ts, &c));
        assert_eq!(o.mult_after_5m_bps, Some(10_000));
        assert_eq!(o.mult_after_30m_bps, Some(10_000));
    }

    #[test]
    fn died_is_a_reproducible_function_of_stored_counts() {
        let c = cfg();
        let ex = HashSet::new();
        // Entered, collapsed to 1% of entry, then silent for far longer than the window.
        let trades = vec![
            buy(150, 1, 1_000, 1_000, None),
            buy(160, 2, 1_000, 100_000, None),
        ];
        let o = compute(&input(&trades, &ex, &ts, &c));
        assert!(o.died, "quiet and collapsed");

        // Still trading right up to the head: not dead.
        let live = vec![
            buy(150, 1, 1_000, 1_000, None),
            buy(199_999, 2, 1_000, 100_000, None),
        ];
        let o = compute(&input(&live, &ex, &ts, &c));
        assert!(!o.died, "recent activity means it is not silent");
    }

    #[test]
    fn post_entry_facts_are_recorded_but_kept_out_of_the_filterable_set() {
        let c = cfg();
        let ex = HashSet::new();
        let trades = vec![
            buy(150, 1, 1_000, 1_000, None),
            buy(155, 2, 1_000, 900, None),
            buy(160, 3, 1_000, 800, None),
        ];
        let o = compute(&input(&trades, &ex, &ts, &c));
        assert_eq!(o.distinct_buyers_1m, Some(3));
        assert_eq!(
            o.every_early_buy_taxed,
            Some(false),
            "none of these paid the opening tax"
        );
    }

    // --- replay verification -----------------------------------------------------------

    #[test]
    fn the_replay_reproduces_a_real_dev_buy_exactly() {
        // The launch probed on 2026-09-07: this is the property the F9 reconstruction
        // rests on, so it is checked against a real event rather than assumed.
        let c = cfg();
        let real = TradeRow {
            tx_hash: B256::repeat_byte(1),
            log_index: 0,
            curve: addr(9),
            block: 100,
            tx_index: 0,
            side: Side::Buy,
            actor: addr(1),
            recipient: addr(1),
            amount_in: U256::from(88_421_000_000_000_000u64),
            amount_out: U256::from_str_radix("49524734362106262014495324", 10).unwrap(),
            fee: U256::from(884_210_000_000_000u64),
            tax: U256::ZERO,
            snipe_tax: None,
        };
        let check = verify_replay(&c, &[real]);
        assert_eq!(check.checked, 1);
        assert_eq!(check.exact, 1, "must reproduce the event exactly");
        assert!(check.is_exact());
    }

    #[test]
    fn the_replay_reports_a_mismatch_rather_than_hiding_it() {
        let c = cfg();
        let mut wrong = buy(100, 1, 88_421_000_000_000_000, 1, None);
        wrong.fee = U256::from(884_210_000_000_000u64);
        let check = verify_replay(&c, &[wrong]);
        assert_eq!(check.mismatched, 1);
        assert!(
            !check.is_exact(),
            "a silent mismatch would make F9 untrustworthy"
        );
    }

    #[test]
    fn trade_price_normalises_both_sides_to_quote_per_token() {
        let b = buy(1, 1, 1_000, 500, None);
        let mut s = b.clone();
        s.side = Side::Sell;
        s.amount_in = U256::from(500u64); // tokens in
        s.amount_out = U256::from(1_000u64); // quote out
        assert_eq!(
            trade_price(&b),
            trade_price(&s),
            "the same price whichever way the trade went"
        );
    }
}
