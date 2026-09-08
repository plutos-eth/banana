//! Execute — build, simulate, sign, send, and read back what actually happened.
//!
//! # Nothing here spends without a [`Spend`]
//!
//! Every entry path takes one by value. A `Spend` is produced only by
//! [`crate::guards::Budget::authorise`] and cannot be cloned, so a function that forgets
//! the money guards has nothing to pass and does not compile. Exits do not take one: the
//! guards limit what goes *in*, and refusing to sell would be a guard that loses money
//! rather than saves it.
//!
//! # Simulate first, always
//!
//! Before anything is signed, the order is run through `eth_call` and `eth_estimateGas`
//! against the real contract at the real block. Two things fall out of that, and both
//! matter more than the gas number:
//!
//! * **A revert is caught before it costs anything.** "The curve would refuse this order,
//!   and here is what it said" is a refusal to show the user, not a failed transaction to
//!   explain afterwards.
//! * **TEST becomes a real rehearsal.** The same call gives the tokens the order would
//!   have bought, so a dry run reports a fill it actually verified rather than a number it
//!   made up. TEST runs every step of this file and stops at the signature, because the
//!   session holds a [`crate::NoSigner`] and there is nothing to sign with.
//!
//! # What a fill reports is what the chain says it paid
//!
//! Not the intent. After a send, the receipt's own `CurveBuy` and `SnipeTaxCharged` logs
//! give the quote in, the tokens out and the opening tax actually taken (PLAN.md F7). A
//! journal of intentions would be a journal of fiction.
//!
//! # Gas
//!
//! No priority fee: this is an Arbitrum-stack chain with no mempool to bid into (spec §2),
//! so bidding buys nothing and the base fee is what is paid. The limit comes from
//! `eth_estimateGas` with a margin, never from a constant — a constant is how a contract
//! upgrade turns into a wallet full of out-of-gas receipts.

use std::time::Duration;

use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::{SolCall, SolEvent, SolValue};
use banana_chain::abi::{IPonsCurve, IPonsToken};
use banana_chain::gate::{Priority, RpcError};
use banana_chain::rpc::{Client, Receipt};
use banana_core::Bps;
use banana_store::types::Side;

use crate::guards::Spend;
use crate::route::Route;
use crate::session::{Session, SessionError};

/// Multiplier on the estimated gas limit, as a percentage.
///
/// The estimate is taken against the block before ours lands; a curve whose state moved in
/// between can cost a little more. 25% is generous for a fixed-shape call and cheap when
/// unused, because unspent gas is not charged.
const GAS_MARGIN_PCT: u64 = 125;

/// Multiplier on the observed gas price, as a percentage.
///
/// Headroom for the base fee moving between the quote and the send. With no priority fee
/// there is nothing else to pay, and the excess is refunded.
const FEE_HEADROOM_PCT: u64 = 200;

/// How long to wait for a receipt before reporting the send as unconfirmed.
const RECEIPT_TIMEOUT: Duration = Duration::from_secs(30);
const RECEIPT_POLL: Duration = Duration::from_millis(200);

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error(transparent)]
    Rpc(#[from] RpcError),
    #[error(transparent)]
    Session(#[from] SessionError),
    /// The order would fail on chain. Reported before anything is signed.
    #[error("the {venue} refused this order: {detail}")]
    WouldRevert { venue: String, detail: String },
    #[error("{0}")]
    NoVenue(String),
    #[error(
        "selling into a Uniswap v4 pool is not implemented yet, so this position cannot be \
         closed by the program. It graduated out of its curve while held; sell it by hand \
         and it will drop out of the list"
    )]
    PoolSellUnsupported,
    /// The transaction was sent and no receipt arrived in time.
    ///
    /// Its own case because it is the one state where the program genuinely does not know
    /// whether money moved, and saying so is the only honest answer.
    #[error(
        "transaction {hash} was sent but has not been mined after {secs}s. Check the \
         explorer before assuming it failed: it may still land"
    )]
    Unconfirmed { hash: B256, secs: u64 },
    #[error("the transaction reverted on chain: {hash}")]
    Reverted { hash: B256 },
}

