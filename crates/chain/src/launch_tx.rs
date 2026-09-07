//! Decoding the launch transaction — the point-in-time source of truth.
//!
//! This module exists because of spec §5.3's verification task, and it is the reason
//! `require_twitter` can honestly ship as a filter rule.
//!
//! The reference implementation reads socials from `getTokenInfo()` on the token contract,
//! which returns **current** state. A token that added a Twitter link an hour after launch
//! would look like it had one at launch, silently inflating every backtest that used the
//! default rule. The launch transaction's calldata does not have that problem: it is the
//! transaction that created the token, so whatever it declares was true at `launch_block`
//! by construction.
//!
//! Verified on real data — see `docs/FINDINGS.md` §4 and the fixture test below, which
//! decodes an actual mainnet launch.
//!
//! # When it does not decode
//!
//! A launch through a different router, a direct factory call or a bundler contract will
//! not match the `launchAndBuy` selector. Those become [`LaunchMeta::Undecodable`], whose
//! socials are [`Presence::Unknown`] rather than `Absent`. That distinction is the whole
//! point: treating "we could not read this" as "this token has no Twitter" is a lie in
//! precisely the direction that flatters a backtest.

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolCall;
use quarrel_core::features::{Presence, Socials};

use crate::abi::{IPonsRouter, IPonsToken};

/// What the launch transaction declared. All of it point-in-time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchCalldata {
    pub name: String,
    pub symbol: String,
    /// An IPFS or HTTP URL chosen by the deployer.
    ///
    /// **Never fetched.** Rendering it would tell the deployer the IP address of everyone
    /// watching their launch, in real time, before those people buy (spec §3.1, PLAN.md
    /// C1). It is stored and displayed as text only.
    pub logo: String,
    pub description: String,
    pub socials: Socials,
    /// The raw social strings, for display in the detail drawer.
    pub social_urls: SocialUrls,
    pub creator_fee_recipient: Address,
    pub creator_tax_bps: u32,
    /// Wallets declared exempt from the opening tax: the declared bundle.
    pub exempt_wallets: Vec<Address>,
    /// What the launcher asked to spend on its own first buy.
    ///
    /// The `CurveBuy` log is the ground truth for what it actually spent; this is the
    /// intent. They differ when a fill clamps.
    pub declared_quote_in: U256,
    pub recipient: Address,
    pub launch_config_id: U256,
    pub pair_token: Address,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SocialUrls {
    pub twitter: String,
    pub telegram: String,
    pub discord: String,
    pub website: String,
    pub farcaster: String,
}

/// The result of trying to read a launch transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchMeta {
    Decoded(Box<LaunchCalldata>),
    /// The transaction did not go through `launchAndBuy`.
    Undecodable {
        /// The four-byte selector actually seen, for diagnosing new launch routes.
        selector: [u8; 4],
        reason: String,
    },
}

impl LaunchMeta {
    /// Socials as a filter may see them. Unknown when the launch did not decode.
    pub fn socials(&self) -> Socials {
        match self {
            LaunchMeta::Decoded(c) => c.socials,
            LaunchMeta::Undecodable { .. } => Socials::UNKNOWN,
        }
    }

    /// Declared exempt wallets, or `None` when unreadable.
    ///
    /// `None` rather than zero: an undecodable launch has an unknown bundle size, and
    /// recording it as zero would let `max_exempt_wallets` pass a launch it never saw.
    pub fn exempt_wallets(&self) -> Option<u32> {
        match self {
            LaunchMeta::Decoded(c) => Some(c.exempt_wallets.len() as u32),
            LaunchMeta::Undecodable { .. } => None,
        }
    }

    pub fn name(&self) -> Option<&str> {
        match self {
            LaunchMeta::Decoded(c) => Some(&c.name),
            LaunchMeta::Undecodable { .. } => None,
        }
    }

    pub fn symbol(&self) -> Option<&str> {
        match self {
            LaunchMeta::Decoded(c) => Some(&c.symbol),
            LaunchMeta::Undecodable { .. } => None,
        }
    }

    pub fn is_decoded(&self) -> bool {
        matches!(self, LaunchMeta::Decoded(_))
    }
}

fn presence(s: &str) -> Presence {
    // A declared-but-empty string is a real absence: the launcher had the field and left
    // it blank. Only a launch we could not read at all is Unknown.
    Presence::from_str_field(s)
}

