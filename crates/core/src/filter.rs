//! The rule engine: a serialisable predicate tree over point-in-time features.
//!
//! Spec §7.1. The decision is a **pure boolean** — a launch passes every rule or is refused
//! with the list of rules that refused it, each naming its actual value and its threshold.
//! There is no weighted score and no magic cutoff, which is what makes the Lab and the
//! live sniper provably identical rather than merely intended to agree.
//!
//! The tree is `All`/`Any`/`Not` from day one even though the first UI exposes only a flat
//! AND, because retrofitting it later means rewriting both this evaluator and the SQL
//! generator in `quarrel-store`.
//!
//! # The point-in-time boundary
//!
//! [`EntryFilter::evaluate`] takes `&PitFeatures` and nothing else. `PostEntryFacts` is a
//! different type and is never an argument, so a rule that reads the future is a type
//! error rather than something a reviewer has to notice (spec §5.3).

use serde::{Deserialize, Serialize};

use crate::Bps;
use crate::features::{FeeRecipient, Pair, PitFeatures, Presence};
use crate::pattern::Pattern;

/// Why a launch was refused: the rule, and the values that failed it.
///
/// Spec §3.4 requires the specific rule and the specific values, so `detail` is built to
/// read as a sentence a user can check against a block explorer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    /// Stable machine-readable rule id, e.g. `creator_tax`. Safe to group by.
    ///
    /// Owned rather than `&'static str` so a `Decision` can be deserialised: refusals
    /// cross the Tauri IPC boundary to the feed, and a borrowed id cannot come back.
    pub rule: String,
    /// Human-readable, with both sides of the comparison.
    pub detail: String,
}

impl Refusal {
    fn new(rule: &'static str, detail: impl Into<String>) -> Self {
        Self {
            rule: rule.to_owned(),
            detail: detail.into(),
        }
    }
}

/// The outcome of evaluating a filter. Boolean, plus the reasons.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub passed: bool,
    pub refusals: Vec<Refusal>,
}

impl Decision {
    fn pass() -> Self {
        Self {
            passed: true,
            refusals: Vec::new(),
        }
    }

    fn refuse(refusals: Vec<Refusal>) -> Self {
        debug_assert!(!refusals.is_empty(), "a refusal must give a reason");
        Self {
            passed: false,
            refusals,
        }
    }

    /// One line per refusal, for a log or a terminal.
    pub fn reasons(&self) -> Vec<String> {
        self.refusals
            .iter()
            .map(|r| format!("refused: {}", r.detail))
            .collect()
    }
}

/// A single atomic test against [`PitFeatures`].
///
/// Every variant reads a point-in-time field. Adding a variant that reads current state
/// would require passing that state in, which the signature does not permit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Condition {
    /// A declared Twitter/X link. An `Unknown` refuses (see [`Presence`]).
    RequireTwitter,
    RequireWebsite,
    RequireTelegram,
    /// Any one of the three.
    RequireAnySocial,
    /// The launcher's own buy as basis points of supply. Both ends optional.
    DevBuyBps {
        min: Option<Bps>,
        max: Option<Bps>,
    },
    MaxCreatorTaxBps {
        bps: Bps,
    },
    MaxExemptWallets {
        max: u32,
    },
    FeeRecipientIs {
        recipient: FeeRecipient,
    },
    /// Deployer graduation rate floor. A deployer with **no** prior launches has no rate;
    /// `allow_unproven` decides whether that passes.
    MinDeployerGradRateBps {
        bps: Bps,
        allow_unproven: bool,
    },
    MaxDeployerLaunches {
        max: u32,
    },
    MaxFingerprintTwins {
        max: u32,
    },
    /// Allowed quote assets.
    PairIn {
        pairs: Vec<Pair>,
    },
    /// Regex over name, symbol and description.
    Keyword {
        pattern: Pattern,
    },
}

