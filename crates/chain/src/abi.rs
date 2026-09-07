//! Typed contract bindings via `sol!`.
//!
//! Sources: bodkin's `src/abi/pons.ts` and `src/abi/uniswap.ts` (MIT), which took them from
//! the verified `PonsV2LaunchFactory` / `V2FeeEscrow` / `PonsV2LaunchAndBuy` ABIs on
//! Blockscout and from `contractsV2/src/v2` in `ponsdotdev/ponsfamily` (the curve is not
//! verified on Blockscout, so its ABI comes from the repository).
//!
//! Everything here is re-verified live by `doctor --probe`, because spec §2 warns that
//! factory parameters can change and these were measured on a particular day.
//!
//! # One improvement over the reference
//!
//! bodkin carries `SnipeTaxCharged` as a bare topic hash with no signature, so it can
//! count those logs but not read them. Brute-forcing the signature against the observed
//! hash `0x3bc39a55…` gives an exact match for **`SnipeTaxCharged(address,uint256)`**, so
//! here it is a real typed event and the tax amount is readable.
//!
//! That matters more than it sounds: this event is what identifies the end of the opening
//! tax window without any block timestamps, which is how `entry_price` is determined
//! (PLAN.md F2).

use alloy_sol_types::sol;

sol! {
    /// `PonsV2LaunchFactory`.
    #[derive(Debug)]
    interface IPonsFactory {
        struct LaunchedToken {
            address token;
            address curve;
            address deployer;
            address creatorFeeRecipient;
            address pairToken;
            uint256 graduationThreshold;
            uint24 poolFee;
            int24 tickSpacing;
            uint16 creatorTaxBps;
            bool buybackEnabled;
            uint8 phase;
            uint256 sweptQuote;
            uint256 sweptTokens;
            uint256 sweptAt;
            bool exists;
        }

        struct LaunchConfig {
            uint256 supply;
            uint256 curveFeeBps;
            uint256 phantomQuote;
            uint256 graduationThreshold;
            uint24 poolFee;
            int24 tickSpacing;
            bool enabled;
        }

        struct Socials {
            string twitter;
            string telegram;
            string discord;
            string website;
            string farcaster;
        }

        struct TokenParams {
            string name;
            string symbol;
            string logo;
            string description;
            Socials socials;
            address creatorFeeRecipient;
            uint16 creatorTaxBps;
            bool buybackEnabled;
            bytes32 expectedEconomics;
            bytes32 salt;
        }

        function getLaunchedToken(address token) external view returns (LaunchedToken);
        function getLaunchConfig(uint256 id) external view returns (LaunchConfig);
        function pairTokenEconomics(address pairToken) external view
            returns (uint256 phantomQuote, uint256 graduationThreshold, uint8 decimals);
        function snipeTaxStartBps() external view returns (uint256);
        function snipeTaxSeconds() external view returns (uint256);
        function launchFee() external view returns (uint256);
        function maxCreatorTaxBps() external view returns (uint256);
        function launchEnabled() external view returns (bool);
        function feeEscrow() external view returns (address);
        function memeHook() external view returns (address);
        function poolManager() external view returns (address);
        function launchDeployer() external view returns (address);

        event TokenLaunched(
            address indexed token,
            address indexed curve,
            address indexed deployer,
            address pairToken,
            uint256 launchConfigId,
            uint256 graduationThreshold
        );
        event PoolGraduated(
            address indexed token,
            uint256 positionId,
            uint256 tokenAmount,
            uint256 pairTokenAmount
        );
        event LaunchSwept(address indexed token, uint256 quoteOut, uint256 tokenOut);
        event CreatorFeeRecipientUpdated(
            address indexed token,
            address indexed previousRecipient,
            address indexed newRecipient
        );
    }
}