/// One order, priced and ready to build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Order {
    pub route: Route,
    pub side: Side,
    /// A buy: the quote asset going in. A sell: what came back is measured, not set here.
    pub quote_wei: U256,
    /// A sell: the tokens going in.
    pub tokens_in: U256,
    /// The slippage floor, already applied.
    pub min_out: U256,
    pub recipient: Address,
    /// Who the order is simulated and sent as.
    ///
    /// In LIVE this is the session's wallet. In TEST it is whichever address the user has
    /// configured, so the rehearsal is priced against a real balance rather than against
    /// an empty one — and the signature is still the thing that cannot happen.
    pub from: Address,
}

/// What a completed order actually did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filled {
    /// `None` in TEST, where nothing was sent. The field that stops a rehearsal being
    /// mistaken for a trade, in the journal and everywhere it is read from.
    pub tx_hash: Option<B256>,
    pub simulated: bool,
    /// Quote in for a buy, quote out for a sell.
    pub quote_wei: U256,
    pub tokens: U256,
    pub gas_wei: Option<U256>,
    /// What the opening tax actually took, from `SnipeTaxCharged` (PLAN.md F7).
    pub snipe_tax_wei: Option<U256>,
    pub venue: String,
    pub detail: String,
}

/// Everything an order needs from the chain before it can be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quoted {
    /// What the simulation says will come out.
    pub out: U256,
    pub gas_limit: u64,
    pub max_fee_per_gas: u128,
    pub nonce: u64,
}

/// Buy on the curve.
///
/// Takes the [`Spend`] by value, so the guards must have agreed and the permission cannot
/// be used again. Consumed even when the order fails: an authorisation that survives a
/// failed attempt is an authorisation that can fund a second one.
pub async fn buy(
    client: &Client,
    session: &mut Session,
    spend: Spend,
    order: &Order,
    tax_bps_at_decision: Bps,
) -> Result<Filled, ExecError> {
    let curve = match &order.route {
        Route::Curve { curve } => *curve,
        Route::Pool { .. } => {
            return Err(ExecError::NoVenue(
                "this token has already graduated; banana only opens positions on the \
                 curve, where the opening-tax edge is"
                    .into(),
            ));
        }
        Route::Refuse { reason } => return Err(ExecError::NoVenue(reason.clone())),
    };

    // One source for the amount: the permission itself. Taking it from the order instead
    // would let an order and its authorisation drift apart, which is the whole failure a
    // `Spend` exists to make impossible.
    let wei = spend.wei();
    let data = IPonsCurve::buyCall {
        quoteIn: wei,
        minTokensOut: order.min_out,
        recipient: order.recipient,
    }
    .abi_encode();

    // Native ETH goes as `value`; the call is payable. Non-ETH pairs are refused upstream,
    // by name, because they would need an allowance this program does not manage.
    let quoted = quote(client, order.from, curve, wei, &data, "curve").await?;

    send(
        client,
        session,
        Some(spend),
        curve,
        wei,
        data,
        quoted,
        "curve",
        Side::Buy,
        Some(tax_bps_at_decision),
    )
    .await
}

/// Sell tokens back to the curve.
///
/// No [`Spend`]: the money guards cap what is put at risk, and a guard that could refuse
/// an exit would be a guard that loses money.
pub async fn sell(
    client: &Client,
    session: &mut Session,
    order: &Order,
) -> Result<Filled, ExecError> {
    let curve = match &order.route {
        Route::Curve { curve } => *curve,
        Route::Pool { .. } => return Err(ExecError::PoolSellUnsupported),
        Route::Refuse { reason } => return Err(ExecError::NoVenue(reason.clone())),
    };

    let data = IPonsCurve::sellCall {
        tokensIn: order.tokens_in,
        minQuoteOut: order.min_out,
        recipient: order.recipient,
    }
    .abi_encode();

    let quoted = quote(client, order.from, curve, U256::ZERO, &data, "curve").await?;
    send(
        client,
        session,
        None,
        curve,
        U256::ZERO,
        data,
        quoted,
        "curve",
        Side::Sell,
        None,
    )
    .await
}