impl Condition {
    /// A stable id for grouping refusals in the UI.
    pub fn rule_id(&self) -> &'static str {
        match self {
            Condition::RequireTwitter => "require_twitter",
            Condition::RequireWebsite => "require_website",
            Condition::RequireTelegram => "require_telegram",
            Condition::RequireAnySocial => "require_any_social",
            Condition::DevBuyBps { .. } => "dev_buy",
            Condition::MaxCreatorTaxBps { .. } => "creator_tax",
            Condition::MaxExemptWallets { .. } => "exempt_wallets",
            Condition::FeeRecipientIs { .. } => "fee_recipient",
            Condition::MinDeployerGradRateBps { .. } => "deployer_grad_rate",
            Condition::MaxDeployerLaunches { .. } => "deployer_launches",
            Condition::MaxFingerprintTwins { .. } => "fingerprint_twins",
            Condition::PairIn { .. } => "pair",
            Condition::Keyword { .. } => "keyword",
        }
    }

    /// True when this condition reads a deployer-derived feature, which triggers the
    /// history-depth restriction of PLAN.md C2.
    pub fn uses_deployer_history(&self) -> bool {
        matches!(
            self,
            Condition::MinDeployerGradRateBps { .. } | Condition::MaxDeployerLaunches { .. }
        )
    }

    fn check(&self, f: &PitFeatures) -> Option<Refusal> {
        let id = self.rule_id();
        match self {
            Condition::RequireTwitter => social(id, "twitter", f.socials.twitter),
            Condition::RequireWebsite => social(id, "website", f.socials.website),
            Condition::RequireTelegram => social(id, "telegram", f.socials.telegram),
            Condition::RequireAnySocial => {
                if f.socials.any_present() {
                    None
                } else if f.socials == crate::features::Socials::UNKNOWN {
                    Some(Refusal::new(
                        id,
                        "socials unreadable (launch did not decode), so cannot confirm any",
                    ))
                } else {
                    Some(Refusal::new(id, "no socials declared at launch"))
                }
            }
            Condition::DevBuyBps { min: lo, max: hi } => {
                let v = f.dev_buy_bps;
                if let Some(min) = *lo
                    && v < min
                {
                    return Some(Refusal::new(
                        id,
                        format!("dev_buy {} < floor {}", pct(v), pct(min)),
                    ));
                }
                if let Some(max) = *hi
                    && v > max
                {
                    return Some(Refusal::new(
                        id,
                        format!("dev_buy {} > ceiling {}", pct(v), pct(max)),
                    ));
                }
                None
            }
            Condition::MaxCreatorTaxBps { bps: max } => (f.creator_tax_bps > *max).then(|| {
                Refusal::new(
                    id,
                    format!(
                        "creator_tax {} > ceiling {}",
                        pct(f.creator_tax_bps),
                        pct(*max)
                    ),
                )
            }),
            Condition::MaxExemptWallets { max } => (f.exempt_wallets > *max).then(|| {
                Refusal::new(
                    id,
                    format!("{} exempt wallets > ceiling {}", f.exempt_wallets, max),
                )
            }),
            Condition::FeeRecipientIs { recipient: want } => {
                (f.fee_recipient != *want).then(|| {
                    Refusal::new(
                        id,
                        format!("fee_recipient {:?}, wanted {:?}", f.fee_recipient, want),
                    )
                })
            }
            Condition::MinDeployerGradRateBps {
                bps,
                allow_unproven,
            } => match f.deployer_grad_rate_bps() {
                None => (!allow_unproven).then(|| {
                    Refusal::new(
                        id,
                        "deployer has no prior launches, so no graduation rate to test",
                    )
                }),
                Some(rate) => (rate < *bps).then(|| {
                    Refusal::new(
                        id,
                        format!(
                            "deployer_grad_rate {} < floor {} ({}/{} graduated)",
                            pct(rate),
                            pct(*bps),
                            f.deployer_graduations,
                            f.deployer_launches
                        ),
                    )
                }),
            },
            Condition::MaxDeployerLaunches { max } => (f.deployer_launches > *max).then(|| {
                Refusal::new(
                    id,
                    format!(
                        "deployer has {} prior launches > ceiling {}",
                        f.deployer_launches, max
                    ),
                )
            }),
            Condition::MaxFingerprintTwins { max } => (f.fingerprint_twins_30m > *max).then(|| {
                Refusal::new(
                    id,
                    format!(
                        "launch farm: {} twins in 30 min > ceiling {}",
                        f.fingerprint_twins_30m, max
                    ),
                )
            }),
            Condition::PairIn { pairs: allowed } => (!allowed.contains(&f.pair)).then(|| {
                let names: Vec<_> = allowed.iter().map(|p| p.label()).collect();
                Refusal::new(
                    id,
                    format!(
                        "pair is {}, not one of [{}]",
                        f.pair.label(),
                        names.join(", ")
                    ),
                )
            }),
            Condition::Keyword { pattern: p } => (!p.is_match(&f.haystack()))
                .then(|| Refusal::new(id, format!("keyword /{}/ not found", p.as_str()))),
        }
    }
}