/// Decode a launch transaction's calldata.
///
/// Never fails: an unrecognised shape becomes [`LaunchMeta::Undecodable`], because the
/// indexer must record every launch and a panic or an error would drop one silently.
pub fn decode_launch(input: &[u8]) -> LaunchMeta {
    if input.len() < 4 {
        return LaunchMeta::Undecodable {
            selector: [0; 4],
            reason: format!(
                "calldata is {} bytes, too short for a selector",
                input.len()
            ),
        };
    }
    let mut selector = [0u8; 4];
    selector.copy_from_slice(&input[..4]);

    if selector != IPonsRouter::launchAndBuyCall::SELECTOR {
        return LaunchMeta::Undecodable {
            selector,
            reason: format!(
                "selector 0x{} is not launchAndBuy (0x{}); launched through another route",
                alloy_primitives::hex::encode(selector),
                alloy_primitives::hex::encode(IPonsRouter::launchAndBuyCall::SELECTOR)
            ),
        };
    }

    let call = match IPonsRouter::launchAndBuyCall::abi_decode(input) {
        Ok(c) => c,
        Err(e) => {
            return LaunchMeta::Undecodable {
                selector,
                reason: format!("selector matched but the arguments did not decode: {e}"),
            };
        }
    };

    let p = call.params;
    let s = p.socials;
    let urls = SocialUrls {
        twitter: s.twitter.clone(),
        telegram: s.telegram.clone(),
        discord: s.discord.clone(),
        website: s.website.clone(),
        farcaster: s.farcaster.clone(),
    };

    LaunchMeta::Decoded(Box::new(LaunchCalldata {
        socials: Socials {
            twitter: presence(&urls.twitter),
            website: presence(&urls.website),
            telegram: presence(&urls.telegram),
        },
        name: p.name,
        symbol: p.symbol,
        logo: p.logo,
        description: p.description,
        social_urls: urls,
        creator_fee_recipient: p.creatorFeeRecipient,
        creator_tax_bps: p.creatorTaxBps as u32,
        exempt_wallets: call.snipeTaxExemptions,
        declared_quote_in: call.quoteIn,
        recipient: call.recipient,
        launch_config_id: call.launchConfigId,
        pair_token: call.pairToken,
    }))
}

/// Make an attacker-chosen string safe to put on a screen.
///
/// Token names, symbols and descriptions are arbitrary Unicode written by whoever launched
/// the token. The real launch used as a fixture here has an emoji in its symbol, which is
/// harmless -- but the same field can carry things that are not:
///
/// * **Bidirectional overrides** (U+202A..U+202E, U+2066..U+2069) reorder the text around
///   them, so a symbol can render as something other than what it is.
/// * **Control characters** can break a terminal or a log line.
/// * **Zero-width characters** let two visually identical tokens carry different strings,
///   which is how a copycat launch impersonates a real one.
///
/// This does not attempt full homograph defence -- that belongs with the anti-rug work of
/// phase 7. It removes the categories that can misrepresent what the user is looking at.
pub fn sanitise_for_display(s: &str) -> String {
    s.chars()
        .filter(|c| {
            let n = *c as u32;
            let is_bidi = (0x202A..=0x202E).contains(&n) || (0x2066..=0x2069).contains(&n);
            let is_zero_width = matches!(n, 0x200B..=0x200F | 0xFEFF);
            !(is_bidi || is_zero_width || c.is_control())
        })
        .collect()
}