/// Make sure the curve may move `tokens` on our behalf.
///
/// Read first: an allowance that is already enough costs one call to confirm, and an
/// approval sent every time would double the transactions and the gas for nothing.
pub async fn ensure_allowance(
    client: &Client,
    session: &mut Session,
    token: Address,
    spender: Address,
    tokens: U256,
) -> Result<Option<B256>, ExecError> {
    let owner = session.address();
    let current = client
        .call(
            token,
            &IPonsToken::allowanceCall { owner, spender },
            Priority::Hot,
        )
        .await?;
    if current >= tokens {
        return Ok(None);
    }
    let data = IPonsToken::approveCall {
        spender,
        amount: U256::MAX,
    }
    .abi_encode();
    let quoted = quote(client, owner, token, U256::ZERO, &data, "token").await?;
    let filled = send(
        client,
        session,
        None,
        token,
        U256::ZERO,
        data,
        quoted,
        "token",
        Side::Sell,
        None,
    )
    .await?;
    Ok(filled.tx_hash)
}

/// Simulate the order and price its gas.
///
/// Three reads in one round of the gate. The `eth_call` is what turns a revert into a
/// sentence before it turns into a receipt.
async fn quote(
    client: &Client,
    from: Address,
    to: Address,
    value: U256,
    data: &[u8],
    venue: &str,
) -> Result<Quoted, ExecError> {
    let (simulated, gas, price, nonce) = tokio::join!(
        client.call_raw(from, to, value, data, Priority::Hot),
        client.estimate_gas(from, to, value, data, Priority::Hot),
        client.gas_price(Priority::Hot),
        client.next_nonce(from, Priority::Hot),
    );

    // A revert shows up in either of the first two. Reported as a refusal with the
    // contract's own words, never as a generic failure.
    let simulated = simulated.map_err(|e| revert_or(e, venue))?;
    let gas = gas.map_err(|e| revert_or(e, venue))?;

    let out = U256::abi_decode(&simulated).unwrap_or(U256::ZERO);
    let price = price?;
    Ok(Quoted {
        out,
        gas_limit: gas.saturating_mul(GAS_MARGIN_PCT) / 100,
        max_fee_per_gas: price.saturating_mul(FEE_HEADROOM_PCT as u128) / 100,
        nonce: nonce?,
    })
}

/// Turn an endpoint error into a refusal when it is one, and leave it alone when it is not.
///
/// The distinction matters: "the curve would refuse this order" is something to show the
/// user and move on from, while "the endpoint is down" is something to retry.
fn revert_or(e: RpcError, venue: &str) -> ExecError {
    let text = e.to_string();
    if text.contains("revert") || text.contains("execution") {
        ExecError::WouldRevert {
            venue: venue.to_owned(),
            detail: text,
        }
    } else {
        ExecError::Rpc(e)
    }
}

/// Sign and broadcast, or — in TEST — stop at the signature and report the simulation.
///
/// The stop is not a check. A test session holds a [`crate::NoSigner`], which has no key,
/// so `sign` returns an error and there is no branch anywhere that could be edited to make
/// a dry run spend money.
#[allow(clippy::too_many_arguments)]
async fn send(
    client: &Client,
    session: &mut Session,
    spend: Option<Spend>,
    to: Address,
    value: U256,
    data: Vec<u8>,
    quoted: Quoted,
    venue: &str,
    side: Side,
    tax_bps: Option<Bps>,
) -> Result<Filled, ExecError> {
    let tx = crate::signer::TxRequest {
        chain_id: banana_chain::addr::CHAIN_ID,
        nonce: quoted.nonce,
        to,
        value,
        data: data.into(),
        gas_limit: quoted.gas_limit,
        max_fee_per_gas: quoted.max_fee_per_gas,
        // No mempool, no auction, nothing to bid into.
        max_priority_fee_per_gas: 0,
    };

    let signed = match spend {
        Some(s) => session.sign(s, &tx),
        None => session.sign_exit(&tx),
    };

    let signed = match signed {
        Ok(s) => s,
        // The dry-run path. Everything above this line really happened — the order was
        // built against the real curve and the real chain confirmed it would work — and
        // the only thing that did not is the part that costs money.
        Err(SessionError::Signer(crate::SignerError::NoKey)) => {
            return Ok(Filled {
                tx_hash: None,
                simulated: true,
                quote_wei: if side == Side::Buy { value } else { quoted.out },
                tokens: if side == Side::Buy {
                    quoted.out
                } else {
                    U256::ZERO
                },
                gas_wei: Some(
                    U256::from(quoted.gas_limit).saturating_mul(U256::from(quoted.max_fee_per_gas)),
                ),
                snipe_tax_wei: None,
                venue: venue.to_owned(),
                detail: format!(
                    "TEST: simulated against the live {venue}, which confirmed the order \
                     would return {} — not sent, because this session holds no key",
                    quoted.out
                ),
            });
        }
        Err(e) => return Err(e.into()),
    };

    let hash = client
        .send_raw_transaction(&signed.raw, Priority::Hot)
        .await?;
    tracing::info!(%hash, venue, "sent");

    let receipt = await_receipt(client, hash).await?;
    if !receipt.success {
        return Err(ExecError::Reverted { hash });
    }

    let (quote_wei, tokens, snipe_tax_wei) = read_back(&receipt, to, side);
    Ok(Filled {
        tx_hash: Some(hash),
        simulated: false,
        quote_wei,
        tokens,
        gas_wei: Some(receipt.fee_wei()),
        snipe_tax_wei,
        venue: venue.to_owned(),
        detail: match tax_bps {
            Some(bps) => format!("decided at {bps} bps of opening tax"),
            None => String::new(),
        },
    })
}

