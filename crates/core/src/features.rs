//! What is known about a launch, split by **when** it became knowable.
//!
//! Spec §5.3 is the reason this file exists as two types rather than one struct with a
//! comment. Using current state for a point-in-time feature produces a beautiful, wrong
//! backtest, and it is the single easiest way to build a product that lies. So:
//!
//! * [`PitFeatures`] — computed only from blocks strictly **before** `launch_block`, plus
//!   the launch transaction's own calldata. This is the only thing an entry filter ever
//!   sees.
//! * [`PostEntryFacts`] — the future relative to entry. Display only. The filter evaluator
//!   does not take one as an argument, so a rule cannot read one even by accident.
//!
//! That separation is structural. There is no `#[allow]` that defeats it and no review
//! step it depends on.

use serde::{Deserialize, Serialize};

use crate::Bps;

/// Whether a fact is true, false, or was never readable.
///
/// The third state is not pedantry. Launch metadata comes from decoding `launchAndBuy`
/// calldata (see `docs/FINDINGS.md` §4); a launch through a different router, a direct
/// factory call or a bundler will not decode. Collapsing that into "absent" would let a
/// filter silently treat "we could not read this" as "this token has no Twitter", which is
/// a lie in the direction that flatters the backtest.
///
/// A `require_*` rule **refuses** an `Unknown`. Refusing is the honest direction when the
/// data is missing, and the count of unknowns is surfaced in the funnel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    Present,
    Absent,
    Unknown,
}

impl Presence {
    /// True only when definitely present. `Unknown` is not present.
    pub fn is_present(self) -> bool {
        matches!(self, Presence::Present)
    }

    pub fn from_str_field(s: &str) -> Self {
        if s.trim().is_empty() {
            Presence::Absent
        } else {
            Presence::Present
        }
    }
}

/// Social links as declared **in the launch calldata**.
///
/// Never sourced from `getTokenInfo()`, which returns current state: a token that added a
/// link an hour after launch would look like it had one at launch, inflating every
/// backtest that used `require_twitter`. See `docs/FINDINGS.md` §4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Socials {
    pub twitter: Presence,
    pub website: Presence,
    pub telegram: Presence,
}

impl Socials {
    /// Every field unreadable, because the launch transaction did not decode.
    pub const UNKNOWN: Self = Self {
        twitter: Presence::Unknown,
        website: Presence::Unknown,
        telegram: Presence::Unknown,
    };

    /// Nothing declared, and we know that for certain.
    pub const NONE: Self = Self {
        twitter: Presence::Absent,
        website: Presence::Absent,
        telegram: Presence::Absent,
    };

    pub fn any_present(&self) -> bool {
        self.twitter.is_present() || self.website.is_present() || self.telegram.is_present()
    }
}

/// Who receives the creator fee, relative to the deployer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeeRecipient {
    /// The deployer pays itself: the ordinary case.
    Deployer,
    /// A third party. Often a builder or KOL arrangement, which is a signal either way.
    ThirdParty,
    Unknown,
}

/// The quote asset a curve trades in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pair {
    /// Native ETH, `pairToken == address(0)`.
    Eth,
    /// Anything else, by symbol as read from the token contract.
    Other(String),
}

impl Pair {
    pub fn label(&self) -> &str {
        match self {
            Pair::Eth => "ETH",
            Pair::Other(s) => s,
        }
    }
}