/// Decode `getTokenInfo()` returndata.
///
/// **Current state, never point-in-time.** Available for the live detail drawer, where it
/// is labelled as a current reading. It must never reach a filter; that is what
/// [`decode_launch`] is for.
pub fn decode_current_token_info(data: &[u8]) -> Option<(String, String, SocialUrls)> {
    let r = IPonsToken::getTokenInfoCall::abi_decode_returns(data).ok()?;
    Some((
        r.tokenLogo,
        r.tokenDescription,
        SocialUrls {
            twitter: r.tokenSocials.twitter,
            telegram: r.tokenSocials.telegram,
            discord: r.tokenSocials.discord,
            website: r.tokenSocials.website,
            farcaster: r.tokenSocials.farcaster,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::hex;

    /// Real mainnet launch, transaction
    /// `0x928dad6a9910fdfd2e086c877ac5b2c2078872344f2474b32419633979fa2ad8`, captured
    /// 2026-09-07. This is the fixture that settles spec §5.3.
    fn real_launch_calldata() -> Vec<u8> {
        let raw = include_str!("../tests/fixtures/launch_calldata.hex");
        hex::decode(raw.trim()).expect("fixture is valid hex")
    }

    #[test]
    fn a_real_launch_decodes_and_its_socials_are_point_in_time() {
        let meta = decode_launch(&real_launch_calldata());
        let LaunchMeta::Decoded(c) = meta else {
            panic!("the real launch must decode: {meta:?}");
        };

        assert_eq!(c.name, "SpaceWaffle");
        // The real symbol carries an emoji. Token names and symbols are arbitrary
        // attacker-chosen Unicode, which an ASCII-only reading of the calldata hides.
        assert_eq!(c.symbol, "W\u{1f170}\u{fe0f}FFLE");

        // The answer to the §5.3 verification task: these came out of the transaction
        // that created the token, not out of current contract state.
        assert!(
            c.social_urls.twitter.contains("x.com"),
            "twitter url: {:?}",
            c.social_urls.twitter
        );
        assert!(
            c.social_urls.website.starts_with("https://"),
            "website url: {:?}",
            c.social_urls.website
        );
        assert_eq!(c.socials.twitter, Presence::Present);
        assert_eq!(c.socials.website, Presence::Present);
        assert_eq!(
            c.socials.telegram,
            Presence::Absent,
            "declared but blank is a real absence, not Unknown"
        );

        assert!(!c.description.is_empty());
        assert_eq!(c.launch_config_id, U256::ZERO);
        assert!(c.pair_token.is_zero(), "native ETH pair");
        assert_eq!(c.declared_quote_in, U256::from(88_421_000_000_000_000u64));
    }

    #[test]
    fn the_real_launch_declares_no_exempt_wallets() {
        let LaunchMeta::Decoded(c) = decode_launch(&real_launch_calldata()) else {
            panic!("must decode");
        };
        assert_eq!(
            c.exempt_wallets.len(),
            0,
            "no declared bundle on this launch"
        );
        assert_eq!(c.creator_tax_bps, 0, "matches the CurveBuy tax of 0");
    }

    #[test]
    fn an_unknown_route_is_unknown_not_absent() {
        // This is the failure mode that would silently inflate every backtest: a launch
        // we cannot read must never be recorded as "has no Twitter".
        let mut input = vec![0xde, 0xad, 0xbe, 0xef];
        input.extend_from_slice(&[0u8; 128]);

        let meta = decode_launch(&input);
        assert!(!meta.is_decoded());
        assert_eq!(meta.socials(), Socials::UNKNOWN);
        assert_ne!(
            meta.socials(),
            Socials::NONE,
            "Unknown and Absent must never collapse into each other"
        );
        assert_eq!(
            meta.exempt_wallets(),
            None,
            "an unreadable bundle size must not read as zero"
        );

        let LaunchMeta::Undecodable { selector, reason } = meta else {
            unreachable!()
        };
        assert_eq!(selector, [0xde, 0xad, 0xbe, 0xef]);
        assert!(reason.contains("another route"), "{reason}");
    }

    #[test]
    fn truncated_calldata_degrades_instead_of_panicking() {
        // The indexer must record every launch; a panic here would drop one silently.
        for input in [vec![], vec![0x01], vec![0xf8, 0x5f, 0x8e]] {
            let meta = decode_launch(&input);
            assert!(!meta.is_decoded());
            assert_eq!(meta.socials(), Socials::UNKNOWN);
        }
    }

    #[test]
    fn the_right_selector_with_wrong_arguments_is_undecodable_not_a_panic() {
        let mut input = IPonsRouter::launchAndBuyCall::SELECTOR.to_vec();
        input.extend_from_slice(&[0xAAu8; 64]); // nonsense body
        let meta = decode_launch(&input);
        assert!(!meta.is_decoded());
        let LaunchMeta::Undecodable { reason, .. } = meta else {
            unreachable!()
        };
        assert!(reason.contains("did not decode"), "{reason}");
    }

    #[test]
    fn display_sanitising_strips_text_that_can_misrepresent_itself() {
        // A right-to-left override can make a symbol render as something else entirely.
        assert_eq!(sanitise_for_display("SAFE\u{202E}NUR"), "SAFENUR");
        // Zero-width characters let a copycat look identical to the token it imitates.
        assert_eq!(sanitise_for_display("PE\u{200B}PE"), "PEPE");
        assert_eq!(sanitise_for_display("bad\nline\u{0007}"), "badline");
        // Ordinary text, including emoji, is left exactly as it is.
        assert_eq!(
            sanitise_for_display("W\u{1f170}\u{fe0f}FFLE"),
            "W\u{1f170}\u{fe0f}FFLE"
        );
        assert_eq!(sanitise_for_display("SpaceWaffle"), "SpaceWaffle");
    }

    #[test]
    fn a_declared_but_empty_social_is_absent_not_present() {
        assert_eq!(presence(""), Presence::Absent);
        assert_eq!(presence("   "), Presence::Absent);
        assert_eq!(presence("https://x.com/a"), Presence::Present);
    }
}
