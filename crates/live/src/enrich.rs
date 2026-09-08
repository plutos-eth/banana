//! Enrich — turning a launch event into the features a rule can read.
//!
//! The Lab reads `PitFeatures` from the store, where four indexing phases put them. The
//! sniper has to build the same thing from the chain, in under a second, for a token that
//! is thirty seconds old and has no row anywhere.
//!
//! # Same features or nothing
//!
//! A backtest is a promise about what the sniper will do. If the live path computes a
//! feature differently from the indexer — even slightly, even in one field — the promise
//! is void and the product lies without ever printing a wrong number. So every field here
//! comes from the same source the indexer uses:
//!
//! | field | source | why not the obvious alternative |
//! |---|---|---|
//! | socials, name, description | launch calldata | `getTokenInfo()` is current state: a link added an hour later would read as declared at launch |
//! | `creator_tax_bps` | launch calldata | `getLaunchedToken().creatorTaxBps` is current and can be updated after launch |
//! | `exempt_wallets` | launch calldata | nowhere else |
//! | `dev_buy_bps` | the launch transaction's own `CurveBuy` | the first buy *on the curve* belongs to a sniper when the deployer did not buy — a real bug, found in phase 4 |
//! | deployer history | [`crate::seen`] | the store plus what this session watched, with the seam between them accounted for |
//!
//! # Three requests, in parallel
//!
//! `eth_getTransactionByHash` for the calldata, `eth_getTransactionReceipt` for the dev
//! buy, and one Multicall3 `aggregate3` for everything readable from contracts. All three
//! go at once and all three are `Priority::Hot`; the multicall is `allowFailure` throughout
//! so one reverting getter cannot take the rest with it.
//!
//! # Nothing missing is silently zero
//!
//! Every read that fails leaves its field `None` or `Unknown` and adds a line to
//! [`Reading::gaps`]. A `require_*` rule refuses an `Unknown`, which is the honest
//! direction: "we could not read this" must never become "this token does not have one".

use std::time::Duration;

use alloy_primitives::{Address, U256};
use alloy_sol_types::{SolCall, SolEvent};
use banana_chain::abi::{IPonsCurve, IPonsFactory, IPonsToken, Phase};
use banana_chain::addr;
use banana_chain::gate::{Priority, RpcError};
use banana_chain::launch_tx::{self, LaunchMeta, sanitise_for_display};
use banana_chain::rpc::{CallResult, Client, Receipt};
use banana_core::features::{FeeRecipient, Fingerprint, Pair, PitFeatures, Socials};
use banana_core::{BPS, Bps};
use banana_store::DeployerSeen;

use crate::watch::Launch;

/// How long to wait before asking a second time for a receipt that was not there yet.
///
/// One block plus a margin. A launch detected 300 ms after its block will normally have a
/// receipt already; this covers the case where detection beat the node's own indexing.
const RECEIPT_RETRY: Duration = Duration::from_millis(200);

/// Live contract state for one token.
///
/// **None of this is a filter input.** It is current state by definition — the phase, the
/// reserves, the tax right now — and spec §5.2 keeps current state out of
/// [`PitFeatures`]. It is here because the executor needs it to price and route an order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainState {
    /// `None` when `getLaunchedToken` did not decode or returned a phase this build does
    /// not know. Routing refuses a `None` rather than guessing (spec §7).
    pub phase: Option<Phase>,
    pub quote_reserve: U256,
    pub token_reserve: U256,
    /// Total supply, from the launch config where readable.
    pub supply: U256,
    pub curve_fee_bps: Bps,
    /// The creator tax **as it stands now**, which is what a buy will actually pay. The
    /// point-in-time one, for the filter, comes from the calldata.
    pub live_creator_tax_bps: Bps,
    /// `currentSnipeTaxBps` for a throwaway recipient: the opening tax as it stands.
    pub snipe_tax_bps: Bps,
    pub graduation_threshold: U256,
    pub tick_spacing: i32,
    pub pool_fee: u32,
    pub is_native_quote: bool,
    /// The non-phantom part of the quote reserve: what has actually been paid in.
    pub real_quote_reserve: U256,
    pub sellable_tokens: U256,
    pub reserved_tokens: U256,
    pub graduated: bool,
    pub ready_to_graduate: bool,
    /// False when the reads that price the curve did not come back. Nothing is quoted
    /// against a default-constructed state: an all-zero curve prices every order at zero.
    pub priced: bool,
}