sol! {
    /// `PonsV2BondingCurve`. One contract per token, which is why trade logs must be
    /// fetched by topic across all addresses and matched client-side.
    #[derive(Debug)]
    interface IPonsCurve {
        function buy(uint256 quoteIn, uint256 minTokensOut, address recipient)
            external payable returns (uint256 tokensOut);
        function sell(uint256 tokensIn, uint256 minQuoteOut, address recipient)
            external returns (uint256 quoteOut);

        function getReserves() external view returns (uint256 quoteReserve, uint256 tokenReserve);
        function realQuoteReserve() external view returns (uint256);
        function sellableTokens() external view returns (uint256);
        function reservedTokens() external view returns (uint256);
        function graduationThreshold() external view returns (uint256);
        function readyToGraduate() external view returns (bool);
        function graduated() external view returns (bool);
        function feeBps() external view returns (uint256);
        function creatorTaxBps() external view returns (uint256);
        function isNativeQuote() external view returns (bool);
        function pairToken() external view returns (address);
        function launchedAt() external view returns (uint256);
        function snipeTaxExempt(address account) external view returns (bool);
        /// The opening tax **for a specific recipient**. An exempt wallet reads zero.
        function currentSnipeTaxBps(address recipient) external view returns (uint256);

        event CurveBuy(
            address indexed buyer,
            address indexed recipient,
            uint256 quoteIn,
            uint256 tokensOut,
            uint256 fee,
            uint256 tax
        );
        event CurveSell(
            address indexed seller,
            address indexed recipient,
            uint256 tokensIn,
            uint256 quoteOut,
            uint256 fee,
            uint256 tax
        );
        event CurveBuyRefunded(address indexed recipient, uint256 refundAmount);
        event CurveCompleted();

        /// Signature recovered by matching keccak against the observed topic0
        /// `0x3bc39a5562b28f5fe8f36cecabfbaa12bb969acf05717994709225fc412a9934`.
        ///
        /// Its **presence or absence** on a `CurveBuy` is what marks the end of the
        /// opening-tax window, and therefore what determines `entry_price` without
        /// needing block timestamps (PLAN.md F2).
        event SnipeTaxCharged(address indexed payer, uint256 amount);
    }
}

sol! {
    /// The three-argument `launchToken`, selector `0xf35abbcf`.
    ///
    /// Its own interface rather than an overload beside the four-argument form: `sol!`
    /// would then generate suffixed names, and two plainly-named calls are easier to read
    /// at the call site than `launchToken_0Call`.
    #[derive(Debug)]
    interface IPonsFactoryNoExempt {
        struct Socials {
            string twitter;
            string telegram;
            string discord;
            string website;
            string farcaster;
        }

        struct TokenParams {
            string name;
            string symbol;
            string logo;
            string description;
            Socials socials;
            address creatorFeeRecipient;
            uint16 creatorTaxBps;
            bool buybackEnabled;
            bytes32 expectedEconomics;
            bytes32 salt;
        }

        function launchToken(
            TokenParams params,
            uint256 launchConfigId,
            address pairToken
        ) external payable returns (address token, address curve);
    }
}

sol! {
    /// The pons token. `getTokenInfo` returns **current** state, so it is used only for
    /// live display and never as a filter input -- see `docs/FINDINGS.md` §4.
    #[derive(Debug)]
    interface IPonsToken {
        struct Socials {
            string twitter;
            string telegram;
            string discord;
            string website;
            string farcaster;
        }

        function getTokenInfo() external view
            returns (address tokenDeployer, string tokenLogo, string tokenDescription, Socials tokenSocials);
        function name() external view returns (string);
        function symbol() external view returns (string);
        function decimals() external view returns (uint8);
        function totalSupply() external view returns (uint256);
        function balanceOf(address owner) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
        function approve(address spender, uint256 amount) external returns (bool);
    }
}

