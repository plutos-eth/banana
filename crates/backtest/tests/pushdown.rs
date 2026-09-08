//! PLAN.md D6: the SQL pre-filter and the Rust evaluator, checked against each other.
//!
//! D6 was written as "the SQL generator and the Rust evaluator agree". Building it showed
//! that "agree" cannot be the contract: [`Condition::Keyword`] is a regex, and SQLite has
//! no regex without a registered function, so a generator obliged to be *equal* to the
//! evaluator either cannot exist or has to drag a regex engine into the query planner.
//!
//! The contract is one-sided instead, and stronger where it matters:
//!
//! * **Soundness, always.** Every launch the Rust evaluator passes is returned by the SQL.
//!   This is what makes the pre-filter incapable of changing an answer.
//! * **Equality when the tree is fully pushable**, which is most of them.
//!
//! Checked over 100 random predicate trees. The generator deliberately produces trees the
//! SQL cannot fully push — regexes, and `Not` over them — because those are where an
//! unsound optimisation would hide.

mod support;

use banana_core::features::{FeeRecipient, Pair, Presence};
use banana_core::filter::{Condition, EntryFilter};
use banana_core::pattern::Pattern;
use banana_store::History;
use support::{Fixture, Launch};

/// 100 trees, as the acceptance criterion asks for.
const TREES: usize = 100;

/// xorshift64*, so the trees are random but the test is not flaky.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

fn condition(rng: &mut Rng) -> Condition {
    match rng.below(13) {
        0 => Condition::RequireTwitter,
        1 => Condition::RequireWebsite,
        2 => Condition::RequireTelegram,
        3 => Condition::RequireAnySocial,
        4 => Condition::DevBuyBps {
            min: rng.chance(70).then(|| rng.below(600) as u32),
            max: rng.chance(70).then(|| 400 + rng.below(1_000) as u32),
        },
        5 => Condition::MaxCreatorTaxBps {
            bps: rng.below(1_000) as u32,
        },
        6 => Condition::MaxExemptWallets {
            max: rng.below(6) as u32,
        },
        7 => Condition::FeeRecipientIs {
            recipient: match rng.below(3) {
                0 => FeeRecipient::Deployer,
                1 => FeeRecipient::ThirdParty,
                _ => FeeRecipient::Unknown,
            },
        },
        8 => Condition::MinDeployerGradRateBps {
            bps: rng.below(10_000) as u32,
            allow_unproven: rng.chance(50),
        },
        9 => Condition::MaxDeployerLaunches {
            max: rng.below(5) as u32,
        },
        10 => Condition::MaxFingerprintTwins {
            max: rng.below(4) as u32,
        },
        11 => Condition::PairIn {
            pairs: if rng.chance(60) {
                vec![Pair::Eth]
            } else {
                vec![
                    Pair::Eth,
                    Pair::Other(format!("{:#x}", support::addr_n(77))),
                ]
            },
        },
        // The unpushable one. Kept common on purpose: it is where an unsound narrowing
        // would show up.
        _ => Condition::Keyword {
            pattern: Pattern::new(["waffle", "dog", "^Space", "[0-9]+"][rng.below(4) as usize])
                .unwrap(),
        },
    }
}

fn tree(rng: &mut Rng, depth: u32) -> EntryFilter {
    if depth == 0 || rng.chance(40) {
        return EntryFilter::Cond(condition(rng));
    }
    match rng.below(3) {
        0 => EntryFilter::All(
            (0..1 + rng.below(3))
                .map(|_| tree(rng, depth - 1))
                .collect(),
        ),
        1 => EntryFilter::Any(
            (0..1 + rng.below(3))
                .map(|_| tree(rng, depth - 1))
                .collect(),
        ),
        _ => EntryFilter::Not(Box::new(tree(rng, depth - 1))),
    }
}