fn social(id: &'static str, which: &str, p: Presence) -> Option<Refusal> {
    match p {
        Presence::Present => None,
        Presence::Absent => Some(Refusal::new(id, format!("no {which} declared at launch"))),
        // Not "absent": we could not read the launch calldata, so we refuse rather than
        // guess in the direction that flatters the backtest.
        Presence::Unknown => Some(Refusal::new(
            id,
            format!("{which} unreadable (launch transaction did not decode)"),
        )),
    }
}

/// Format basis points as a percentage without floating point.
///
/// 250 -> "2.50%". Integer division only; spec §12.
fn pct(bps: Bps) -> String {
    format!("{}.{:02}%", bps / 100, bps % 100)
}

/// A serialisable predicate tree.
///
/// `All` of an empty list passes (the identity of AND) and `Any` of an empty list refuses,
/// which are the mathematically right answers and keep tree-building code simple.
/// Adjacently tagged (`{"op": ..., "of": ...}`) rather than internally tagged, because an
/// internal tag cannot be attached to the sequence carried by `All`/`Any`, and an
/// `untagged` condition variant makes serde's generated impls exceed the trait-resolution
/// depth on a recursive type. The wire form stays readable:
///
/// ```json
/// {"op":"all","of":[{"op":"cond","of":{"kind":"require_twitter"}}]}
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", content = "of", rename_all = "snake_case")]
pub enum EntryFilter {
    All(Vec<EntryFilter>),
    Any(Vec<EntryFilter>),
    Not(Box<EntryFilter>),
    Cond(Condition),
}

impl EntryFilter {
    /// Evaluate against point-in-time features.
    ///
    /// The signature is the point-in-time guarantee: there is no argument through which a
    /// post-entry fact could arrive.
    pub fn evaluate(&self, f: &PitFeatures) -> Decision {
        match self {
            EntryFilter::Cond(c) => match c.check(f) {
                None => Decision::pass(),
                Some(r) => Decision::refuse(vec![r]),
            },
            EntryFilter::All(children) => {
                let mut refusals = Vec::new();
                for child in children {
                    let d = child.evaluate(f);
                    if !d.passed {
                        refusals.extend(d.refusals);
                    }
                }
                if refusals.is_empty() {
                    Decision::pass()
                } else {
                    Decision::refuse(refusals)
                }
            }
            EntryFilter::Any(children) => {
                if children.is_empty() {
                    return Decision::refuse(vec![Refusal::new(
                        "any",
                        "no alternatives were offered",
                    )]);
                }
                let mut all = Vec::new();
                for child in children {
                    let d = child.evaluate(f);
                    if d.passed {
                        return Decision::pass();
                    }
                    all.extend(d.refusals);
                }
                // Every branch failed, so every branch's reason is relevant.
                let mut refusals = vec![Refusal::new(
                    "any",
                    format!("none of {} alternatives passed", children.len()),
                )];
                refusals.extend(all);
                Decision::refuse(refusals)
            }
            EntryFilter::Not(inner) => {
                if inner.evaluate(f).passed {
                    Decision::refuse(vec![Refusal::new(
                        "not",
                        "excluded: the negated condition matched",
                    )])
                } else {
                    Decision::pass()
                }
            }
        }
    }