sol! {
    /// `PonsV2LaunchAndBuy`. **This is the point-in-time source of truth for launch
    /// metadata**: `TokenParams` carries name, symbol, logo, description and socials, all
    /// fixed by the transaction that created the token.
    #[derive(Debug)]
    interface IPonsRouter {
        struct Socials {
            string twitter;
            string telegram;
            string discord;
            string website;
            string farcaster;
        }

        struct TokenParams {
            string name;
            string symbol;
            string logo;
            string description;
            Socials socials;
            address creatorFeeRecipient;
            uint16 creatorTaxBps;
            bool buybackEnabled;
            bytes32 expectedEconomics;
            bytes32 salt;
        }

        function launchAndBuy(
            TokenParams params,
            uint256 launchConfigId,
            address pairToken,
            uint256 quoteIn,
            uint256 minTokensOut,
            address recipient,
            address[] snipeTaxExemptions
        ) external payable returns (address token, address curve, uint256 tokensOut);

        /// Launch without an opening buy. Sent to the **factory** address, not the router;
        /// it is declared here because it takes the identical `TokenParams`, and `sol!`
        /// types are per-interface.
        ///
        /// Not in the reference implementation's ABI, and measured to be the *dominant*
        /// route: 47% of launches in a 20,000-block sample, against 35% for
        /// `launchAndBuy`. Recovered by matching keccak against the observed selector
        /// `0xa72101af` after the calldata layout showed the same `TokenParams` followed by
        /// a config id, an address and a dynamic array.
        ///
        /// Decoding it matters because it carries the same point-in-time metadata: without
        /// it, two launches in three would have `socials = Unknown` for a reason about our
        /// decoder rather than about the token.
        function launchToken(
            TokenParams params,
            uint256 launchConfigId,
            address pairToken,
            address[] snipeTaxExemptions
        ) external payable returns (address token, address curve);
    }
}

sol! {
    /// `V2FeeEscrow`.
    #[derive(Debug)]
    interface IFeeEscrow {
        function balanceOf(address recipient) external view returns (uint256);
        function claim() external returns (uint256 amount);
        event Credited(address indexed recipient, address indexed depositor, uint256 amount);
        event Claimed(address indexed recipient, uint256 amount);
    }
}

sol! {
    /// Canonical Multicall3. `aggregate3` with `allowFailure` is one `eth_call` from the
    /// endpoint's point of view, which is the only kind of batching the public RPC accepts
    /// (spec §1: no JSON-RPC batching, because every call inside a batch array is counted).
    #[derive(Debug)]
    interface IMulticall3 {
        struct Call3 {
            address target;
            bool allowFailure;
            bytes callData;
        }
        struct Result {
            bool success;
            bytes returnData;
        }
        function aggregate3(Call3[] calls) external payable returns (Result[] returnData);
    }
}

sol! {
    /// Uniswap v4 pieces a graduated token trades through.
    #[derive(Debug)]
    interface IV4 {
        struct PoolKey {
            address currency0;
            address currency1;
            uint24 fee;
            int24 tickSpacing;
            address hooks;
        }
        struct QuoteExactSingleParams {
            PoolKey poolKey;
            bool zeroForOne;
            uint128 exactAmount;
            bytes hookData;
        }
        function quoteExactInputSingle(QuoteExactSingleParams params)
            external returns (uint256 amountOut, uint256 gasEstimate);
        function getSlot0(bytes32 poolId) external view
            returns (uint160 sqrtPriceX96, int24 tick, uint24 protocolFee, uint24 lpFee);
        function getLiquidity(bytes32 poolId) external view returns (uint128 liquidity);
    }
}

/// Graduation phase, as the factory records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Trading on the bonding curve.
    Curve = 0,
    /// Swept, pool not yet created. **Trading is halted here** and must be refused rather
    /// than guessed at (spec §7).
    Swept = 1,
    /// Trading in the Uniswap v4 pool.
    Pool = 2,
    Rescued = 3,
}

