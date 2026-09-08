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
use banana_core::features::{Presence, Socials};

use crate::abi::{IPonsFactoryNoExempt, IPonsRouter, IPonsToken};

/// The factory and the router declare structurally identical `TokenParams`, but `sol!`
/// gives each interface its own Rust type. One conversion keeps a single decode path.
fn to_router_params_no_exempt(p: IPonsFactoryNoExempt::TokenParams) -> IPonsRouter::TokenParams {
    IPonsRouter::TokenParams {
        name: p.name,
        symbol: p.symbol,
        logo: p.logo,
        description: p.description,
        socials: IPonsRouter::Socials {
            twitter: p.socials.twitter,
            telegram: p.socials.telegram,
            discord: p.socials.discord,
            website: p.socials.website,
            farcaster: p.socials.farcaster,
        },
        creatorFeeRecipient: p.creatorFeeRecipient,
        creatorTaxBps: p.creatorTaxBps,
        buybackEnabled: p.buybackEnabled,
        expectedEconomics: p.expectedEconomics,
        salt: p.salt,
    }
}

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

/// Every launch call in a transaction's calldata, in the order they appear.
///
/// Most launches are a direct call and this returns one frame found at offset zero. The
/// rest arrive through a **bundler**: a contract whose own selector we do not know, which
/// carries the real `launchAndBuy` as an ABI `bytes` argument. Measured over a 24-hour
/// window, that is 6,210 of 24,984 launches -- a quarter of the universe -- and about 90%
/// of them carry a nested frame this recovers (`docs/FINDINGS.md` §11).
///
/// Refusing to read them was safe but expensive: every one had `Unknown` socials, so
/// `require_twitter` refused it, and the default strategy was discarding a quarter of the
/// chain for a reason about our decoder rather than about the token.
///
/// # Why a byte scan does not produce nonsense
///
/// Three things have to hold at once, and a coincidental four-byte match satisfies none of
/// them for long:
///
/// 1. The four bytes match a known launch selector.
/// 2. The 32 bytes before them read as a length that fits inside the remaining calldata.
///    A nested call is passed as `bytes`, so the ABI puts its length immediately in front.
/// 3. That slice **decodes completely** as the call the selector names, with every dynamic
///    offset in range and every string valid UTF-8.
///
/// Probed against the real chain, the two false positives in the window were contract
/// *creation* transactions, whose bytecode happens to contain the selector: both failed
/// (2) with a length word of ~10^76, and neither reached (3).
pub fn decode_launches(input: &[u8]) -> Vec<LaunchCalldata> {
    /// A bundler that wrapped more launches than this is not something to guess about.
    const MAX_FRAMES: usize = 64;

    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 4 <= input.len() && out.len() < MAX_FRAMES {
        if !is_launch_selector(&input[i..i + 4]) {
            i += 1;
            continue;
        }
        // Prefer the length-bounded slice. Decoding to the end of the calldata would also
        // work for a well-formed frame -- trailing bytes are ignored -- but a frame that is
        // shorter than the decode wants would then silently read the bundler's own
        // arguments as the launch's.
        let framed = nested_frame(input, i);
        // How far to skip depends on which path succeeded. A length word is only
        // trustworthy once the slice it describes has decoded; skipping by a length that
        // did not decode could step over the next real frame or land inside one.
        let hit = match framed.and_then(decode_frame) {
            Some(call) => Some((call, framed.map_or(4, <[u8]>::len))),
            None => decode_frame(&input[i..]).map(|call| (call, 4)),
        };
        match hit {
            Some((call, step)) => {
                out.push(call);
                i += step;
            }
            None => i += 1,
        }
    }
    out
}

/// The slice a nested `bytes` argument points at, if the length in front of `at` fits.
fn nested_frame(input: &[u8], at: usize) -> Option<&[u8]> {
    let len_word = input.get(at.checked_sub(32)?..at)?;
    // A length larger than the calldata is the signature of a coincidence, not a call.
    let len = U256::from_be_slice(len_word);
    let len = usize::try_from(len).ok()?;
    if len < 4 || len > input.len() - at {
        return None;
    }
    input.get(at..at + len)
}

/// Every selector that is a launch call in its own right.
///
/// Exposed so nothing has to hard-code these as hex. A query that lists the direct routes
/// by string drifts the moment a fourth is found -- which has already happened once, when
/// the three-argument `launchToken` turned out to be 47% of launches.
pub fn launch_selectors() -> [[u8; 4]; 3] {
    [
        IPonsRouter::launchAndBuyCall::SELECTOR,
        IPonsRouter::launchTokenCall::SELECTOR,
        IPonsFactoryNoExempt::launchTokenCall::SELECTOR,
    ]
}

/// The same list as `0x`-prefixed lowercase hex, matching `enrichment.selector`.
pub fn launch_selectors_hex() -> Vec<String> {
    launch_selectors()
        .iter()
        .map(|s| format!("0x{}", alloy_primitives::hex::encode(s)))
        .collect()
}