    /// Walk every condition in the tree.
    pub fn conditions(&self) -> Vec<&Condition> {
        let mut out = Vec::new();
        self.walk(&mut |c| out.push(c));
        out
    }

    fn walk<'a>(&'a self, f: &mut impl FnMut(&'a Condition)) {
        match self {
            EntryFilter::Cond(c) => f(c),
            EntryFilter::All(cs) | EntryFilter::Any(cs) => {
                for c in cs {
                    c.walk(f);
                }
            }
            EntryFilter::Not(inner) => inner.walk(f),
        }
    }

    /// Whether any condition in the tree reads deployer history (PLAN.md C2).
    ///
    /// When true, the backtest restricts the universe by `deployer_history_depth_blocks`
    /// and shows that restriction as its own funnel stage.
    pub fn uses_deployer_history(&self) -> bool {
        self.conditions().iter().any(|c| c.uses_deployer_history())
    }

    /// Convenience for the flat-AND case the first UI exposes.
    pub fn all_of(conditions: impl IntoIterator<Item = Condition>) -> Self {
        EntryFilter::All(conditions.into_iter().map(EntryFilter::Cond).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::{Socials, *};

    fn clean() -> PitFeatures {
        PitFeatures {
            pair: Pair::Eth,
            name: "SpaceWaffle".into(),
            symbol: "WAFFLE".into(),
            description: "backed by a 2x long perp".into(),
            socials: Socials {
                twitter: Presence::Present,
                website: Presence::Present,
                telegram: Presence::Absent,
            },
            exempt_wallets: 0,
            dev_buy_bps: 300,
            creator_tax_bps: 100,
            fee_recipient: FeeRecipient::Deployer,
            deployer_launches: 0,
            deployer_graduations: 0,
            fingerprint_twins_30m: 0,
            deployer_history_depth_blocks: 500_000,
        }
    }

    /// The bodkin-derived defaults of spec §7.1, so the baseline is recognisable.
    fn default_filter() -> EntryFilter {
        EntryFilter::all_of([
            Condition::RequireTwitter,
            Condition::DevBuyBps {
                min: Some(100),
                max: Some(600),
            },
            Condition::MaxCreatorTaxBps { bps: 200 },
            Condition::MaxExemptWallets { max: 2 },
            Condition::MaxFingerprintTwins { max: 1 },
            Condition::PairIn {
                pairs: vec![Pair::Eth],
            },
        ])
    }

    #[test]
    fn a_clean_launch_passes_the_defaults() {
        let d = default_filter().evaluate(&clean());
        assert!(d.passed, "unexpected refusals: {:?}", d.reasons());
        assert!(d.refusals.is_empty());
    }

    // --- every rule's own refusal path (spec §9) --------------------------------------

    #[test]
    fn require_twitter_refuses_absent() {
        let mut f = clean();
        f.socials.twitter = Presence::Absent;
        let d = EntryFilter::all_of([Condition::RequireTwitter]).evaluate(&f);
        assert!(!d.passed);
        assert_eq!(d.refusals[0].rule, "require_twitter");
        assert!(d.refusals[0].detail.contains("no twitter declared"));
    }

    #[test]
    fn require_twitter_refuses_unknown_rather_than_assuming_absent() {
        let mut f = clean();
        f.socials = Socials::UNKNOWN;
        let d = EntryFilter::all_of([Condition::RequireTwitter]).evaluate(&f);
        assert!(!d.passed);
        assert!(
            d.refusals[0].detail.contains("unreadable"),
            "an unreadable launch must say so, not claim the link is absent: {:?}",
            d.refusals[0].detail
        );
    }

    #[test]
    fn require_website_and_telegram_refuse() {
        let mut f = clean();
        f.socials.website = Presence::Absent;
        assert!(
            !EntryFilter::all_of([Condition::RequireWebsite])
                .evaluate(&f)
                .passed
        );
        assert!(
            !EntryFilter::all_of([Condition::RequireTelegram])
                .evaluate(&f)
                .passed
        );
    }

    #[test]
    fn require_any_social_passes_on_one_and_refuses_on_none() {
        let mut f = clean();
        assert!(
            EntryFilter::all_of([Condition::RequireAnySocial])
                .evaluate(&f)
                .passed
        );
        f.socials = Socials::NONE;
        let d = EntryFilter::all_of([Condition::RequireAnySocial]).evaluate(&f);
        assert!(!d.passed);
        assert!(d.refusals[0].detail.contains("no socials declared"));
    }

    #[test]
    fn dev_buy_refuses_both_ends_and_names_the_values() {
        let band = Condition::DevBuyBps {
            min: Some(100),
            max: Some(600),
        };
        let mut f = clean();

        f.dev_buy_bps = 50;
        let d = EntryFilter::all_of([band.clone()]).evaluate(&f);
        assert_eq!(d.refusals[0].detail, "dev_buy 0.50% < floor 1.00%");

        f.dev_buy_bps = 900;
        let d = EntryFilter::all_of([band]).evaluate(&f);
        assert_eq!(d.refusals[0].detail, "dev_buy 9.00% > ceiling 6.00%");
    }

    #[test]
    fn creator_tax_refusal_reads_exactly_as_the_spec_requires() {
        // Spec §3.4 gives this literal example.
        let mut f = clean();
        f.creator_tax_bps = 600;
        let d = EntryFilter::all_of([Condition::MaxCreatorTaxBps { bps: 200 }]).evaluate(&f);
        assert_eq!(d.refusals[0].detail, "creator_tax 6.00% > ceiling 2.00%");
        assert_eq!(d.reasons()[0], "refused: creator_tax 6.00% > ceiling 2.00%");
    }

    #[test]
    fn exempt_wallets_refuses_a_declared_bundle() {
        let mut f = clean();
        f.exempt_wallets = 4;
        let d = EntryFilter::all_of([Condition::MaxExemptWallets { max: 2 }]).evaluate(&f);
        assert_eq!(d.refusals[0].detail, "4 exempt wallets > ceiling 2");
    }

    #[test]
    fn fee_recipient_mode_refuses() {
        let mut f = clean();
        f.fee_recipient = FeeRecipient::ThirdParty;
        let d = EntryFilter::all_of([Condition::FeeRecipientIs {
            recipient: FeeRecipient::Deployer,
        }])
        .evaluate(&f);
        assert!(!d.passed);
        assert_eq!(d.refusals[0].rule, "fee_recipient");
    }

    #[test]
    fn fingerprint_twins_refuse_a_launch_farm() {
        let mut f = clean();
        f.fingerprint_twins_30m = 5;
        let d = EntryFilter::all_of([Condition::MaxFingerprintTwins { max: 1 }]).evaluate(&f);
        assert_eq!(
            d.refusals[0].detail,
            "launch farm: 5 twins in 30 min > ceiling 1"
        );
    }

    #[test]
    fn pair_refuses_a_non_eth_quote() {
        let mut f = clean();
        f.pair = Pair::Other("USDG".into());
        let d = EntryFilter::all_of([Condition::PairIn {
            pairs: vec![Pair::Eth],
        }])
        .evaluate(&f);
        assert_eq!(d.refusals[0].detail, "pair is USDG, not one of [ETH]");
    }

    #[test]
    fn keyword_refuses_when_absent_and_passes_when_present() {
        let f = clean();
        let hit = Condition::Keyword {
            pattern: Pattern::new("(?i)waffle").unwrap(),
        };
        assert!(EntryFilter::all_of([hit]).evaluate(&f).passed);

        let miss = Condition::Keyword {
            pattern: Pattern::new("(?i)dogwifhat").unwrap(),
        };
        let d = EntryFilter::all_of([miss]).evaluate(&f);
        assert_eq!(d.refusals[0].detail, "keyword /(?i)dogwifhat/ not found");
    }

    // --- the two shaped launches spec §9 asks for -------------------------------------

    #[test]
    fn a_builder_shaped_launch_is_refused_with_every_reason() {
        // Third-party fee recipient, heavy dev buy, a declared bundle, high creator tax.
        let mut f = clean();
        f.fee_recipient = FeeRecipient::ThirdParty;
        f.dev_buy_bps = 1_500;
        f.exempt_wallets = 6;
        f.creator_tax_bps = 500;

        let filter = EntryFilter::All(vec![
            EntryFilter::Cond(Condition::DevBuyBps {
                min: Some(100),
                max: Some(600),
            }),
            EntryFilter::Cond(Condition::MaxCreatorTaxBps { bps: 200 }),
            EntryFilter::Cond(Condition::MaxExemptWallets { max: 2 }),
            EntryFilter::Cond(Condition::FeeRecipientIs {
                recipient: FeeRecipient::Deployer,
            }),
        ]);
        let d = filter.evaluate(&f);
        assert!(!d.passed);
        assert_eq!(
            d.refusals.len(),
            4,
            "every failing rule must report, not just the first"
        );

        let rules: Vec<_> = d.refusals.iter().map(|r| r.rule.as_str()).collect();
        assert_eq!(
            rules,
            ["dev_buy", "creator_tax", "exempt_wallets", "fee_recipient"]
        );
    }

    #[test]
    fn a_serial_deployer_launch_is_refused_by_the_deployer_rules() {
        let mut f = clean();
        f.deployer_launches = 40;
        f.deployer_graduations = 0;

        let filter = EntryFilter::all_of([
            Condition::MaxDeployerLaunches { max: 10 },
            Condition::MinDeployerGradRateBps {
                bps: 1_000,
                allow_unproven: true,
            },
        ]);
        let d = filter.evaluate(&f);
        assert!(!d.passed);
        assert_eq!(d.refusals.len(), 2);
        assert_eq!(
            d.refusals[0].detail,
            "deployer has 40 prior launches > ceiling 10"
        );
        assert!(d.refusals[1].detail.contains("0/40 graduated"));
    }

    #[test]
    fn an_unproven_deployer_is_governed_by_allow_unproven() {
        let f = clean(); // zero prior launches
        let strict = EntryFilter::all_of([Condition::MinDeployerGradRateBps {
            bps: 1_000,
            allow_unproven: false,
        }]);
        assert!(!strict.evaluate(&f).passed);

        let lenient = EntryFilter::all_of([Condition::MinDeployerGradRateBps {
            bps: 1_000,
            allow_unproven: true,
        }]);
        assert!(lenient.evaluate(&f).passed);
    }

    // --- tree semantics ----------------------------------------------------------------

    #[test]
    fn any_passes_when_one_branch_passes() {
        let mut f = clean();
        f.socials.telegram = Presence::Absent;
        let filter = EntryFilter::Any(vec![
            EntryFilter::Cond(Condition::RequireTelegram),
            EntryFilter::Cond(Condition::RequireTwitter),
        ]);
        assert!(filter.evaluate(&f).passed);
    }

    #[test]
    fn any_refuses_with_every_branch_reason() {
        let mut f = clean();
        f.socials = Socials::NONE;
        let filter = EntryFilter::Any(vec![
            EntryFilter::Cond(Condition::RequireTelegram),
            EntryFilter::Cond(Condition::RequireTwitter),
        ]);
        let d = filter.evaluate(&f);
        assert!(!d.passed);
        assert_eq!(d.refusals[0].rule, "any");
        assert_eq!(d.refusals.len(), 3, "the summary plus both branch reasons");
    }

    #[test]
    fn not_inverts() {
        let f = clean(); // has twitter
        let filter = EntryFilter::Not(Box::new(EntryFilter::Cond(Condition::RequireTwitter)));
        assert!(
            !filter.evaluate(&f).passed,
            "twitter present, so NOT refuses"
        );

        let filter = EntryFilter::Not(Box::new(EntryFilter::Cond(Condition::RequireTelegram)));
        assert!(filter.evaluate(&f).passed, "telegram absent, so NOT passes");
    }

    #[test]
    fn empty_all_passes_and_empty_any_refuses() {
        let f = clean();
        assert!(
            EntryFilter::All(vec![]).evaluate(&f).passed,
            "identity of AND"
        );
        assert!(
            !EntryFilter::Any(vec![]).evaluate(&f).passed,
            "identity of OR"
        );
    }

    #[test]
    fn nested_trees_evaluate_correctly() {
        let mut f = clean();
        f.creator_tax_bps = 900;
        // (twitter AND (tax<=200 OR dev_buy in range))
        let filter = EntryFilter::All(vec![
            EntryFilter::Cond(Condition::RequireTwitter),
            EntryFilter::Any(vec![
                EntryFilter::Cond(Condition::MaxCreatorTaxBps { bps: 200 }),
                EntryFilter::Cond(Condition::DevBuyBps {
                    min: Some(100),
                    max: Some(600),
                }),
            ]),
        ]);
        assert!(filter.evaluate(&f).passed, "the OR is satisfied by dev_buy");

        f.dev_buy_bps = 5_000;
        assert!(
            !filter.evaluate(&f).passed,
            "now neither branch of the OR holds"
        );
    }

    // --- serialisation and introspection ------------------------------------------------

    #[test]
    fn the_tree_round_trips_through_json() {
        let filter = EntryFilter::All(vec![
            EntryFilter::Cond(Condition::RequireTwitter),
            EntryFilter::Not(Box::new(EntryFilter::Cond(Condition::MaxExemptWallets {
                max: 2,
            }))),
            EntryFilter::Any(vec![
                EntryFilter::Cond(Condition::MaxCreatorTaxBps { bps: 200 }),
                EntryFilter::Cond(Condition::Keyword {
                    pattern: Pattern::new("cat|dog").unwrap(),
                }),
            ]),
        ]);
        let json = serde_json::to_string(&filter).unwrap();
        let back: EntryFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(back, filter, "a saved strategy must reload identically");
    }

    #[test]
    fn deployer_history_use_is_detectable_for_the_c2_restriction() {
        assert!(!default_filter().uses_deployer_history());

        let with = EntryFilter::all_of([
            Condition::RequireTwitter,
            Condition::MaxDeployerLaunches { max: 10 },
        ]);
        assert!(with.uses_deployer_history());

        // It must be found through nesting, not only at the top level.
        let nested = EntryFilter::Any(vec![EntryFilter::Not(Box::new(EntryFilter::Cond(
            Condition::MinDeployerGradRateBps {
                bps: 100,
                allow_unproven: true,
            },
        )))]);
        assert!(nested.uses_deployer_history());
    }

    #[test]
    fn percent_formatting_uses_no_floating_point() {
        assert_eq!(pct(0), "0.00%");
        assert_eq!(pct(1), "0.01%");
        assert_eq!(pct(250), "2.50%");
        assert_eq!(pct(9_900), "99.00%");
        assert_eq!(pct(10_000), "100.00%");
    }
}