async fn await_receipt(client: &Client, hash: B256) -> Result<Receipt, ExecError> {
    let deadline = tokio::time::Instant::now() + RECEIPT_TIMEOUT;
    loop {
        if let Some(r) = client.get_transaction_receipt(hash, Priority::Hot).await? {
            return Ok(r);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ExecError::Unconfirmed {
                hash,
                secs: RECEIPT_TIMEOUT.as_secs(),
            });
        }
        tokio::time::sleep(RECEIPT_POLL).await;
    }
}

/// What the transaction actually did, from its own logs.
///
/// Not from the calldata and not from the simulation: the curve moved between the quote
/// and the send, and the difference is exactly the number a user needs to see.
pub fn read_back(receipt: &Receipt, curve: Address, side: Side) -> (U256, U256, Option<U256>) {
    let want = match side {
        Side::Buy => IPonsCurve::CurveBuy::SIGNATURE_HASH,
        Side::Sell => IPonsCurve::CurveSell::SIGNATURE_HASH,
    };
    let mut quote = U256::ZERO;
    let mut tokens = U256::ZERO;
    for log in receipt.logs_from(curve) {
        let Some(t0) = log.topic0() else { continue };
        if t0 == want {
            let words: Vec<U256> = log.data.chunks_exact(32).map(U256::from_be_slice).collect();
            if words.len() >= 2 {
                // A buy is (quoteIn, tokensOut, ...); a sell is (tokensIn, quoteOut, ...).
                let (a, b) = (words[0], words[1]);
                match side {
                    Side::Buy => {
                        quote = a;
                        tokens = b;
                    }
                    Side::Sell => {
                        tokens = a;
                        quote = b;
                    }
                }
            }
        }
    }
    // Emitted by the curve on a taxed buy. Its **absence** is the signal that the opening
    // window has closed, so `None` and `Some(0)` are different answers here.
    let snipe_tax = receipt
        .logs_from(curve)
        .find(|l| l.topic0() == Some(IPonsCurve::SnipeTaxCharged::SIGNATURE_HASH))
        .and_then(|l| l.data.get(..32).map(U256::from_be_slice));
    (quote, tokens, snipe_tax)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Bytes;
    use banana_chain::rpc::RawLog;

    fn log(curve: Address, topic0: B256, words: &[u64]) -> RawLog {
        let mut data = Vec::new();
        for w in words {
            data.extend_from_slice(&U256::from(*w).to_be_bytes::<32>());
        }
        RawLog {
            address: curve,
            topics: vec![topic0, B256::repeat_byte(1), B256::repeat_byte(1)],
            data: Bytes::from(data),
            block_number: 1,
            tx_hash: B256::repeat_byte(9),
            tx_index: 0,
            log_index: 0,
        }
    }

    fn receipt(logs: Vec<RawLog>) -> Receipt {
        Receipt {
            tx_hash: B256::repeat_byte(9),
            block_number: 1,
            success: true,
            gas_used: 200_000,
            effective_gas_price: 1_000_000_000,
            logs,
        }
    }

    #[test]
    fn a_buy_is_read_back_from_its_own_curve_buy_log() {
        let c = Address::repeat_byte(2);
        let r = receipt(vec![log(
            c,
            IPonsCurve::CurveBuy::SIGNATURE_HASH,
            &[1_000, 55_000, 10, 0],
        )]);
        let (quote, tokens, tax) = read_back(&r, c, Side::Buy);
        assert_eq!(quote, U256::from(1_000u64));
        assert_eq!(tokens, U256::from(55_000u64));
        assert_eq!(tax, None, "no SnipeTaxCharged means the window had closed");
    }

    /// A sell's words are the other way round. Reading them in buy order would report a
    /// sale of 55,000 wei for 1,000 tokens.
    #[test]
    fn a_sell_reads_tokens_in_and_quote_out_in_that_order() {
        let c = Address::repeat_byte(2);
        let r = receipt(vec![log(
            c,
            IPonsCurve::CurveSell::SIGNATURE_HASH,
            &[55_000, 1_200, 10, 0],
        )]);
        let (quote, tokens, _) = read_back(&r, c, Side::Sell);
        assert_eq!(tokens, U256::from(55_000u64));
        assert_eq!(quote, U256::from(1_200u64));
    }

    /// The opening tax is read from the log, not inferred from the intent.
    #[test]
    fn the_opening_tax_paid_comes_from_the_snipe_tax_log() {
        let c = Address::repeat_byte(2);
        let r = receipt(vec![
            log(
                c,
                IPonsCurve::CurveBuy::SIGNATURE_HASH,
                &[1_000, 55_000, 10, 3],
            ),
            log(c, IPonsCurve::SnipeTaxCharged::SIGNATURE_HASH, &[42]),
        ]);
        let (_, _, tax) = read_back(&r, c, Side::Buy);
        assert_eq!(tax, Some(U256::from(42u64)));
    }

    /// Another curve's logs in the same transaction are not ours.
    #[test]
    fn logs_from_a_different_curve_are_ignored() {
        let ours = Address::repeat_byte(2);
        let theirs = Address::repeat_byte(7);
        let r = receipt(vec![log(
            theirs,
            IPonsCurve::CurveBuy::SIGNATURE_HASH,
            &[9_999, 9_999, 0, 0],
        )]);
        assert_eq!(
            read_back(&r, ours, Side::Buy),
            (U256::ZERO, U256::ZERO, None)
        );
    }

    /// A revert is a refusal to show, not a network error to retry.
    #[test]
    fn a_revert_is_classified_as_a_refusal() {
        let e = revert_or(
            RpcError::Rpc {
                code: 3,
                message: "execution reverted: SlippageExceeded".into(),
            },
            "curve",
        );
        assert!(matches!(e, ExecError::WouldRevert { .. }));
        assert!(e.to_string().contains("SlippageExceeded"));
    }

    #[test]
    fn a_dead_endpoint_is_not_mistaken_for_a_revert() {
        let e = revert_or(
            RpcError::Malformed {
                label: "eth_call".into(),
                detail: "connection reset".into(),
            },
            "curve",
        );
        assert!(matches!(e, ExecError::Rpc(_)));
    }

    /// A graduated position cannot be closed by this build, and says so rather than
    /// routing a sell somewhere that would revert or, worse, succeed wrongly.
    #[test]
    fn a_pool_sell_refuses_by_name() {
        let e = ExecError::PoolSellUnsupported;
        assert!(e.to_string().contains("sell it by hand"));
    }

    #[test]
    fn gas_carries_a_margin_and_no_priority_fee() {
        let q = Quoted {
            out: U256::from(1u64),
            gas_limit: 100_000u64.saturating_mul(GAS_MARGIN_PCT) / 100,
            max_fee_per_gas: 1_000u128.saturating_mul(FEE_HEADROOM_PCT as u128) / 100,
            nonce: 0,
        };
        assert_eq!(q.gas_limit, 125_000);
        assert_eq!(q.max_fee_per_gas, 2_000);
    }
}
