//! Bonding-curve pricing, in the protocol's integer order.
//!
//! This is a port of `PonsV2BondingCurve.buy/sell`, by way of bodkin's `src/pons/curve.ts`
//! (MIT), which in turn credits `slightlyuseless/pons-sniper` (MIT). **The operation order
//! is preserved symbol for symbol and must not be "cleaned up"** (spec §1, §12): the
//! deployed contract is the authority, and `min_tokens_out` is only a safe bound if it is
//! computed with the same rounding the contract uses.
//!
//! Fees come off the *input* on a buy and off the *output* on a sell. The opening (snipe)
//! tax only ever applies to buys.
//!
//! No floating point appears anywhere in this file; `tests/source_hygiene.rs` enforces
//! that. Every ratio is integer basis points.
//!
//! # Verified against the chain
//!
//! `quote_buy` reproduces a real launch transaction exactly, with zero difference — see
//! `reproduces_a_real_dev_buy` below. That fixture is not a remembered number: it is the
//! `CurveBuy` event of a real launch, checked against the `LaunchConfig` read from the
//! factory.

use alloy_primitives::U256;

use crate::{BPS, Bps};

/// Something the arithmetic cannot represent. Every one of these means the inputs were
/// impossible, not that the trade is merely unprofitable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CurveError {
    /// A multiplication or addition exceeded 256 bits. Real reserves cannot do this
    /// (the largest realistic product is ~1e45 against a ~1.2e77 ceiling), so this means
    /// corrupt or adversarial input.
    #[error("curve arithmetic overflowed 256 bits")]
    Overflow,
    /// A sell asked for more tokens out than the reserve holds.
    #[error("output {requested} exceeds reserve {reserve}")]
    ReserveExhausted { requested: U256, reserve: U256 },
    /// Fee plus creator tax already consumes the whole input, leaving nothing to swap.
    #[error("fee {fee_bps}bps + creator tax {tax_bps}bps + opening {opening_bps}bps >= 100%")]
    InputFullyConsumed {
        fee_bps: Bps,
        tax_bps: Bps,
        opening_bps: Bps,
    },
}

type Result<T> = core::result::Result<T, CurveError>;

#[inline]
fn mul(a: U256, b: U256) -> Result<U256> {
    a.checked_mul(b).ok_or(CurveError::Overflow)
}

#[inline]
fn add(a: U256, b: U256) -> Result<U256> {
    a.checked_add(b).ok_or(CurveError::Overflow)
}

/// Constant-product output. `(in * reserve_out) / (reserve_in + in)`, truncating.
///
/// Multiply first, then divide: reversing it loses precision and stops matching the
/// contract.
pub fn amount_out(in_amount: U256, reserve_in: U256, reserve_out: U256) -> Result<U256> {
    let denom = add(reserve_in, in_amount)?;
    if denom.is_zero() {
        return Ok(U256::ZERO);
    }
    Ok(mul(in_amount, reserve_out)? / denom)
}

/// Constant-product input required for an exact output, `+1` to round in the pool's
/// favour exactly as the contract does.
pub fn amount_in(out_amount: U256, reserve_in: U256, reserve_out: U256) -> Result<U256> {
    if out_amount >= reserve_out {
        return Err(CurveError::ReserveExhausted {
            requested: out_amount,
            reserve: reserve_out,
        });
    }
    let numer = mul(out_amount, reserve_in)?;
    add(numer / (reserve_out - out_amount), U256::ONE)
}

/// Ceiling division. Used when grossing a net amount back up through the fee.
fn ceil_div(a: U256, b: U256) -> Result<U256> {
    debug_assert!(!b.is_zero(), "ceil_div by zero");
    if b.is_zero() {
        return Err(CurveError::Overflow);
    }
    Ok(add(a, b - U256::ONE)? / b)
}

#[inline]
fn bps_of(amount: U256, bps: Bps) -> Result<U256> {
    Ok(mul(amount, U256::from(bps))? / U256::from(BPS))
}