fn is_launch_selector(b: &[u8]) -> bool {
    launch_selectors().iter().any(|s| s == b)
}

/// Decode one complete call frame, or `None` if it is not a launch we understand.
fn decode_frame(input: &[u8]) -> Option<LaunchCalldata> {
    if input.len() < 4 {
        return None;
    }
    let selector = &input[..4];

    // Launch without a buy, called on the factory. Measured to be the DOMINANT direct
    // route -- 47% of launches against 35% for launchAndBuy -- so handling it is what took
    // decode coverage from a third of the universe to four fifths.
    if selector == IPonsRouter::launchTokenCall::SELECTOR {
        let c = IPonsRouter::launchTokenCall::abi_decode(input).ok()?;
        return Some(build(
            c.params,
            c.snipeTaxExemptions,
            // No opening buy on this path: the launcher paid only the launch fee.
            U256::ZERO,
            Address::ZERO,
            c.launchConfigId,
            c.pairToken,
        ));
    }
    if selector == IPonsFactoryNoExempt::launchTokenCall::SELECTOR {
        let c = IPonsFactoryNoExempt::launchTokenCall::abi_decode(input).ok()?;
        return Some(build(
            to_router_params_no_exempt(c.params),
            Vec::new(),
            U256::ZERO,
            Address::ZERO,
            c.launchConfigId,
            c.pairToken,
        ));
    }
    if selector == IPonsRouter::launchAndBuyCall::SELECTOR {
        let c = IPonsRouter::launchAndBuyCall::abi_decode(input).ok()?;
        return Some(build(
            c.params,
            c.snipeTaxExemptions,
            c.quoteIn,
            c.recipient,
            c.launchConfigId,
            c.pairToken,
        ));
    }
    None
}

/// Decode the launch calldata for a transaction that produced exactly one launch.
///
/// Never fails: an unrecognised shape becomes [`LaunchMeta::Undecodable`], because the
/// indexer must record every launch and a panic or an error would drop one silently.
pub fn decode_launch(input: &[u8]) -> LaunchMeta {
    decode_launch_at(input, 0, 1)
}

/// Decode the calldata for the `ordinal`-th of `total` launches in one transaction.
///
/// Only three transactions in a measured 24-hour window launched more than one token, but
/// the mapping still has to be right or those launches get another token's name and
/// socials. The rule is deliberately strict: the frames found must match the launch events
/// seen **exactly** in number, or nothing is decoded. Guessing which frame belongs to which
/// event would be the flattering direction, and there is no way to check it afterwards.
pub fn decode_launch_at(input: &[u8], ordinal: usize, total: usize) -> LaunchMeta {
    let mut selector = [0u8; 4];
    if input.len() < 4 {
        return LaunchMeta::Undecodable {
            selector,
            reason: format!(
                "calldata is {} bytes, too short for a selector",
                input.len()
            ),
        };
    }
    selector.copy_from_slice(&input[..4]);

    let mut frames = decode_launches(input);
    if frames.len() != total {
        return LaunchMeta::Undecodable {
            selector,
            reason: if frames.is_empty() {
                let hex = alloy_primitives::hex::encode(selector);
                if is_launch_selector(&selector) {
                    // A route we know, whose arguments are not what we expect. Worth
                    // saying differently from an unknown wrapper: this one means the ABI
                    // has moved, which `doctor` should be catching.
                    format!("selector 0x{hex} is a launch route but its arguments did not decode")
                } else {
                    format!(
                        "selector 0x{hex} is not a known launch route and no launch call is                          nested inside it"
                    )
                }
            } else {
                format!(
                    "{} launch call(s) in calldata but {total} launch event(s) in the                      transaction; which belongs to which cannot be established",
                    frames.len()
                )
            },
        };
    }
    if ordinal >= frames.len() {
        return LaunchMeta::Undecodable {
            selector,
            reason: format!("launch {ordinal} of {total} is outside the frames found"),
        };
    }
    LaunchMeta::Decoded(Box::new(frames.swap_remove(ordinal)))
}