impl ChainState {
    /// The pricing state, in the form `banana_core::curve` takes.
    ///
    /// The same struct the backtest replays with, so a live quote and a replayed one go
    /// through identical arithmetic.
    pub fn curve_state(&self) -> banana_core::curve::CurveState {
        banana_core::curve::CurveState {
            quote_reserve: self.quote_reserve,
            token_reserve: self.token_reserve,
            real_quote_reserve: self.real_quote_reserve,
            sellable_tokens: self.sellable_tokens,
            reserved_tokens: self.reserved_tokens,
            graduation_threshold: self.graduation_threshold,
            fee_bps: self.curve_fee_bps,
            creator_tax_bps: self.live_creator_tax_bps,
            opening_tax_bps: self.snipe_tax_bps,
            graduated: self.graduated,
            ready_to_graduate: self.ready_to_graduate,
        }
    }

    /// Where an order for this token may go, or why it may not go anywhere.
    ///
    /// Refuses an unread phase rather than guessing at one (spec section 7).
    pub fn route(&self, token: Address, curve: Address, pair_token: Address) -> crate::Route {
        let key = crate::PoolKey::pons(token, pair_token, self.tick_spacing, addr::PONS_HOOK);
        match self.phase {
            Some(p) => crate::Route::of(p, curve, key),
            None => crate::Route::Refuse {
                reason: "the factory did not report a phase for this token, so there is no \
                         venue this build is willing to route to"
                    .into(),
            },
        }
    }
}

impl Default for ChainState {
    fn default() -> Self {
        Self {
            phase: None,
            quote_reserve: U256::ZERO,
            token_reserve: U256::ZERO,
            supply: U256::ZERO,
            curve_fee_bps: 0,
            live_creator_tax_bps: 0,
            snipe_tax_bps: 0,
            graduation_threshold: U256::ZERO,
            tick_spacing: 0,
            pool_fee: 0,
            is_native_quote: true,
            real_quote_reserve: U256::ZERO,
            sellable_tokens: U256::ZERO,
            reserved_tokens: U256::ZERO,
            graduated: false,
            ready_to_graduate: false,
            priced: false,
        }
    }
}

/// Everything read from the chain about one launch, before session history is folded in.
#[derive(Debug, Clone)]
pub struct Reading {
    pub launch: Launch,
    pub name: String,
    pub symbol: String,
    pub description: String,
    pub socials: Socials,
    pub exempt_wallets: Option<u32>,
    /// From the calldata: point-in-time, unlike the live one in [`ChainState`].
    pub creator_tax_bps: Option<Bps>,
    pub fee_recipient: FeeRecipient,
    /// The deployer's own buy in the launch transaction, as bps of supply.
    ///
    /// `Some(0)` is a real answer — launched without buying — and a different signal from
    /// `None`, which means the transaction or the supply could not be read.
    pub dev_buy_bps: Option<Bps>,
    pub dev_buy_quote: Option<U256>,
    pub pair: Pair,
    pub state: ChainState,
    /// Whether the launch transaction decoded at all.
    pub decoded: bool,
    /// Everything that could not be read, in the user's words.
    pub gaps: Vec<String>,
    pub elapsed_ms: u64,
}