impl Phase {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Phase::Curve),
            1 => Some(Phase::Swept),
            2 => Some(Phase::Pool),
            3 => Some(Phase::Rescued),
            _ => None,
        }
    }

    /// Whether a token can be traded at all in this phase.
    pub fn is_tradeable(self) -> bool {
        matches!(self, Phase::Curve | Phase::Pool)
    }

    pub fn label(self) -> &'static str {
        match self {
            Phase::Curve => "curve",
            Phase::Swept => "swept",
            Phase::Pool => "pool",
            Phase::Rescued => "rescued",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::hex;
    use alloy_sol_types::{SolCall, SolEvent};

    /// Every one of these was observed on-chain on 2026-09-07. If a binding drifts, these
    /// fail rather than the indexer silently matching nothing.
    #[test]
    fn event_topics_match_the_hashes_seen_on_chain() {
        assert_eq!(
            hex::encode_prefixed(IPonsFactory::TokenLaunched::SIGNATURE_HASH),
            "0x8d4aad4953d0ca700d468f3753aa14432d1b35b43ec6409f051fb6aa43a89607"
        );
        assert_eq!(
            hex::encode_prefixed(IPonsCurve::CurveBuy::SIGNATURE_HASH),
            "0xec36bf571f136799e8dc0b0b8bea4b04d8bd3d43de838aab0d5fc21d4cbfc455"
        );
        assert_eq!(
            hex::encode_prefixed(IPonsCurve::CurveSell::SIGNATURE_HASH),
            "0x8113d738abdcb6b38357e9d53a54a7157861a09031b453651f0fe7fe151f59df"
        );
        assert_eq!(
            hex::encode_prefixed(IPonsFactory::PoolGraduated::SIGNATURE_HASH),
            "0x0a44ef75df69c534f43cd6c1aa3ef8983065fe5fe79ef9e79f6494e6f258c259"
        );
        assert_eq!(
            hex::encode_prefixed(IPonsFactory::LaunchSwept::SIGNATURE_HASH),
            "0xcdb72f157fd3666758a6ce201387ffb52038c7562e4fff352828da1096c4b6b4"
        );
    }

    /// The recovered signature. bodkin only had the hash; matching it proves the shape,
    /// which is what makes the tax amount readable rather than merely countable.
    #[test]
    fn snipe_tax_charged_signature_matches_the_observed_topic() {
        assert_eq!(
            hex::encode_prefixed(IPonsCurve::SnipeTaxCharged::SIGNATURE_HASH),
            "0x3bc39a5562b28f5fe8f36cecabfbaa12bb969acf05717994709225fc412a9934",
            "SnipeTaxCharged(address,uint256) must hash to the topic0 seen on chain"
        );
    }

    #[test]
    fn launch_and_buy_selector_matches_a_real_launch_transaction() {
        // Observed on tx 0x928dad6a9910fdfd2e086c877ac5b2c2078872344f2474b32419633979fa2ad8.
        assert_eq!(
            hex::encode_prefixed(IPonsRouter::launchAndBuyCall::SELECTOR),
            "0xf85f8e41"
        );
    }

    #[test]
    fn getlaunchconfig_selector_matches_what_was_probed() {
        assert_eq!(
            hex::encode_prefixed(IPonsFactory::getLaunchConfigCall::SELECTOR),
            "0x1cad862d"
        );
    }

    #[test]
    fn the_swept_phase_is_not_tradeable() {
        // Spec §7: refuse during the swept gap rather than guessing the venue.
        assert!(!Phase::Swept.is_tradeable());
        assert!(!Phase::Rescued.is_tradeable());
        assert!(Phase::Curve.is_tradeable());
        assert!(Phase::Pool.is_tradeable());
    }

    #[test]
    fn phase_round_trips_from_the_factory_encoding() {
        for (v, p) in [
            (0u8, Phase::Curve),
            (1, Phase::Swept),
            (2, Phase::Pool),
            (3, Phase::Rescued),
        ] {
            assert_eq!(Phase::from_u8(v), Some(p));
        }
        assert_eq!(
            Phase::from_u8(9),
            None,
            "an unknown phase must not be guessed"
        );
    }
}
