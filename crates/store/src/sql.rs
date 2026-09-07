//! Pushing an [`EntryFilter`] into SQLite, as a **conservative pre-filter**.
//!
//! PLAN.md D6 called for a SQL generator that walks the same tree as the Rust evaluator,
//! with a test that the two agree. Building it turned up a problem with "agree": one
//! condition — [`Condition::Keyword`] — is a regex, and SQLite has no regex without a
//! registered function. A generator that must be *equal* to the evaluator therefore either
//! cannot exist or has to drag a regex engine into the query planner.
//!
//! So the contract here is one-sided instead, and it is stronger where it matters:
//!
//! > **Soundness.** Every launch the Rust evaluator would pass is returned by the SQL.
//! > The SQL may return extra rows; it may never drop one.
//!
//! The Rust evaluator in `quarrel-core` then runs over what comes back and makes the
//! actual decision. There is still exactly one evaluator, so the Lab and the live sniper
//! remain provably identical — the SQL is an index-assisted narrowing step that cannot
//! change an answer, only the amount of work. Tests assert the containment over random
//! trees against the real store, and assert exactness for trees that are fully pushable.
//!
//! # Why soundness needs care with `NOT`
//!
//! A fragment `f` is *sound* for a condition `c` when `c ⟹ f`. Conjunction and disjunction
//! preserve that. Negation does not: from `c ⟹ f` nothing follows about `¬c ⟹ ¬f`. So a
//! `Not` is pushed only when its child is **exact**, and otherwise degrades to `1`. Each
//! fragment therefore carries whether it is exact, not merely whether it exists.

use quarrel_core::features::{FeeRecipient, Pair};
use quarrel_core::filter::{Condition, EntryFilter};
use rusqlite::types::Value;

/// A `WHERE` fragment and its bound parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct SqlFilter {
    /// A complete boolean expression over the `lab` column aliases. Never empty.
    pub where_clause: String,
    pub params: Vec<Value>,
    /// True when the fragment is logically equal to the filter rather than a superset.
    ///
    /// Reported so a caller can tell the difference between "SQL did the whole job" and
    /// "SQL narrowed and Rust must finish", and so the exactness test knows which trees to
    /// hold to equality.
    pub exact: bool,
}

impl SqlFilter {
    /// The fragment that keeps everything: the identity of AND.
    fn everything() -> Self {
        Self {
            where_clause: "1".into(),
            params: Vec::new(),
            exact: false,
        }
    }

    /// An exact leaf, forced to a definite boolean.
    ///
    /// The `COALESCE` is the whole reason this constructor exists. SQLite is three-valued:
    /// with no `enrichment` row for a launch, `e.website = 1` is NULL, and `NOT NULL` is
    /// NULL, which does not satisfy a `WHERE`. The Rust evaluator has no third value — an
    /// unreadable field refuses, so `Not(RequireWebsite)` *passes* — and the pre-filter was
    /// therefore dropping rows the evaluator accepts. A property test over the real store
    /// caught it on the eighteenth random tree.
    ///
    /// Coalescing at the leaf makes the invariant structural: every fragment is 0 or 1,
    /// `AND`/`OR`/`NOT` of definite values stay definite, and a condition added later
    /// cannot reintroduce the hole without going out of its way.
    fn exact_of(where_clause: String, params: Vec<Value>) -> Self {
        Self {
            where_clause: format!("COALESCE({where_clause}, 0)"),
            params,
            exact: true,
        }
    }
}

/// Column aliases the fragment is written against.
///
/// Kept here rather than inlined in strings so the query in `lab.rs` and the generator
/// cannot drift apart silently.
pub mod col {
    pub const DEPLOYER: &str = "l.deployer";
    pub const PAIR_TOKEN: &str = "l.pair_token";
    pub const TWITTER: &str = "e.twitter";
    pub const WEBSITE: &str = "e.website";
    pub const TELEGRAM: &str = "e.telegram";
    pub const EXEMPT: &str = "e.exempt_wallets";
    pub const CREATOR_TAX: &str = "e.creator_tax_bps";
    pub const DEV_BUY: &str = "e.dev_buy_bps";
    pub const FEE_RECIPIENT: &str = "e.creator_fee_recipient";
    pub const DEPLOYER_LAUNCHES: &str = "p.deployer_launches";
    pub const GRAD_RATE: &str = "p.deployer_grad_rate_bps";
    pub const TWINS: &str = "p.fingerprint_twins_30m";
}

/// Present, as stored by `types::presence_to_i64`.
const PRESENT: i64 = 1;

/// Build a conservative pre-filter for `filter`.
pub fn push_down(filter: &EntryFilter) -> SqlFilter {
    match filter {
        EntryFilter::Cond(c) => condition(c),
        EntryFilter::All(children) => combine(children, "AND", "1"),
        EntryFilter::Any(children) => combine(children, "OR", "0"),
        EntryFilter::Not(child) => {
            let inner = push_down(child);
            // Only an exact child can be negated soundly; see the module note. An exact
            // fragment is already a definite 0 or 1, so this needs no further coalescing.
            if inner.exact {
                SqlFilter {
                    where_clause: format!("NOT ({})", inner.where_clause),
                    params: inner.params,
                    exact: true,
                }
            } else {
                SqlFilter::everything()
            }
        }
    }
}