fn build(
    p: IPonsRouter::TokenParams,
    exemptions: Vec<Address>,
    quote_in: U256,
    recipient: Address,
    launch_config_id: U256,
    pair_token: Address,
) -> LaunchCalldata {
    let s = p.socials;
    let urls = SocialUrls {
        twitter: s.twitter.clone(),
        telegram: s.telegram.clone(),
        discord: s.discord.clone(),
        website: s.website.clone(),
        farcaster: s.farcaster.clone(),
    };

    LaunchCalldata {
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
        exempt_wallets: exemptions,
        declared_quote_in: quote_in,
        recipient,
        launch_config_id,
        pair_token,
    }
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

    // --- bundlers -----------------------------------------------------------------------

    /// Calldata from `0x042bde9c…`, a launch through the window's most common bundler
    /// (`0xe1b77db5`, 2,749 launches). The launch parameters are one frame down.
    fn bundler_calldata() -> Vec<u8> {
        let raw = include_str!("../tests/fixtures/bundler_calldata.hex");
        hex::decode(raw.trim()).expect("fixture is valid hex")
    }

    /// Calldata from `0x7d135096…`, which produced **four** launches and contains no
    /// recognisable launch call at all.
    fn bundler_multi_calldata() -> Vec<u8> {
        let raw = include_str!("../tests/fixtures/bundler_multi_calldata.hex");
        hex::decode(raw.trim()).expect("fixture is valid hex")
    }

    /// Read off the real transaction: the nested `bytes` argument runs from 548 for 1,444.
    const NESTED_AT: usize = 548;
    const NESTED_LEN: usize = 1_444;

    #[test]
    fn a_launch_wrapped_by_a_bundler_is_read_through() {
        let meta = decode_launch(&bundler_calldata());
        let LaunchMeta::Decoded(c) = meta else {
            panic!("the launch parameters are in the calldata, one frame down");
        };
        assert_eq!(c.name, "Google Bucks");
        assert_eq!(c.symbol, "gBUX");
        assert_eq!(c.creator_tax_bps, 0);
        // The reason this matters: under the old decoder this launch had an *unknown*
        // bundle, so `max_exempt_wallets` refused it for want of data. The bundle is real
        // and it is five, which refuses it on the merits instead.
        assert_eq!(c.exempt_wallets.len(), 5);
        assert_eq!(c.socials.twitter, Presence::Present);
    }

    #[test]
    fn a_bundler_carrying_no_launch_call_is_refused_rather_than_guessed_at() {
        // Four launches came out of this transaction and none of their parameters is in
        // it. Whatever route it used, inventing an answer would be worse than Unknown.
        let meta = decode_launch_at(&bundler_multi_calldata(), 0, 4);
        let LaunchMeta::Undecodable { reason, .. } = meta else {
            panic!("nothing in this calldata decodes");
        };
        assert!(reason.contains("not a known launch route"), "{reason}");
        assert_eq!(decode_launches(&bundler_multi_calldata()).len(), 0);
    }

    /// The false-positive case, taken from the two real ones in the window.
    #[test]
    fn a_selector_that_appears_inside_bytecode_is_not_mistaken_for_a_call() {
        // Both false positives in the measured window were contract *creation*
        // transactions whose bytecode contains the four bytes. What rejects them is that
        // the 32 bytes in front do not read as a length that fits.
        let mut bytecode = vec![0x60, 0x80, 0x60, 0x40];
        bytecode.extend(std::iter::repeat_n(0xAB, 100));
        bytecode.extend_from_slice(&IPonsRouter::launchAndBuyCall::SELECTOR);
        bytecode.extend(std::iter::repeat_n(0xCD, 200));

        assert_eq!(
            decode_launches(&bytecode).len(),
            0,
            "four bytes in the middle of bytecode are not a launch"
        );
    }

    #[test]
    fn several_nested_launches_are_matched_to_their_events_by_position() {
        // Constructed, because no transaction in the measured window both launched several
        // tokens and carried their calls: outer selector, then each frame behind its own
        // ABI length word, which is how a `bytes` argument is laid out.
        let bundler = bundler_calldata();
        let google = &bundler[NESTED_AT..NESTED_AT + NESTED_LEN];
        let waffle = real_launch_calldata();

        let mut two = vec![0xDE, 0xAD, 0xBE, 0xEF];
        for frame in [waffle.as_slice(), google] {
            two.extend_from_slice(&U256::from(frame.len()).to_be_bytes::<32>()[..]);
            two.extend_from_slice(frame);
        }

        assert_eq!(decode_launches(&two).len(), 2);
        assert_eq!(decode_launch_at(&two, 0, 2).name(), Some("SpaceWaffle"));
        assert_eq!(decode_launch_at(&two, 1, 2).name(), Some("Google Bucks"));
    }

    #[test]
    fn a_frame_count_that_disagrees_with_the_event_count_decodes_nothing() {
        // Two frames, one launch event. There is no way to establish which frame belongs
        // to the event, and picking one would give some launch another token's socials.
        let bundler = bundler_calldata();
        let google = &bundler[NESTED_AT..NESTED_AT + NESTED_LEN];
        let waffle = real_launch_calldata();

        let mut two = vec![0xDE, 0xAD, 0xBE, 0xEF];
        for frame in [waffle.as_slice(), google] {
            two.extend_from_slice(&U256::from(frame.len()).to_be_bytes::<32>()[..]);
            two.extend_from_slice(frame);
        }

        let meta = decode_launch(&two);
        let LaunchMeta::Undecodable { reason, .. } = &meta else {
            panic!("an ambiguous mapping must not resolve to one of the candidates");
        };
        assert!(reason.contains("cannot be established"), "{reason}");
        assert_eq!(meta.socials(), Socials::UNKNOWN);
    }

    #[test]
    fn a_direct_launch_is_still_one_frame_at_offset_zero() {
        // The nested scan must not change the common case.
        let frames = decode_launches(&real_launch_calldata());
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].name, "SpaceWaffle");
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
        assert!(reason.contains("not a known launch route"), "{reason}");
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