/// The curve's state, reduced to what pricing needs.
///
/// Live metadata (when it was read, whether the socket was healthy) deliberately lives
/// elsewhere: this type is pure input to pure functions, so the same struct serves a live
/// quote and a replayed historical one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurveState {
    /// Includes the phantom reserve.
    pub quote_reserve: U256,
    pub token_reserve: U256,
    /// The non-phantom part: what has actually been paid in. Drives graduation.
    pub real_quote_reserve: U256,
    pub sellable_tokens: U256,
    pub reserved_tokens: U256,
    pub graduation_threshold: U256,
    pub fee_bps: Bps,
    pub creator_tax_bps: Bps,
    /// The opening tax *for a specific recipient*: an exempt wallet reads zero here.
    pub opening_tax_bps: Bps,
    pub graduated: bool,
    pub ready_to_graduate: bool,
}

impl CurveState {
    /// The state a curve opens in, from its launch config.
    ///
    /// Verified against the chain: `quote_reserve` starts at the phantom quote and
    /// `token_reserve` at the full supply. Reserved tokens are
    /// `supply * phantom / (phantom + threshold)`, which for the live config (1.68 and
    /// 4.2 ETH) is exactly 2/7 of supply -- the 28.57% of spec §2.
    ///
    /// This is what makes the F9 entry reconstruction possible without an archive node:
    /// combined with [`CurveState::apply_buy`] and [`CurveState::apply_sell`], the entire
    /// price path of every token is replayable from logs alone.
    pub fn opening(config: &LaunchConfig) -> Result<Self> {
        let denom = add(config.phantom_quote, config.graduation_threshold)?;
        let reserved = if denom.is_zero() {
            U256::ZERO
        } else {
            mul(config.supply, config.phantom_quote)? / denom
        };
        Ok(Self {
            quote_reserve: config.phantom_quote,
            token_reserve: config.supply,
            real_quote_reserve: U256::ZERO,
            sellable_tokens: config.supply - reserved,
            reserved_tokens: reserved,
            graduation_threshold: config.graduation_threshold,
            fee_bps: config.curve_fee_bps,
            creator_tax_bps: 0,
            opening_tax_bps: 0,
            graduated: false,
            ready_to_graduate: false,
        })
    }

    /// Advance the state by an observed `CurveBuy`.
    ///
    /// Every argument comes straight from the event payload, so a replay is a
    /// re-derivation rather than a simulation.
    ///
    /// # The snipe tax is already inside `fee`
    ///
    /// The opening tax is **not** subtracted here, and that is not an oversight. Measured
    /// on 400 snipe-taxed buys, `fee - snipeTaxCharged` is exactly 1% of `quoteIn` in
    /// every single case: the event's `fee` field aggregates the curve fee and the opening
    /// tax. Subtracting the `SnipeTaxCharged` amount as well double-counts it.
    ///
    /// It is worth knowing how badly that fails, because it fails quietly. A curve's first
    /// snipe-taxed buy is off by ~0.2%, and every later trade on that curve then replays
    /// from a wrong reserve -- so a single mistake at trade 22 of 577 poisons the other
    /// 555. Replay exactness across real curves went from 30% to 99% on this one change.
    ///
    /// [`quote_buy`] is unaffected: it *predicts* a buy from basis points, where the three
    /// deductions genuinely are separate. This is the *replay* path, which consumes an
    /// event that has already combined them.
    pub fn apply_buy(
        &mut self,
        quote_in: U256,
        tokens_out: U256,
        fee: U256,
        tax: U256,
    ) -> Result<()> {
        let deducted = add(fee, tax)?;
        let net = quote_in.checked_sub(deducted).ok_or(CurveError::Overflow)?;
        self.quote_reserve = add(self.quote_reserve, net)?;
        self.real_quote_reserve = add(self.real_quote_reserve, net)?;
        self.token_reserve =
            self.token_reserve
                .checked_sub(tokens_out)
                .ok_or(CurveError::ReserveExhausted {
                    requested: tokens_out,
                    reserve: self.token_reserve,
                })?;
        self.sellable_tokens = self.sellable_tokens.saturating_sub(tokens_out);
        Ok(())
    }

    /// Advance the state by an observed `CurveSell`. Fee and tax come off the *output*,
    /// so the reserve moves by the gross, not by what the seller received.
    pub fn apply_sell(
        &mut self,
        tokens_in: U256,
        quote_out: U256,
        fee: U256,
        tax: U256,
    ) -> Result<()> {
        let gross = add(add(quote_out, fee)?, tax)?;
        self.quote_reserve =
            self.quote_reserve
                .checked_sub(gross)
                .ok_or(CurveError::ReserveExhausted {
                    requested: gross,
                    reserve: self.quote_reserve,
                })?;
        self.real_quote_reserve = self.real_quote_reserve.saturating_sub(gross);
        self.token_reserve = add(self.token_reserve, tokens_in)?;
        self.sellable_tokens = add(self.sellable_tokens, tokens_in)?;
        Ok(())
    }