/// A store with enough variety that the conditions actually discriminate.
fn varied_store(n: u64) -> History {
    let mut rng = Rng(0xDEAD_BEEF);
    let mut f = Fixture::new(0, 5_000_000);
    for i in 0..n {
        let decoded = rng.chance(90);
        f.add(Launch {
            block: 1_000 + i * 7,
            deployer: 1 + rng.below(40),
            pair_token: if rng.chance(60) {
                alloy_primitives::Address::ZERO
            } else {
                support::addr_n(77)
            },
            twitter: match rng.below(3) {
                0 => Presence::Present,
                1 => Presence::Absent,
                _ => Presence::Unknown,
            },
            dev_buy_bps: rng.chance(85).then(|| rng.below(900) as u32),
            creator_tax_bps: rng.chance(90).then(|| rng.below(1_000) as u32),
            exempt_wallets: rng.chance(90).then(|| rng.below(5) as u32),
            decoded,
            name: ["SpaceWaffle", "dogcoin", "PLAIN", "waffle99"][rng.below(4) as usize].into(),
            deployer_launches: rng.below(6) as u32,
            deployer_graduations: rng.below(3) as u32,
            twins: rng.below(5) as u32,
            depth_blocks: rng.below(2_000_000),
            // A launch with no enrichment row at all. The first version of this fixture
            // had none, which is why the three-valued-logic bug survived until the real
            // store was tried: there, the calldata phase had not caught up yet.
            no_enrichment: rng.chance(8),
            ..Launch::at(1_000 + i * 7)
        });
    }
    f.finish()
}

/// The property, run over one store.
///
/// Returns `(rows the SQL returned, rows the evaluator passed)` summed over every tree, so
/// the caller can report how tight the narrowing was.
fn check(history: &History, label: &str) -> (u64, u64) {
    let all = history.candidates(None).unwrap();
    // The snapshot is taken once and every comparison is restricted to it, so an index
    // still writing into the same database can only add tokens this check ignores. Without
    // that, the test failed on a race rather than on a property.
    let known: std::collections::HashSet<_> = all.iter().map(|c| c.token).collect();
    let mut rng = Rng(0x5EED_1234);
    let (mut returned, mut passed) = (0u64, 0u64);
    let mut exact_trees = 0;

    for i in 0..TREES {
        let t = tree(&mut rng, 3);
        let sql = banana_store::push_down(&t);
        let rows = history.candidates(Some(&sql)).unwrap();
        let returned_tokens: std::collections::HashSet<_> = rows
            .iter()
            .map(|c| c.token)
            .filter(|t| known.contains(t))
            .collect();

        // Soundness: nothing the evaluator would have passed may be missing.
        let mut expected = 0u64;
        for c in &all {
            if t.evaluate(&c.features).passed {
                expected += 1;
                assert!(
                    returned_tokens.contains(&c.token),
                    "{label} tree {i}: the pre-filter dropped a launch the evaluator passes.\n\
                     filter: {}\nsql: {}",
                    serde_json::to_string(&t).unwrap(),
                    sql.where_clause
                );
            }
        }

        // Exactness, where the whole tree was pushable.
        if sql.exact {
            exact_trees += 1;
            assert_eq!(
                returned_tokens.len() as u64,
                expected,
                "{label} tree {i}: an exact pre-filter returned rows the evaluator refuses.\n\
                 filter: {}\nsql: {}",
                serde_json::to_string(&t).unwrap(),
                sql.where_clause
            );
        }

        returned += rows.len() as u64;
        passed += expected;
    }

    assert!(
        exact_trees > TREES / 4,
        "{label}: only {exact_trees} of {TREES} trees were fully pushable, which suggests \
         the generator stopped being able to push anything"
    );
    (returned, passed)
}

#[test]
fn the_pre_filter_never_drops_a_launch_the_evaluator_would_pass() {
    let h = varied_store(2_000);
    let (returned, passed) = check(&h, "synthetic");
    // Not an assertion about a specific ratio -- just a floor that would catch a
    // degenerate generator that pushed nothing at all.
    assert!(
        returned < passed * 40 + 40_000,
        "the pre-filter returned {returned} rows to yield {passed}, which is barely a filter"
    );
    println!("synthetic store: SQL returned {returned} rows for {passed} passing");
}

/// The acceptance criterion asks for the real store. Runs when one has been indexed.
#[test]
fn the_same_property_holds_over_the_indexed_window() {
    let path = std::path::Path::new("../../data/lab/history.db");
    if !path.exists() {
        println!(
            "no indexed store at {}; synthetic coverage only",
            path.display()
        );
        return;
    }
    // An index in flight writes enrichment, features and outcomes into rows this test has
    // already snapshotted, so the two queries would see different databases and the
    // failure would be a race rather than a property. The writer lock already says whether
    // that is happening.
    if !banana_store::Lock::is_free(path) {
        println!("an index holds the writer lock; skipping the real-store check");
        return;
    }
    let h = History::open_read_only(path).unwrap();
    let (returned, passed) = check(&h, "real");
    println!("real store: SQL returned {returned} rows for {passed} passing");
}