fn combine(children: &[EntryFilter], op: &str, empty: &str) -> SqlFilter {
    if children.is_empty() {
        // `All([])` passes and `Any([])` refuses — the identities the evaluator uses.
        return SqlFilter {
            where_clause: empty.into(),
            params: Vec::new(),
            exact: true,
        };
    }
    let mut parts = Vec::with_capacity(children.len());
    let mut params = Vec::new();
    let mut exact = true;
    for child in children {
        let f = push_down(child);
        exact &= f.exact;
        parts.push(f.where_clause);
        params.extend(f.params);
    }
    SqlFilter {
        where_clause: format!("({})", parts.join(&format!(" {op} "))),
        params,
        exact,
    }
}

fn int(v: impl Into<i64>) -> Value {
    Value::Integer(v.into())
}

fn condition(c: &Condition) -> SqlFilter {
    match c {
        // A `Presence` other than Present refuses, including Unknown, so this is exact.
        Condition::RequireTwitter => present(col::TWITTER),
        Condition::RequireWebsite => present(col::WEBSITE),
        Condition::RequireTelegram => present(col::TELEGRAM),
        Condition::RequireAnySocial => SqlFilter::exact_of(
            format!(
                "({t} = {PRESENT} OR {w} = {PRESENT} OR {g} = {PRESENT})",
                t = col::TWITTER,
                w = col::WEBSITE,
                g = col::TELEGRAM
            ),
            Vec::new(),
        ),

        // `IS NOT NULL` is not redundant: an unreadable value refuses in Rust, and in
        // SQLite `NULL <= 200` is NULL, which also does not pass a WHERE. Stating it makes
        // the intent legible and keeps the fragment true under `NOT`.
        Condition::DevBuyBps { min, max } => {
            let mut sql = format!("({} IS NOT NULL", col::DEV_BUY);
            let mut params = Vec::new();
            if let Some(lo) = min {
                sql.push_str(&format!(" AND {} >= ?", col::DEV_BUY));
                params.push(int(*lo));
            }
            if let Some(hi) = max {
                sql.push_str(&format!(" AND {} <= ?", col::DEV_BUY));
                params.push(int(*hi));
            }
            sql.push(')');
            SqlFilter::exact_of(sql, params)
        }
        Condition::MaxCreatorTaxBps { bps } => SqlFilter::exact_of(
            format!("({c} IS NOT NULL AND {c} <= ?)", c = col::CREATOR_TAX),
            vec![int(*bps)],
        ),
        Condition::MaxExemptWallets { max } => SqlFilter::exact_of(
            format!("({c} IS NOT NULL AND {c} <= ?)", c = col::EXEMPT),
            vec![int(*max)],
        ),
        Condition::FeeRecipientIs { recipient } => {
            let sql = match recipient {
                FeeRecipient::Deployer => format!(
                    "({r} IS NOT NULL AND {r} = {d})",
                    r = col::FEE_RECIPIENT,
                    d = col::DEPLOYER
                ),
                FeeRecipient::ThirdParty => format!(
                    "({r} IS NOT NULL AND {r} <> {d})",
                    r = col::FEE_RECIPIENT,
                    d = col::DEPLOYER
                ),
                FeeRecipient::Unknown => format!("({r} IS NULL)", r = col::FEE_RECIPIENT),
            };
            SqlFilter::exact_of(sql, Vec::new())
        }

        // NULL here means the deployer has no prior launches, which is the same thing the
        // Rust side reads off `deployer_launches == 0`.
        Condition::MinDeployerGradRateBps {
            bps,
            allow_unproven,
        } => {
            let sql = if *allow_unproven {
                format!("({c} IS NULL OR {c} >= ?)", c = col::GRAD_RATE)
            } else {
                format!("({c} IS NOT NULL AND {c} >= ?)", c = col::GRAD_RATE)
            };
            SqlFilter::exact_of(sql, vec![int(*bps)])
        }
        // A launch with no `pit_features` row reads as zero on the Rust side, so COALESCE
        // rather than an IS NULL test.
        Condition::MaxDeployerLaunches { max } => SqlFilter::exact_of(
            format!("(COALESCE({}, 0) <= ?)", col::DEPLOYER_LAUNCHES),
            vec![int(*max)],
        ),
        Condition::MaxFingerprintTwins { max } => SqlFilter::exact_of(
            format!("(COALESCE({}, 0) <= ?)", col::TWINS),
            vec![int(*max)],
        ),

        Condition::PairIn { pairs } => match pair_keys(pairs) {
            Some(keys) => {
                let holes = vec!["?"; keys.len()].join(", ");
                SqlFilter::exact_of(
                    format!("({} IN ({holes}))", col::PAIR_TOKEN),
                    keys.into_iter().map(Value::Text).collect(),
                )
            }
            // A pair named by a symbol we cannot resolve to an address is not pushable.
            None => SqlFilter::everything(),
        },

        // The one genuinely unpushable condition, and the reason this is a pre-filter
        // rather than a translation. Narrowing on a regex would mean either a registered
        // SQLite function or a prefix heuristic that could drop a matching row.
        Condition::Keyword { .. } => SqlFilter::everything(),
    }
}