    /// Progress toward graduation in basis points, capped at 10000.
    ///
    /// Integer, not a float: this is displayed to the user as a percentage and a float
    /// here would be the first crack in spec §12.
    pub fn progress_bps(&self) -> Bps {
        if self.graduation_threshold.is_zero() {
            return 0;
        }
        let p = self.real_quote_reserve.saturating_mul(U256::from(BPS)) / self.graduation_threshold;
        if p > U256::from(BPS) {
            BPS
        } else {
            p.to::<u32>()
        }
    }
}

/// The per-launch parameters the factory records. One row per `launchConfigId`, not one
/// per token, so a replay reads this once rather than per launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaunchConfig {
    pub supply: U256,
    pub curve_fee_bps: Bps,
    pub phantom_quote: U256,
    pub graduation_threshold: U256,
}

impl LaunchConfig {
    /// Config id 0, the only one live on 2026-09-07, read from the factory with
    /// `getLaunchConfig(0)` and confirmed against a real launch transaction.
    ///
    /// Written as arithmetic rather than hex limbs so it is readable and obviously right.
    /// It is a **fixture and a fallback, never the source of truth**: the indexer reads
    /// the real config per `launchConfigId` and `doctor` re-verifies it, because spec §2
    /// warns that factory parameters can change.
    /// The config for one launch, from the graduation threshold its `TokenLaunched`
    /// event carried.
    ///
    /// **The phantom reserve is per pair token, not a global constant.** Spec §2 gives
    /// "4.2 ETH real quote against a 1.68 ETH phantom reserve", which is true only of
    /// ETH-paired launches -- measured at just 40% of the universe. The other 60% pair
    /// against 40-odd different tokens, each with its own economics, and applying ETH's
    /// numbers to them makes every replayed price wrong.
    ///
    /// The relationship is fixed even though the values are not: solving the initial
    /// reserve from the first real buy on 400 curves gives
    /// `phantom = graduation_threshold * 2/5` on 368 of them, and 1.68/4.2 is exactly 2/5.
    /// That also keeps reserved supply at `phantom/(phantom+threshold)` = 2/7 = 28.57% for
    /// every pair, matching §2.
    ///
    /// **This is a fallback, not the source of truth.** The protocol stores the phantom
    /// quote per pair token and derives the threshold from it, not the other way round, so
    /// recovering it by dividing is exact only when the threshold happens to be divisible.
    /// Rounding the other way was measured and is worse: replay exactness across real
    /// curves is 98.9% flooring and 83% with a ceiling, so neither rounding is right for
    /// every curve and the division is simply not invertible.
    ///
    /// The real value comes from `pairTokenEconomics(pairToken)`, read once per distinct
    /// pair token. Use [`LaunchConfig::with_phantom`] when it is available; this is for
    /// when it is not.
    pub fn for_threshold(supply: U256, curve_fee_bps: Bps, graduation_threshold: U256) -> Self {
        Self {
            supply,
            curve_fee_bps,
            phantom_quote: graduation_threshold * U256::from(2u64) / U256::from(5u64),
            graduation_threshold,
        }
    }

    /// The exact config, with the phantom quote read from `pairTokenEconomics`.
    pub fn with_phantom(
        supply: U256,
        curve_fee_bps: Bps,
        graduation_threshold: U256,
        phantom_quote: U256,
    ) -> Self {
        Self {
            supply,
            curve_fee_bps,
            phantom_quote,
            graduation_threshold,
        }
    }

    pub fn live_id_0() -> Self {
        let one_eth = U256::from(1_000_000_000_000_000_000u64);
        Self {
            // 1e9 tokens at 18 decimals.
            supply: U256::from(1_000_000_000u64) * one_eth,
            curve_fee_bps: 100,
            // 1.68 ETH
            phantom_quote: one_eth * U256::from(168u64) / U256::from(100u64),
            // 4.2 ETH
            graduation_threshold: one_eth * U256::from(42u64) / U256::from(10u64),
        }
    }
}