impl Reading {
    /// The launch-farm signature, from calldata only.
    ///
    /// Built with `banana_core`'s constructor, the same one the indexer uses, so a live
    /// fingerprint and an indexed one for the same launch are the same string.
    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint::new(
            self.dev_buy_quote,
            self.creator_tax_bps,
            self.socials,
            self.exempt_wallets,
        )
    }

    /// Finish the features with what this session knows about the deployer.
    ///
    /// Split out because the store is not `Sync` and must not be held across an await;
    /// everything here is synchronous and cheap.
    pub fn into_features(
        self,
        deployer: DeployerSeen,
        twins_30m: u32,
        depth_blocks: u64,
    ) -> PitFeatures {
        PitFeatures {
            pair: self.pair,
            name: self.name,
            symbol: self.symbol,
            description: self.description,
            socials: self.socials,
            exempt_wallets: self.exempt_wallets,
            dev_buy_bps: self.dev_buy_bps,
            creator_tax_bps: self.creator_tax_bps,
            fee_recipient: self.fee_recipient,
            deployer_launches: deployer.launches,
            deployer_graduations: deployer.graduations,
            fingerprint_twins_30m: twins_30m,
            deployer_history_depth_blocks: depth_blocks,
        }
    }
}

/// Read everything about one launch from the chain.
///
/// `ordinal` and `total` describe this launch's position among the launches produced by
/// the same transaction — a bundler can make several — and are passed straight to the
/// decoder, which refuses to guess when the calldata frames do not match.
pub async fn read(
    client: &Client,
    launch: &Launch,
    ordinal: usize,
    total: usize,
) -> Result<Reading, RpcError> {
    let started = tokio::time::Instant::now();

    // Three round trips at once. The multicall is the long pole; the other two ride along.
    let (tx, receipt, calls) = tokio::join!(
        client.get_transaction(launch.tx_hash, Priority::Hot),
        receipt_with_one_retry(client, launch),
        client.multicall(bundle(launch), Priority::Hot),
    );

    let mut gaps = Vec::new();
    let calls = match calls {
        Ok(c) => c,
        // A failed multicall is not a failed enrichment: the calldata half is what the
        // filter mostly reads, and refusing the whole launch would lose it for a reason
        // about the endpoint rather than about the token.
        Err(e) => {
            gaps.push(format!(
                "contract reads failed ({e}); phase and pricing unknown"
            ));
            Vec::new()
        }
    };
    let state = decode_state(launch, &calls, &mut gaps);

    // --- the dev buy, from the launch transaction's own logs ---------------------------
    let (dev_buy_quote, dev_buy_tokens) = match &receipt {
        Ok(Some(r)) if r.success => dev_buy_from(r, launch.curve),
        Ok(Some(_)) => {
            // A reverted launch cannot have emitted a CurveBuy, and there is nothing here
            // worth trading anyway.
            gaps.push("the launch transaction reverted".into());
            (None, None)
        }
        Ok(None) => {
            gaps.push("no receipt yet, so the deployer's own buy could not be read".into());
            (None, None)
        }
        Err(e) => {
            gaps.push(format!(
                "receipt unreadable ({e}); the deployer's buy is unknown"
            ));
            (None, None)
        }
    };
    let dev_buy_bps = dev_buy_tokens.and_then(|tk| bps_of_supply(tk, state.supply));
    if dev_buy_bps.is_none() && dev_buy_tokens.is_some() {
        gaps.push("total supply unreadable, so the deployer's buy has no size in bps".into());
    }

    // --- the calldata, which is point-in-time by construction --------------------------
    let meta = match &tx {
        Ok(Some(t)) => launch_tx::decode_launch_at(&t.input, ordinal, total),
        Ok(None) => {
            gaps.push("launch transaction not found".into());
            unreadable()
        }
        Err(e) => {
            gaps.push(format!("launch transaction unreadable ({e})"));
            unreadable()
        }
    };
    if let LaunchMeta::Undecodable { selector, .. } = &meta {
        gaps.push(format!(
            "launch calldata did not decode (selector 0x{}); socials, creator tax and \
             exempt wallets are unknown, and any rule requiring them refuses",
            alloy_primitives::hex::encode(selector)
        ));
    }

    let (name, symbol, description, creator_tax_bps, fee_recipient) = match &meta {
        LaunchMeta::Decoded(c) => (
            // Attacker-chosen strings: strip anything that can misrepresent itself before
            // it reaches a rule, a log or a screen.
            sanitise_for_display(&c.name),
            sanitise_for_display(&c.symbol),
            sanitise_for_display(&c.description),
            Some(c.creator_tax_bps),
            if c.creator_fee_recipient == launch.deployer {
                FeeRecipient::Deployer
            } else {
                FeeRecipient::ThirdParty
            },
        ),
        LaunchMeta::Undecodable { .. } => (
            String::new(),
            String::new(),
            String::new(),
            None,
            FeeRecipient::Unknown,
        ),
    };

    let pair = decode_pair(launch, &calls, &mut gaps);

    Ok(Reading {
        launch: launch.clone(),
        name,
        symbol,
        description,
        socials: meta.socials(),
        exempt_wallets: meta.exempt_wallets(),
        creator_tax_bps,
        fee_recipient,
        dev_buy_bps,
        dev_buy_quote,
        pair,
        state,
        decoded: meta.is_decoded(),
        gaps,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

fn unreadable() -> LaunchMeta {
    LaunchMeta::Undecodable {
        selector: [0; 4],
        reason: "transaction unreadable".into(),
    }
}

/// Ask once, and once more after a block if the node had not caught up.
///
/// Detection can beat the node's own receipt indexing by a few tens of milliseconds. One
/// retry turns that into a delay instead of an unknown dev buy.
async fn receipt_with_one_retry(
    client: &Client,
    launch: &Launch,
) -> Result<Option<Receipt>, RpcError> {
    match client
        .get_transaction_receipt(launch.tx_hash, Priority::Hot)
        .await
    {
        Ok(None) => {
            tokio::time::sleep(RECEIPT_RETRY).await;
            client
                .get_transaction_receipt(launch.tx_hash, Priority::Hot)
                .await
        }
        other => other,
    }
}

/// Order matters: [`decode_state`] and [`decode_pair`] read by index.
///
/// Fourteen reads in one `aggregate3`, every one `allowFailure`, which is the shape spec
/// section 7 asks for. One reverting getter costs its own field and nothing else.
fn bundle(launch: &Launch) -> Vec<(Address, Vec<u8>)> {
    let c = launch.curve;
    let mut calls = vec![
        (
            addr::PONS_FACTORY,
            IPonsFactory::getLaunchedTokenCall {
                token: launch.token,
            }
            .abi_encode(),
        ),
        (c, IPonsCurve::getReservesCall {}.abi_encode()),
        (
            c,
            IPonsCurve::currentSnipeTaxBpsCall {
                // A throwaway recipient, never the user's wallet: `currentSnipeTaxBps` is
                // per-recipient, so reading it with the real address would tell the
                // endpoint which wallet is watching this launch. The number is the same
                // unless the wallet is exempt, and a sniper never is.
                recipient: addr::DEAD,
            }
            .abi_encode(),
        ),
        (c, IPonsCurve::feeBpsCall {}.abi_encode()),
        (c, IPonsCurve::creatorTaxBpsCall {}.abi_encode()),
        (c, IPonsCurve::isNativeQuoteCall {}.abi_encode()),
        (c, IPonsCurve::realQuoteReserveCall {}.abi_encode()),
        (c, IPonsCurve::sellableTokensCall {}.abi_encode()),
        (c, IPonsCurve::reservedTokensCall {}.abi_encode()),
        (c, IPonsCurve::graduatedCall {}.abi_encode()),
        (c, IPonsCurve::readyToGraduateCall {}.abi_encode()),
        (
            addr::PONS_FACTORY,
            IPonsFactory::getLaunchConfigCall {
                id: U256::from(launch.launch_config_id),
            }
            .abi_encode(),
        ),
        (launch.token, IPonsToken::totalSupplyCall {}.abi_encode()),
    ];
    // Only for a non-ETH pair, so the common case does not pay for a call it ignores.
    if !launch.is_native_pair() {
        calls.push((launch.pair_token, IPonsToken::symbolCall {}.abi_encode()));
    }
    calls
}

const I_LAUNCHED: usize = 0;
const I_RESERVES: usize = 1;
const I_SNIPE_TAX: usize = 2;
const I_FEE_BPS: usize = 3;
const I_CREATOR_TAX: usize = 4;
const I_NATIVE: usize = 5;
const I_REAL_QUOTE: usize = 6;
const I_SELLABLE: usize = 7;
const I_RESERVED: usize = 8;
const I_GRADUATED: usize = 9;
const I_READY: usize = 10;
const I_CONFIG: usize = 11;
const I_SUPPLY: usize = 12;
const I_PAIR_SYMBOL: usize = 13;

fn at<C: SolCall>(calls: &[CallResult], i: usize) -> Option<C::Return> {
    calls.get(i).and_then(|c| c.decode::<C>())
}

/// Re-read a token's live state. One multicall, no calldata and no receipt.
///
/// This is what the mark loop runs against every open position, and it is deliberately the
/// same bundle the entry path uses: a position priced one way at entry and another way at
/// exit would show a profit that came out of the arithmetic.
pub async fn refresh(client: &Client, launch: &Launch) -> Result<ChainState, RpcError> {
    let calls = client.multicall(bundle(launch), Priority::Hot).await?;
    let mut gaps = Vec::new();
    Ok(decode_state(launch, &calls, &mut gaps))
}

fn decode_state(launch: &Launch, calls: &[CallResult], gaps: &mut Vec<String>) -> ChainState {
    let mut s = ChainState {
        graduation_threshold: launch.graduation_threshold,
        ..Default::default()
    };

    match at::<IPonsFactory::getLaunchedTokenCall>(calls, I_LAUNCHED) {
        Some(t) => {
            s.phase = Phase::from_u8(t.phase);
            if s.phase.is_none() {
                gaps.push(format!(
                    "the factory reports phase {}, which this build does not know. \
                     Trading is refused rather than guessed at",
                    t.phase
                ));
            }
            s.live_creator_tax_bps = t.creatorTaxBps as Bps;
            s.tick_spacing = t.tickSpacing.as_i32();
            s.pool_fee = t.poolFee.to::<u32>();
            if !t.graduationThreshold.is_zero() {
                s.graduation_threshold = t.graduationThreshold;
            }
        }
        None => gaps.push(
            "the factory did not answer getLaunchedToken, so the phase is unknown and \
             trading is refused"
                .into(),
        ),
    }

    match at::<IPonsCurve::getReservesCall>(calls, I_RESERVES) {
        Some(r) => {
            s.quote_reserve = r.quoteReserve;
            s.token_reserve = r.tokenReserve;
            // Both reserves present is the minimum for any quote to mean anything.
            s.priced = !r.quoteReserve.is_zero() && !r.tokenReserve.is_zero();
        }
        None => gaps.push("the curve did not answer getReserves, so it cannot be priced".into()),
    }

    match at::<IPonsCurve::currentSnipeTaxBpsCall>(calls, I_SNIPE_TAX) {
        Some(v) => s.snipe_tax_bps = v.saturating_to::<u32>(),
        None => gaps.push(
            "the curve did not answer currentSnipeTaxBps; the opening tax will be polled \
             instead of read once"
                .into(),
        ),
    }

    if let Some(v) = at::<IPonsCurve::feeBpsCall>(calls, I_FEE_BPS) {
        s.curve_fee_bps = v.saturating_to::<u32>();
    }
    // The curve's own creator tax, which is what a trade actually pays. The point-in-time
    // one, for the filter, comes from the calldata and is a different number by design.
    if let Some(v) = at::<IPonsCurve::creatorTaxBpsCall>(calls, I_CREATOR_TAX) {
        s.live_creator_tax_bps = v.saturating_to::<u32>();
    }
    match at::<IPonsCurve::isNativeQuoteCall>(calls, I_NATIVE) {
        Some(v) => s.is_native_quote = v,
        // The event's own `pairToken` is the point-in-time answer and needs no call.
        None => s.is_native_quote = launch.is_native_pair(),
    }
    if let Some(v) = at::<IPonsCurve::realQuoteReserveCall>(calls, I_REAL_QUOTE) {
        s.real_quote_reserve = v;
    }
    if let Some(v) = at::<IPonsCurve::sellableTokensCall>(calls, I_SELLABLE) {
        s.sellable_tokens = v;
    }
    if let Some(v) = at::<IPonsCurve::reservedTokensCall>(calls, I_RESERVED) {
        s.reserved_tokens = v;
    }
    if let Some(v) = at::<IPonsCurve::graduatedCall>(calls, I_GRADUATED) {
        s.graduated = v;
    }
    if let Some(v) = at::<IPonsCurve::readyToGraduateCall>(calls, I_READY) {
        s.ready_to_graduate = v;
    }

    // Supply from the launch config: a protocol parameter, not per-token state. The
    // token's own `totalSupply()` is the fallback and is recorded as one, because it is a
    // current-state read standing in for a fixed one.
    match at::<IPonsFactory::getLaunchConfigCall>(calls, I_CONFIG) {
        Some(c) if !c.supply.is_zero() => {
            s.supply = c.supply;
            if s.curve_fee_bps == 0 {
                s.curve_fee_bps = c.curveFeeBps.saturating_to::<u32>();
            }
        }
        _ => match at::<IPonsToken::totalSupplyCall>(calls, I_SUPPLY) {
            Some(v) => s.supply = v,
            None => gaps.push(
                "neither the launch config nor the token answered with a supply, so the \
                 deployer's buy cannot be sized"
                    .into(),
            ),
        },
    }

    s
}

fn decode_pair(launch: &Launch, calls: &[CallResult], gaps: &mut Vec<String>) -> Pair {
    if launch.is_native_pair() {
        return Pair::Eth;
    }
    match at::<IPonsToken::symbolCall>(calls, I_PAIR_SYMBOL) {
        Some(sym) if !sym.trim().is_empty() => Pair::Other(sanitise_for_display(&sym)),
        _ => {
            // Named by address rather than guessed at. A `PairIn` rule listing symbols
            // will refuse this, which is right: we do not know what it pairs against.
            gaps.push(format!(
                "the pair token {} does not answer symbol(), so it is named by address \
                 and a rule listing pairs by symbol will refuse it",
                launch.pair_token
            ));
            Pair::Other(format!("{:#x}", launch.pair_token))
        }
    }
}

/// The deployer's own buy: the `CurveBuy` this curve emitted **inside the launch
/// transaction**.
///
/// Restricting to the launch transaction is what makes it point-in-time. The first buy on
/// the curve is a different thing: when the deployer launches without buying, that buy
/// belongs to a sniper in a later block, and reading it here would feed a filter a fact
/// from after the launch. That was a real bug in phase 4, measured at one launch in five.
///
/// No `CurveBuy` in a successful launch transaction is a knowable zero, not an unknown.
fn dev_buy_from(receipt: &Receipt, curve: Address) -> (Option<U256>, Option<U256>) {
    for log in receipt.logs_from(curve) {
        if log.topic0() != Some(IPonsCurve::CurveBuy::SIGNATURE_HASH) {
            continue;
        }
        let words: Vec<U256> = log.data.chunks_exact(32).map(U256::from_be_slice).collect();
        // quoteIn, tokensOut, fee, tax.
        if words.len() >= 2 {
            return (Some(words[0]), Some(words[1]));
        }
    }
    (Some(U256::ZERO), Some(U256::ZERO))
}

fn bps_of_supply(tokens: U256, supply: U256) -> Option<Bps> {
    if supply.is_zero() {
        return None;
    }
    tokens
        .checked_mul(U256::from(BPS))
        .and_then(|v| v.checked_div(supply))
        .and_then(|v| v.try_into().ok())
}

/// Group the launches of one sweep by transaction, so a bundler's launches get the
/// `(ordinal, total)` the decoder needs.
///
/// A transaction's launches are always in one sweep, because a sweep covers whole blocks.
pub fn positions(launches: &[Launch]) -> Vec<(usize, usize)> {
    let mut per_tx: std::collections::HashMap<_, Vec<usize>> = std::collections::HashMap::new();
    for (i, l) in launches.iter().enumerate() {
        per_tx.entry(l.tx_hash).or_default().push(i);
    }
    let mut out = vec![(0usize, 1usize); launches.len()];
    for group in per_tx.values() {
        // By log index, which is the order the factory emitted them in.
        let mut sorted = group.clone();
        sorted.sort_by_key(|&i| launches[i].log_index);
        let total = sorted.len();
        for (ordinal, &i) in sorted.iter().enumerate() {
            out[i] = (ordinal, total);
        }
    }
    out
}

/// True when a launch is worth spending an enrichment on.
///
/// Only that the pair is one the strategy allows and the launch is not already too old —
/// cheap checks from the event alone. Everything else needs the reads.
pub fn worth_enriching(launch: &Launch, decay_ms: u64, stale_grace_ms: u64) -> bool {
    launch.age_estimate_ms() <= decay_ms.saturating_add(stale_grace_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{B256, Bytes, hex};
    use banana_chain::rpc::RawLog;
    use banana_core::features::Presence;

    fn launch(block: u64, tx: u8, log_index: u64) -> Launch {
        Launch {
            token: Address::repeat_byte(1),
            curve: Address::repeat_byte(2),
            deployer: Address::repeat_byte(3),
            pair_token: Address::ZERO,
            launch_config_id: 0,
            graduation_threshold: U256::from(4_000_000_000_000_000_000u64),
            block,
            tx_hash: B256::repeat_byte(tx),
            log_index,
            head_at_sight: block + 3,
            ordinal: 0,
            total: 1,
        }
    }

    fn curve_buy_log(curve: Address, quote: u64, tokens: u128) -> RawLog {
        let mut data = Vec::new();
        data.extend_from_slice(&U256::from(quote).to_be_bytes::<32>());
        data.extend_from_slice(&U256::from(tokens).to_be_bytes::<32>());
        data.extend_from_slice(&[0u8; 32]); // fee
        data.extend_from_slice(&[0u8; 32]); // tax
        RawLog {
            address: curve,
            topics: vec![
                IPonsCurve::CurveBuy::SIGNATURE_HASH,
                B256::repeat_byte(3),
                B256::repeat_byte(3),
            ],
            data: Bytes::from(data),
            block_number: 1,
            tx_hash: B256::repeat_byte(9),
            tx_index: 0,
            log_index: 0,
        }
    }

    fn receipt(logs: Vec<RawLog>, success: bool) -> Receipt {
        Receipt {
            tx_hash: B256::repeat_byte(9),
            block_number: 1,
            success,
            gas_used: 100_000,
            effective_gas_price: 1_000_000,
            logs,
        }
    }

    #[test]
    fn the_dev_buy_comes_from_the_launch_transactions_own_curve_buy() {
        let curve = Address::repeat_byte(2);
        let r = receipt(vec![curve_buy_log(curve, 1_000, 5_000)], true);
        assert_eq!(
            dev_buy_from(&r, curve),
            (Some(U256::from(1_000)), Some(U256::from(5_000)))
        );
    }

    /// A launch transaction with no buy is a knowable zero — the deployer launched without
    /// buying — and must not read as unknown.
    #[test]
    fn no_buy_in_the_launch_transaction_is_a_real_zero() {
        let curve = Address::repeat_byte(2);
        assert_eq!(
            dev_buy_from(&receipt(vec![], true), curve),
            (Some(U256::ZERO), Some(U256::ZERO))
        );
    }

    /// The point-in-time rule that was a real bug: a buy from another curve in the same
    /// transaction is not this token's dev buy.
    #[test]
    fn a_buy_on_a_different_curve_is_not_this_tokens_dev_buy() {
        let ours = Address::repeat_byte(2);
        let theirs = Address::repeat_byte(7);
        let r = receipt(vec![curve_buy_log(theirs, 9_999, 9_999)], true);
        assert_eq!(dev_buy_from(&r, ours), (Some(U256::ZERO), Some(U256::ZERO)));
    }

    #[test]
    fn bps_of_supply_is_integer_arithmetic() {
        // 5% of supply.
        let supply = U256::from(1_000_000u64);
        assert_eq!(bps_of_supply(U256::from(50_000u64), supply), Some(500));
        assert_eq!(bps_of_supply(U256::from(1u64), U256::ZERO), None);
    }

    /// A bundler's launches must each get their own frame, in log-index order.
    #[test]
    fn positions_number_a_bundlers_launches_in_emission_order() {
        let ls = vec![
            launch(100, 1, 5),
            launch(100, 1, 2),
            launch(100, 2, 9),
            launch(100, 1, 7),
        ];
        let p = positions(&ls);
        assert_eq!(p[1], (0, 3), "log index 2 is the first of its transaction");
        assert_eq!(p[0], (1, 3));
        assert_eq!(p[3], (2, 3));
        assert_eq!(p[2], (0, 1), "a different transaction stands alone");
    }

    #[test]
    fn a_launch_older_than_the_window_plus_grace_is_not_worth_reading() {
        let mut l = launch(900_000, 1, 0);
        l.head_at_sight = 900_000 + 10;
        assert!(worth_enriching(&l, 3_000, 1_000));
        l.head_at_sight = 900_000 + 100;
        assert!(!worth_enriching(&l, 3_000, 1_000));
    }

    /// The live fingerprint is built by `core`'s constructor, so it is character-identical
    /// to the one the indexer stores for the same launch.
    #[test]
    fn the_fingerprint_matches_the_indexers() {
        let reading = Reading {
            launch: launch(1, 1, 0),
            name: "a".into(),
            symbol: "A".into(),
            description: String::new(),
            socials: Socials {
                twitter: Presence::Present,
                website: Presence::Absent,
                telegram: Presence::Absent,
            },
            exempt_wallets: Some(2),
            creator_tax_bps: Some(100),
            fee_recipient: FeeRecipient::Deployer,
            dev_buy_bps: Some(300),
            dev_buy_quote: Some(U256::from(7u64)),
            pair: Pair::Eth,
            state: ChainState::default(),
            decoded: true,
            gaps: Vec::new(),
            elapsed_ms: 0,
        };
        let expected = Fingerprint::new(
            Some(U256::from(7u64)),
            Some(100),
            Socials {
                twitter: Presence::Present,
                website: Presence::Absent,
                telegram: Presence::Absent,
            },
            Some(2),
        );
        assert_eq!(reading.fingerprint().as_str(), expected.as_str());
    }

    /// An unreadable phase is `None`, and the gap says routing will refuse.
    #[test]
    fn an_unknown_phase_is_none_and_says_so() {
        let mut gaps = Vec::new();
        let s = decode_state(&launch(1, 1, 0), &[], &mut gaps);
        assert_eq!(s.phase, None);
        assert!(gaps.iter().any(|g| g.contains("phase is unknown")));
    }

    /// A missing socials read is Unknown, never Absent: a `require_twitter` rule must
    /// refuse rather than conclude the token has no Twitter.
    #[test]
    fn an_undecodable_launch_leaves_socials_unknown() {
        let m = unreadable();
        assert_eq!(m.socials(), Socials::UNKNOWN);
        assert_eq!(m.exempt_wallets(), None);
    }

    #[test]
    fn the_bundle_skips_the_pair_symbol_for_an_eth_launch() {
        let native = launch(1, 1, 0);
        assert_eq!(bundle(&native).len(), I_PAIR_SYMBOL);
        let mut other = native.clone();
        other.pair_token = Address::repeat_byte(8);
        assert_eq!(bundle(&other).len(), I_PAIR_SYMBOL + 1);
    }

    #[test]
    fn a_pair_token_that_will_not_name_itself_is_named_by_address() {
        let mut l = launch(1, 1, 0);
        l.pair_token = Address::repeat_byte(8);
        let mut gaps = Vec::new();
        let p = decode_pair(&l, &[], &mut gaps);
        assert_eq!(p, Pair::Other(format!("{:#x}", l.pair_token)));
        assert!(gaps.iter().any(|g| g.contains("will refuse it")));
    }

    #[test]
    fn an_eth_launch_is_the_eth_pair_with_no_read_at_all() {
        let mut gaps = Vec::new();
        assert_eq!(decode_pair(&launch(1, 1, 0), &[], &mut gaps), Pair::Eth);
        assert!(gaps.is_empty());
    }

    #[test]
    fn the_selector_of_an_unreadable_transaction_is_reported_as_zero_not_guessed() {
        match unreadable() {
            LaunchMeta::Undecodable { selector, .. } => {
                assert_eq!(hex::encode(selector), "00000000")
            }
            _ => panic!("must not decode"),
        }
    }
}