/// Everything an entry filter is allowed to read.
///
/// Every field here is computable from blocks strictly before `launch_block`, or from the
/// launch transaction's own calldata. `crates/backtest` carries a regression test that
/// fails if any of these is derived from a block at or after `launch_block`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PitFeatures {
    // --- from the TokenLaunched event -------------------------------------------------
    pub pair: Pair,

    // --- from launch calldata (point-in-time by construction) -------------------------
    pub name: String,
    pub symbol: String,
    pub description: String,
    pub socials: Socials,
    /// Wallets declared exempt from the opening tax at launch: the declared bundle.
    ///
    /// `None` when the launch transaction did not decode. Not zero: recording an
    /// unreadable bundle as an empty one would let `max_exempt_wallets` pass a launch
    /// whose bundle was never seen, which is [`Presence::Unknown`]'s mistake in a
    /// different field.
    pub exempt_wallets: Option<u32>,

    // --- from logs emitted in the launch transaction ----------------------------------
    /// The launcher's own buy **in the launch transaction**, as basis points of supply.
    ///
    /// `Some(0)` is a real answer — the deployer launched without buying — and is a
    /// different signal from `None`, which means the launch transaction was unreadable.
    pub dev_buy_bps: Option<Bps>,
    /// `None` when the launch transaction did not decode.
    pub creator_tax_bps: Option<Bps>,
    pub fee_recipient: FeeRecipient,

    // --- from blocks strictly before launch_block -------------------------------------
    pub deployer_launches: u32,
    pub deployer_graduations: u32,
    /// Earlier launches in the preceding 30 minutes sharing this launch's fingerprint,
    /// from a **different** deployer. One operator printing tokens from many wallets.
    pub fingerprint_twins_30m: u32,
    /// How much prior history actually existed for this launch (PLAN.md C2).
    ///
    /// On a 24-hour index a launch two hours in has two hours of visible deployer history
    /// and one twenty hours in has twenty, so every deployer looks fresher than it is and
    /// progressively more so toward the start of the window. Any strategy using a
    /// deployer feature restricts the universe by this value, and says so in the funnel.
    pub deployer_history_depth_blocks: u64,
}

impl PitFeatures {
    /// Graduation rate in basis points, or `None` when the deployer has no prior launches.
    ///
    /// `None` rather than zero: "never launched before" and "launched nine times and never
    /// graduated" are opposite signals, and a zero would merge them.
    pub fn deployer_grad_rate_bps(&self) -> Option<Bps> {
        if self.deployer_launches == 0 {
            return None;
        }
        Some((self.deployer_graduations * crate::BPS) / self.deployer_launches)
    }

    /// Name, symbol and description joined, for keyword matching.
    pub fn haystack(&self) -> String {
        format!("{} {} {}", self.name, self.symbol, self.description)
    }
}

/// Facts that are the **future** relative to entry.
///
/// Display only, and structurally unreachable from a filter: nothing in this type is ever
/// passed to the rule engine. Spec §5.2 lists these as "post-entry facts, display only,
/// never filter inputs"; keeping them in a separate type is what enforces it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostEntryFacts {
    pub distinct_buyers_1m: u32,
    /// Every buy so far paid the opening tax, meaning only bots have touched it.
    pub every_early_buy_taxed: bool,
    /// Progress along the curve. A **current-state** reading, which is precisely why it
    /// cannot live in [`PitFeatures`] (PLAN.md C5).
    pub curve_progress_bps: Bps,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feats() -> PitFeatures {
        PitFeatures {
            pair: Pair::Eth,
            name: "SpaceWaffle".into(),
            symbol: "WAFFLE".into(),
            description: "backed by a perp".into(),
            socials: Socials::NONE,
            exempt_wallets: Some(0),
            dev_buy_bps: Some(495),
            creator_tax_bps: Some(0),
            fee_recipient: FeeRecipient::Deployer,
            deployer_launches: 0,
            deployer_graduations: 0,
            fingerprint_twins_30m: 0,
            deployer_history_depth_blocks: 0,
        }
    }

    #[test]
    fn unknown_is_not_present() {
        assert!(!Presence::Unknown.is_present());
        assert!(!Presence::Absent.is_present());
        assert!(Presence::Present.is_present());
    }

    #[test]
    fn unknown_socials_do_not_count_as_having_any() {
        assert!(!Socials::UNKNOWN.any_present());
        assert!(!Socials::NONE.any_present());
    }

    #[test]
    fn no_prior_launches_is_none_not_zero_rate() {
        let mut f = feats();
        assert_eq!(f.deployer_grad_rate_bps(), None, "never launched");

        f.deployer_launches = 9;
        f.deployer_graduations = 0;
        assert_eq!(
            f.deployer_grad_rate_bps(),
            Some(0),
            "nine launches, none graduated, is a real zero and a different signal"
        );
    }

    #[test]
    fn grad_rate_is_integer_bps() {
        let mut f = feats();
        f.deployer_launches = 3;
        f.deployer_graduations = 1;
        assert_eq!(f.deployer_grad_rate_bps(), Some(3_333));
    }

    #[test]
    fn pit_features_round_trip_through_json() {
        let f = feats();
        let s = serde_json::to_string(&f).unwrap();
        assert_eq!(serde_json::from_str::<PitFeatures>(&s).unwrap(), f);
    }
}