/// The opening tax is capped so a buyer always nets at least 1% of the spend.
///
/// Ported verbatim: `BPS - fee - creatorTax - 100`, floored at zero.
pub fn effective_opening_bps(fee_bps: Bps, creator_tax_bps: Bps, opening_tax_bps: Bps) -> Bps {
    if opening_tax_bps == 0 {
        return 0;
    }
    let max = BPS
        .saturating_sub(fee_bps)
        .saturating_sub(creator_tax_bps)
        .saturating_sub(100);
    opening_tax_bps.min(max)
}

/// The result of pricing a buy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuyQuote {
    pub tokens_out: U256,
    /// What is actually spent. Less than the requested input when the fill clamped.
    pub spent: U256,
    pub refund: U256,
    /// fee + creator tax + effective opening tax, for display.
    pub total_input_bps: Bps,
    /// True when the buy was limited by the remaining sellable supply.
    pub clamped: bool,
}

/// Price a buy of `quote_in` against `s`.
///
/// Operation order matches the contract: the three deductions come off the input, the
/// remainder swaps, and only then is the result compared against the sellable supply.
pub fn quote_buy(s: &CurveState, quote_in: U256) -> Result<BuyQuote> {
    let open_bps = effective_opening_bps(s.fee_bps, s.creator_tax_bps, s.opening_tax_bps);
    let total_input_bps = s.fee_bps + s.creator_tax_bps + open_bps;
    if total_input_bps >= BPS {
        return Err(CurveError::InputFullyConsumed {
            fee_bps: s.fee_bps,
            tax_bps: s.creator_tax_bps,
            opening_bps: open_bps,
        });
    }

    let mut spent = quote_in;
    let fee = bps_of(spent, s.fee_bps)?;
    let tax = bps_of(spent, s.creator_tax_bps)?;
    let opening = bps_of(spent, open_bps)?;
    let net = spent
        .checked_sub(fee)
        .and_then(|v| v.checked_sub(tax))
        .and_then(|v| v.checked_sub(opening))
        .ok_or(CurveError::Overflow)?;

    let mut tokens_out = amount_out(net, s.quote_reserve, s.token_reserve)?;
    let mut clamped = false;

    if tokens_out > s.sellable_tokens {
        clamped = true;
        tokens_out = s.sellable_tokens;
        let net_needed = amount_in(s.sellable_tokens, s.quote_reserve, s.token_reserve)?;
        let grossed = ceil_div(
            mul(net_needed, U256::from(BPS))?,
            U256::from(BPS - total_input_bps),
        )?;
        spent = grossed.min(quote_in);
    }

    Ok(BuyQuote {
        tokens_out,
        spent,
        refund: quote_in - spent,
        total_input_bps,
        clamped,
    })
}

/// Quote units received for selling `tokens_in`. Fee and creator tax come off the output.
///
/// This is the function behind a position mark (spec §1): a real sell quote for the whole
/// position, which is why a fresh entry marks below par. That is the round trip, not a
/// loss.
pub fn quote_sell(s: &CurveState, tokens_in: U256) -> Result<U256> {
    let gross = amount_out(tokens_in, s.token_reserve, s.quote_reserve)?;
    let fee = bps_of(gross, s.fee_bps)?;
    let tax = bps_of(gross, s.creator_tax_bps)?;
    gross
        .checked_sub(fee)
        .and_then(|v| v.checked_sub(tax))
        .ok_or(CurveError::Overflow)
}