fn present(col: &str) -> SqlFilter {
    SqlFilter::exact_of(format!("({col} = {PRESENT})"), Vec::new())
}

/// Map [`Pair`]s onto the stored `pair_token` keys, or `None` if any is not an address.
///
/// Non-ETH pairs are currently labelled by address (see `lab.rs`), so this is a parse. A
/// symbol that cannot be parsed makes the whole condition unpushable rather than silently
/// matching nothing.
fn pair_keys(pairs: &[Pair]) -> Option<Vec<String>> {
    pairs
        .iter()
        .map(|p| match p {
            Pair::Eth => Some(crate::types::addr_key(alloy_primitives::Address::ZERO)),
            Pair::Other(s) => s
                .parse::<alloy_primitives::Address>()
                .ok()
                .map(crate::types::addr_key),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use quarrel_core::pattern::Pattern;

    fn twitter() -> EntryFilter {
        EntryFilter::Cond(Condition::RequireTwitter)
    }

    fn keyword() -> EntryFilter {
        EntryFilter::Cond(Condition::Keyword {
            pattern: Pattern::new("dog").unwrap(),
        })
    }

    #[test]
    fn a_pushable_condition_is_exact() {
        let f = push_down(&twitter());
        assert!(f.exact);
        assert_eq!(f.where_clause, "COALESCE((e.twitter = 1), 0)");
    }

    #[test]
    fn a_regex_keeps_everything_rather_than_guessing() {
        let f = push_down(&keyword());
        assert_eq!(f.where_clause, "1", "must not narrow on a regex");
        assert!(!f.exact);
    }

    #[test]
    fn an_and_of_pushable_and_unpushable_keeps_the_pushable_half() {
        let f = push_down(&EntryFilter::All(vec![twitter(), keyword()]));
        assert_eq!(f.where_clause, "(COALESCE((e.twitter = 1), 0) AND 1)");
        assert!(!f.exact, "the tree as a whole is not fully pushed");
    }

    /// The soundness rule that `Not` exists to protect.
    #[test]
    fn not_over_an_unpushable_child_gives_up_instead_of_inverting() {
        // `NOT 1` would be `0`, which drops every row -- including the ones the evaluator
        // would have passed. Giving up is the only sound answer.
        let f = push_down(&EntryFilter::Not(Box::new(keyword())));
        assert_eq!(f.where_clause, "1");
        assert!(!f.exact);
    }

    #[test]
    fn not_over_an_exact_child_is_pushed_and_stays_exact() {
        let f = push_down(&EntryFilter::Not(Box::new(twitter())));
        assert_eq!(f.where_clause, "NOT (COALESCE((e.twitter = 1), 0))");
        assert!(f.exact);
    }

    #[test]
    fn an_or_with_an_unpushable_branch_keeps_everything() {
        // Any row could satisfy the regex branch, so nothing may be excluded.
        let f = push_down(&EntryFilter::Any(vec![twitter(), keyword()]));
        assert_eq!(f.where_clause, "(COALESCE((e.twitter = 1), 0) OR 1)");
        assert!(!f.exact);
    }

    #[test]
    fn empty_groups_use_the_same_identities_as_the_evaluator() {
        assert_eq!(push_down(&EntryFilter::All(vec![])).where_clause, "1");
        assert_eq!(push_down(&EntryFilter::Any(vec![])).where_clause, "0");
    }

    #[test]
    fn thresholds_are_bound_as_parameters_not_interpolated() {
        let f = push_down(&EntryFilter::Cond(Condition::MaxCreatorTaxBps { bps: 250 }));
        assert!(!f.where_clause.contains("250"), "no literal in the SQL");
        assert_eq!(f.params, vec![Value::Integer(250)]);
    }

    #[test]
    fn eth_maps_onto_the_zero_address() {
        let f = push_down(&EntryFilter::Cond(Condition::PairIn {
            pairs: vec![Pair::Eth],
        }));
        assert_eq!(
            f.params,
            vec![Value::Text(
                "0x0000000000000000000000000000000000000000".into()
            )]
        );
    }

    #[test]
    fn an_unresolvable_pair_symbol_keeps_everything_rather_than_matching_nothing() {
        let f = push_down(&EntryFilter::Cond(Condition::PairIn {
            pairs: vec![Pair::Other("USDC".into())],
        }));
        assert_eq!(f.where_clause, "1");
    }

    #[test]
    fn a_missing_features_row_reads_as_zero_on_both_sides() {
        // The Rust evaluator hydrates an absent pit_features row as zeroes, so the SQL
        // must not drop those launches on a ceiling condition.
        let f = push_down(&EntryFilter::Cond(Condition::MaxDeployerLaunches {
            max: 3,
        }));
        assert!(f.where_clause.contains("COALESCE"));
    }
}