/// `min_tokens_out` for a given slippage.
///
/// This bounds the *rate*, not the quantity: a clamped fill at the accepted rate still
/// settles, which is why it is derived from the quote rather than from the request.
pub fn min_out_from_rate(quote: U256, slippage_bps: Bps) -> U256 {
    let keep = BPS.saturating_sub(slippage_bps);
    quote.saturating_mul(U256::from(keep)) / U256::from(BPS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eth(whole: u64, wei: u64) -> U256 {
        U256::from(whole) * U256::from(1_000_000_000_000_000_000u64) + U256::from(wei)
    }

    fn fresh() -> CurveState {
        CurveState::opening(&LaunchConfig::live_id_0()).expect("opening state")
    }

    #[test]
    fn live_config_constant_matches_the_factory() {
        let c = LaunchConfig::live_id_0();
        assert_eq!(c.supply, U256::from(10u64).pow(U256::from(27u64)), "1e27");
        assert_eq!(c.phantom_quote, eth(1, 680_000_000_000_000_000), "1.68 ETH");
        assert_eq!(
            c.graduation_threshold,
            eth(4, 200_000_000_000_000_000),
            "4.2 ETH"
        );
        assert_eq!(c.curve_fee_bps, 100);
    }

    /// Ground truth. Launch transaction
    /// `0x928dad6a9910fdfd2e086c877ac5b2c2078872344f2474b32419633979fa2ad8`, curve
    /// `0x912f61576f387e8fc5cd81434fd047c35cd95552`, read 2026-09-07.
    ///
    /// The deployer is exempt from the opening tax on its own launch (the receipt carries
    /// no `SnipeTaxCharged`) and this launch set no creator tax, so only the 1% curve fee
    /// applies.
    #[test]
    fn reproduces_a_real_dev_buy() {
        let s = fresh();
        let quote_in = U256::from(88_421_000_000_000_000u64);
        let q = quote_buy(&s, quote_in).expect("quote");

        assert_eq!(
            q.tokens_out,
            U256::from_str_radix("49524734362106262014495324", 10).unwrap(),
            "tokens_out must match the CurveBuy event exactly, not approximately"
        );
        assert!(!q.clamped);
        assert_eq!(q.spent, quote_in);
        assert_eq!(q.refund, U256::ZERO);
        assert_eq!(q.total_input_bps, 100);
    }

    #[test]
    fn fee_matches_the_real_event() {
        let s = fresh();
        let quote_in = U256::from(88_421_000_000_000_000u64);
        let fee = bps_of(quote_in, s.fee_bps).unwrap();
        assert_eq!(fee, U256::from(884_210_000_000_000u64));
    }

    /// Spec §9 asks for "3.00% of supply from 0.0535 ETH on a fresh curve". Measured
    /// against the real config that input yields 3.056%, not 3.00%; the spec's pairing is
    /// slightly off. The real curve is the authority, so the test asserts what the curve
    /// actually does and records the discrepancy.
    #[test]
    fn spec_dev_buy_example_yields_3_056_percent_not_3_00() {
        let s = fresh();
        let q = quote_buy(&s, U256::from(53_500_000_000_000_000u64)).unwrap();
        let share_bps = (q.tokens_out * U256::from(BPS)) / LaunchConfig::live_id_0().supply;
        assert_eq!(
            share_bps.to::<u32>(),
            305,
            "3.05% (3.056), not the spec's 3.00%"
        );
    }

    #[test]
    fn a_read_phantom_overrides_the_derived_one() {
        // Deriving is a fallback: the division is not invertible, and one wei of error in
        // the initial reserve makes every replayed buy on that curve wrong.
        let threshold = U256::from(42_347_152_428_810_721_502u128);
        let derived = LaunchConfig::for_threshold(LaunchConfig::live_id_0().supply, 100, threshold);
        let exact = LaunchConfig::with_phantom(
            LaunchConfig::live_id_0().supply,
            100,
            threshold,
            U256::from(16_938_860_971_524_288_601u128),
        );
        assert_ne!(derived.phantom_quote, exact.phantom_quote);
        assert_eq!(exact.graduation_threshold, derived.graduation_threshold);
    }

    #[test]
    fn the_phantom_reserve_follows_the_pair_tokens_threshold() {
        // ETH: the spec's 1.68 against 4.2.
        let eth = LaunchConfig::for_threshold(
            LaunchConfig::live_id_0().supply,
            100,
            eth(4, 200_000_000_000_000_000),
        );
        assert_eq!(
            eth.phantom_quote,
            super::LaunchConfig::live_id_0().phantom_quote
        );

        // A non-ETH pair seen in the indexed window: threshold 41.6, so phantom 16.64.
        let other = LaunchConfig::for_threshold(
            LaunchConfig::live_id_0().supply,
            100,
            U256::from(41_600_000_000_000_000_000u128),
        );
        assert_eq!(
            other.phantom_quote,
            U256::from(16_640_000_000_000_000_000u128)
        );

        // And a six-decimal pair, where using ETH's constants would be wildly wrong.
        let small = LaunchConfig::for_threshold(
            LaunchConfig::live_id_0().supply,
            100,
            U256::from(8_090_000_000u64),
        );
        assert_eq!(small.phantom_quote, U256::from(3_236_000_000u64));
    }

    #[test]
    fn reserved_supply_stays_two_sevenths_for_every_pair() {
        // 28.57% of spec §2 is a consequence of the 2/5 ratio, so it holds for every pair
        // token rather than only for ETH.
        for threshold in [
            eth(4, 200_000_000_000_000_000),
            U256::from(41_600_000_000_000_000_000u128),
            U256::from(8_090_000_000u64),
        ] {
            let cfg = LaunchConfig::for_threshold(LaunchConfig::live_id_0().supply, 100, threshold);
            let s = CurveState::opening(&cfg).unwrap();
            let bps = (s.reserved_tokens * U256::from(BPS) / cfg.supply).to::<u32>();
            assert_eq!(bps, 2857, "threshold {threshold}");
        }
    }

    #[test]
    fn reserved_supply_is_two_sevenths() {
        let s = fresh();
        let supply = LaunchConfig::live_id_0().supply;
        assert_eq!(
            s.reserved_tokens,
            supply * U256::from(2u64) / U256::from(7u64)
        );
        assert_eq!(s.sellable_tokens, supply - s.reserved_tokens);
        // 28.57% of spec §2.
        assert_eq!(
            (s.reserved_tokens * U256::from(BPS) / supply).to::<u32>(),
            2857
        );
    }

    #[test]
    fn a_round_trip_loses_more_than_the_fee() {
        let mut s = fresh();
        let spend = eth(0, 10_000_000_000_000_000); // 0.01 ETH
        let q = quote_buy(&s, spend).unwrap();

        // Apply the buy so the sell is quoted against the moved curve, as it would be in
        // reality. Price impact is part of the round trip.
        let fee = bps_of(spend, s.fee_bps).unwrap();
        s.apply_buy(spend, q.tokens_out, fee, U256::ZERO).unwrap();

        let back = quote_sell(&s, q.tokens_out).unwrap();
        assert!(back < spend, "a round trip must lose money");

        let fee_only = spend * U256::from(s.fee_bps) / U256::from(BPS);
        let lost = spend - back;
        assert!(
            lost > fee_only,
            "loss {lost} must exceed the one-way fee {fee_only}: there are two fees plus impact"
        );
    }

    #[test]
    fn opening_tax_is_capped_so_a_buyer_always_nets_one_percent() {
        // 99% opening tax with a 1% fee and 2% creator tax cannot all be charged.
        let capped = effective_opening_bps(100, 200, 9900);
        assert_eq!(capped, 9600, "10000 - 100 - 200 - 100");

        // Below the cap it passes through untouched.
        assert_eq!(effective_opening_bps(100, 200, 300), 300);
        // Zero stays zero rather than becoming the cap.
        assert_eq!(effective_opening_bps(100, 200, 0), 0);
    }

    #[test]
    fn opening_tax_at_full_draw_takes_almost_everything() {
        let mut s = fresh();
        s.opening_tax_bps = 9900;
        let spend = eth(0, 10_000_000_000_000_000);
        let q = quote_buy(&s, spend).unwrap();

        let clean = quote_buy(&fresh(), spend).unwrap();
        // This is why the sniper waits: racing block one buys ~1% of what waiting buys.
        assert!(
            q.tokens_out * U256::from(50u64) < clean.tokens_out,
            "buying at full draw must cost more than 50x"
        );
        // 100 fee + 0 creator tax + 9800 capped opening.
        assert_eq!(q.total_input_bps, 9_900);
    }

    #[test]
    fn a_fill_larger_than_the_sellable_supply_clamps_and_refunds() {
        let s = fresh();
        // Far more than the 4.2 ETH graduation threshold.
        let q = quote_buy(&s, eth(1_000, 0)).unwrap();
        assert!(q.clamped);
        assert_eq!(q.tokens_out, s.sellable_tokens, "cannot buy past sellable");
        assert!(q.spent < eth(1_000, 0));
        assert_eq!(q.refund, eth(1_000, 0) - q.spent);
        // The grossed spend must cover the net the curve needs.
        assert!(q.spent > U256::ZERO);
    }

    #[test]
    fn clamped_spend_is_grossed_up_not_down() {
        let s = fresh();
        let q = quote_buy(&s, eth(1_000, 0)).unwrap();
        // Re-deriving the net from the grossed spend must still reach the target, which
        // is the point of the ceiling division.
        let net = q.spent - (q.spent * U256::from(s.fee_bps) / U256::from(BPS));
        let reachable = amount_out(net, s.quote_reserve, s.token_reserve).unwrap();
        assert!(
            reachable >= s.sellable_tokens,
            "ceil_div must round up so the clamped fill is actually payable"
        );
    }

    #[test]
    fn min_out_bounds_the_rate() {
        let q = U256::from(1_000_000u64);
        assert_eq!(min_out_from_rate(q, 300), U256::from(970_000u64));
        assert_eq!(min_out_from_rate(q, 0), q);
        assert_eq!(min_out_from_rate(q, BPS), U256::ZERO);
    }

    #[test]
    fn amount_in_rounds_in_the_pools_favour() {
        let out = U256::from(1_000u64);
        let r_in = U256::from(1_000_000u64);
        let r_out = U256::from(1_000_000u64);
        let need = amount_in(out, r_in, r_out).unwrap();
        let got = amount_out(need, r_in, r_out).unwrap();
        assert!(got >= out, "the +1 must guarantee the output is reachable");
    }

    #[test]
    fn amount_in_refuses_to_drain_the_reserve() {
        let r_out = U256::from(1_000u64);
        assert!(matches!(
            amount_in(r_out, U256::from(1_000u64), r_out),
            Err(CurveError::ReserveExhausted { .. })
        ));
    }

    #[test]
    fn replay_reproduces_the_next_quote() {
        // Two buys in sequence: applying the first must leave the curve in the state that
        // prices the second. This is the property phase 3's reconstruction depends on.
        let mut s = fresh();
        let spend = eth(0, 50_000_000_000_000_000);
        let q1 = quote_buy(&s, spend).unwrap();
        let fee1 = bps_of(spend, s.fee_bps).unwrap();
        s.apply_buy(spend, q1.tokens_out, fee1, U256::ZERO).unwrap();

        let q2 = quote_buy(&s, spend).unwrap();
        assert!(
            q2.tokens_out < q1.tokens_out,
            "the same spend must buy fewer tokens after the curve moves"
        );
        assert_eq!(s.real_quote_reserve, spend - fee1);
        assert_eq!(
            s.quote_reserve,
            LaunchConfig::live_id_0().phantom_quote + (spend - fee1)
        );
    }

    #[test]
    fn sell_returns_the_curve_toward_its_prior_state() {
        let mut s = fresh();
        let before = s;
        let spend = eth(0, 50_000_000_000_000_000);
        let q = quote_buy(&s, spend).unwrap();
        let fee = bps_of(spend, s.fee_bps).unwrap();
        s.apply_buy(spend, q.tokens_out, fee, U256::ZERO).unwrap();

        let out = quote_sell(&s, q.tokens_out).unwrap();
        let gross = amount_out(q.tokens_out, s.token_reserve, s.quote_reserve).unwrap();
        let sell_fee = bps_of(gross, s.fee_bps).unwrap();
        s.apply_sell(q.tokens_out, out, sell_fee, U256::ZERO)
            .unwrap();

        assert_eq!(s.token_reserve, before.token_reserve, "tokens all returned");
        // The quote reserve keeps both fees, so it stays above where it started.
        assert!(s.quote_reserve > before.quote_reserve);
    }

    #[test]
    fn progress_is_integer_bps_and_caps_at_full() {
        let mut s = fresh();
        assert_eq!(s.progress_bps(), 0);
        s.real_quote_reserve = s.graduation_threshold / U256::from(2u64);
        assert_eq!(s.progress_bps(), 5_000);
        s.real_quote_reserve = s.graduation_threshold * U256::from(3u64);
        assert_eq!(s.progress_bps(), BPS, "must cap, never exceed 100%");
    }

    #[test]
    fn fully_consumed_input_is_an_error_not_a_silent_zero() {
        let mut s = fresh();
        s.creator_tax_bps = 9_900;
        s.opening_tax_bps = 0;
        // fee 100 + tax 9900 = 10000
        assert!(matches!(
            quote_buy(&s, eth(0, 1_000_000)),
            Err(CurveError::InputFullyConsumed { .. })
        ));
    }

    #[test]
    fn zero_input_is_zero_output_not_a_panic() {
        let s = fresh();
        let q = quote_buy(&s, U256::ZERO).unwrap();
        assert_eq!(q.tokens_out, U256::ZERO);
        assert_eq!(q.spent, U256::ZERO);
        assert_eq!(quote_sell(&s, U256::ZERO).unwrap(), U256::ZERO);
    }
}
