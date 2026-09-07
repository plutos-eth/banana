//! The Strategy Lab's light half: **one row per launch, never one per trade**.
//!
//! PLAN.md D2 said this query would touch `outcomes` and `pit_features` only. Building it
//! showed that is one table short of the truth in one direction and one short in the
//! other: the filterable features also need `launches` (the pair) and `enrichment` (name,
//! socials, dev buy, creator tax, exempt wallets). All four are one row per launch, so the
//! reasoning behind D2 is untouched — what it is really claiming is that the Lab never
//! reads `trades`, which is the ~1 M-rows-a-day table. That still holds, and it is why
//! §4.1's "no DuckDB" call stands.
//!
//! The joins are `LEFT`, deliberately. A launch whose calldata never decoded still has a
//! row here, with unknowns where the data would be; dropping it would shrink the
//! denominator for a reason that is about our decoder rather than about the token.

use alloy_primitives::Address;
use quarrel_core::features::{FeeRecipient, Pair, PitFeatures, Socials};
use rusqlite::types::Value;

use crate::history::History;
use crate::sql::SqlFilter;
use crate::types::{EntryRule, presence_from_i64};
use crate::{Result, StoreError};

/// One launch, as the Lab sees it: what a filter may read, and what became of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub token: Address,
    pub launch_block: u64,
    /// Everything an entry filter is allowed to read. Point-in-time by construction.
    pub features: PitFeatures,
    pub outcome: Outcome,
}

/// The precomputed fate of a launch, reduced to what the Lab measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    pub entry_rule: EntryRule,
    /// False when the curve could not be replayed exactly, so no entry price was invented.
    ///
    /// These launches are **not** dropped: they passed the filter and the sniper would
    /// have entered them, so they stay in the funnel as their own stage (PLAN.md F9).
    pub has_entry: bool,
    pub max_multiple_bps: Option<u64>,
    pub mult_after_5m_bps: Option<u64>,
    pub mult_after_30m_bps: Option<u64>,
    pub migrated: bool,
    pub died: bool,
    pub post_entry_trades: u64,
}

/// The block range the store actually covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub from_block: u64,
    pub to_block: u64,
}

impl Window {
    pub fn blocks(&self) -> u64 {
        self.to_block.saturating_sub(self.from_block) + 1
    }
}

/// How many launches survive the funnel's window-level stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UniverseCounts {
    /// Every launch in the store.
    pub all: u64,
    /// Those the window watched for long enough to judge (spec §5.5).
    pub matured: u64,
    /// Those that also had enough visible deployer history (PLAN.md C2).
    pub matured_and_deep: u64,
}

/// The `FROM` clause every Lab query shares. Aliases match `sql::col`.
const FROM: &str = "FROM launches l
     LEFT JOIN enrichment   e ON e.token = l.token
     LEFT JOIN pit_features p ON p.token = l.token
     LEFT JOIN outcomes     o ON o.token = l.token";

const COLUMNS: &str = "l.token, l.block, l.pair_token,
     e.name, e.symbol, e.description,
     e.twitter, e.website, e.telegram,
     e.exempt_wallets, e.dev_buy_bps, e.creator_tax_bps, e.creator_fee_recipient, l.deployer,
     COALESCE(p.deployer_launches, 0), COALESCE(p.deployer_graduations, 0),
     COALESCE(p.fingerprint_twins_30m, 0), COALESCE(p.deployer_history_depth_blocks, 0),
     o.entry_rule, o.entry_price,
     o.max_multiple_bps, o.mult_after_5m_bps, o.mult_after_30m_bps,
     COALESCE(o.migrated, 0), COALESCE(o.died, 0), COALESCE(o.post_entry_trades, 0)";

impl History {
    /// The window the index covers, from the recorded phase state.
    ///
    /// Falls back to the launches themselves when no phase state exists, so an in-memory
    /// store assembled by a test still reports a sane window.
    pub fn window(&self) -> Result<Option<Window>> {
        let state: (Option<u64>, Option<u64>) = self.conn().query_row(
            "SELECT min(from_block), max(target_block) FROM index_state",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let launches: (Option<u64>, Option<u64>) =
            self.conn()
                .query_row("SELECT min(block), max(block) FROM launches", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?;

        // The launches are ground truth for the start: a launch at block B proves B was
        // scanned. Belt and braces for the resume case that `History::checkpoint` fixes at
        // the source, and it also covers a store a test assembled with no phase state.
        let from = match (state.0, launches.0) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        // The end stays whatever the index targeted. A window can legitimately run past its
        // last launch, and shortening it to the last launch would hide those final blocks
        // of silence from the maturity cutoff.
        let to = state.1.or(launches.1);
        Ok(from.zip(to).map(|(from_block, to_block)| Window {
            from_block,
            to_block,
        }))
    }

    /// The funnel's early stages, counted over the **whole** store in one query.
    ///
    /// Counted here rather than over the pre-filtered candidate set so that "matured"
    /// means matured, and not "matured, among the rows the pre-filter happened to return".
    ///
    /// Maturity is a property of the **window**, not of the token: a launch is mature when
    /// the index kept watching for `maturity_blocks` after it, whatever the token did in
    /// that time. Gating on the token's own trade history instead would drop the launches
    /// that died in ninety seconds — precisely the failures — and leave a universe of
    /// survivors (spec §5.5).
    pub fn universe_counts(
        &self,
        to_block: u64,
        maturity_blocks: u64,
        depth_floor: u64,
    ) -> Result<UniverseCounts> {
        let row = self.conn().query_row(
            "SELECT count(*),
                    sum(?1 - l.block >= ?2),
                    sum(?1 - l.block >= ?2
                        AND COALESCE(p.deployer_history_depth_blocks, 0) >= ?3)
             FROM launches l LEFT JOIN pit_features p ON p.token = l.token",
            rusqlite::params![to_block, maturity_blocks, depth_floor],
            |r| {
                Ok(UniverseCounts {
                    all: r.get(0)?,
                    // NULL when there are no rows to sum, which is zero of them.
                    matured: r.get::<_, Option<u64>>(1)?.unwrap_or(0),
                    matured_and_deep: r.get::<_, Option<u64>>(2)?.unwrap_or(0),
                })
            },
        )?;
        Ok(row)
    }

    /// Every launch in the store, narrowed by an optional SQL pre-filter.
    ///
    /// The pre-filter never decides: it is sound by construction (see [`crate::sql`]) and
    /// the caller's Rust evaluator still runs over everything returned.
    pub fn candidates(&self, prefilter: Option<&SqlFilter>) -> Result<Vec<Candidate>> {
        let (clause, params): (&str, &[Value]) = match prefilter {
            Some(f) => (f.where_clause.as_str(), f.params.as_slice()),
            None => ("1", &[]),
        };
        let sql = format!("SELECT {COLUMNS} {FROM} WHERE {clause} ORDER BY l.block, l.token");
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
            Ok(RawCandidate {
                token: r.get(0)?,
                block: r.get(1)?,
                pair_token: r.get(2)?,
                name: r.get(3)?,
                symbol: r.get(4)?,
                description: r.get(5)?,
                twitter: r.get(6)?,
                website: r.get(7)?,
                telegram: r.get(8)?,
                exempt_wallets: r.get(9)?,
                dev_buy_bps: r.get(10)?,
                creator_tax_bps: r.get(11)?,
                fee_recipient: r.get(12)?,
                deployer: r.get(13)?,
                deployer_launches: r.get(14)?,
                deployer_graduations: r.get(15)?,
                twins: r.get(16)?,
                depth: r.get(17)?,
                entry_rule: r.get(18)?,
                entry_price: r.get(19)?,
                max_multiple_bps: r.get(20)?,
                mult_5m: r.get(21)?,
                mult_30m: r.get(22)?,
                migrated: r.get(23)?,
                died: r.get(24)?,
                post_entry_trades: r.get(25)?,
            })
        })?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row?.hydrate()?);
        }
        Ok(out)
    }
}

/// The raw column tuple, kept separate so hydration is one readable function.
struct RawCandidate {
    token: String,
    block: u64,
    pair_token: String,
    name: Option<String>,
    symbol: Option<String>,
    description: Option<String>,
    twitter: Option<i64>,
    website: Option<i64>,
    telegram: Option<i64>,
    exempt_wallets: Option<u32>,
    dev_buy_bps: Option<u32>,
    creator_tax_bps: Option<u32>,
    fee_recipient: Option<String>,
    deployer: String,
    deployer_launches: u32,
    deployer_graduations: u32,
    twins: u32,
    depth: u64,
    entry_rule: Option<i64>,
    entry_price: Option<Vec<u8>>,
    max_multiple_bps: Option<u64>,
    mult_5m: Option<u64>,
    mult_30m: Option<u64>,
    migrated: i64,
    died: i64,
    post_entry_trades: u64,
}

impl RawCandidate {
    fn hydrate(self) -> Result<Candidate> {
        let token = parse_addr(&self.token)?;
        let pair_token = parse_addr(&self.pair_token)?;
        let deployer = parse_addr(&self.deployer)?;

        // A missing enrichment row means the launch transaction was never read. Every
        // field it would have supplied is Unknown, never a default: a `NULL` presence
        // decodes to `Unknown` and a `NULL` count stays `None`, so a ceiling rule refuses
        // rather than passing a launch whose value was never seen.
        let socials = Socials {
            twitter: presence_from_i64(self.twitter.unwrap_or(2)),
            website: presence_from_i64(self.website.unwrap_or(2)),
            telegram: presence_from_i64(self.telegram.unwrap_or(2)),
        };
        let fee_recipient = match self.fee_recipient.as_deref() {
            None => FeeRecipient::Unknown,
            Some(r) => {
                if parse_addr(r)? == deployer {
                    FeeRecipient::Deployer
                } else {
                    FeeRecipient::ThirdParty
                }
            }
        };

        let features = PitFeatures {
            // Non-ETH pairs are labelled by address for now. Resolving a symbol needs one
            // `symbol()` read per distinct pair token; until then an address is an
            // unfriendly but correct identifier, and it is what `sql::pair_keys` matches.
            pair: if pair_token.is_zero() {
                Pair::Eth
            } else {
                Pair::Other(crate::types::addr_key(pair_token))
            },
            name: self.name.unwrap_or_default(),
            symbol: self.symbol.unwrap_or_default(),
            description: self.description.unwrap_or_default(),
            socials,
            exempt_wallets: self.exempt_wallets,
            dev_buy_bps: self.dev_buy_bps,
            creator_tax_bps: self.creator_tax_bps,
            fee_recipient,
            deployer_launches: self.deployer_launches,
            deployer_graduations: self.deployer_graduations,
            fingerprint_twins_30m: self.twins,
            deployer_history_depth_blocks: self.depth,
        };

        Ok(Candidate {
            token,
            launch_block: self.block,
            features,
            outcome: Outcome {
                entry_rule: self
                    .entry_rule
                    .and_then(EntryRule::from_i64)
                    .unwrap_or(EntryRule::ObservedUntaxedBuy),
                has_entry: self.entry_price.is_some(),
                max_multiple_bps: self.max_multiple_bps,
                mult_after_5m_bps: self.mult_5m,
                mult_after_30m_bps: self.mult_30m,
                migrated: self.migrated != 0,
                died: self.died != 0,
                post_entry_trades: self.post_entry_trades,
            },
        })
    }
}

fn parse_addr(s: &str) -> Result<Address> {
    s.parse()
        .map_err(|_| StoreError::Corrupt(format!("not an address: {s}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::{EnrichmentRow, LaunchRow};
    use alloy_primitives::{B256, U256};

    fn addr(n: u8) -> Address {
        Address::repeat_byte(n)
    }

    fn store_with_one_launch(decoded: bool) -> History {
        let mut h = History::in_memory().unwrap();
        h.insert_launches(&[LaunchRow {
            token: addr(1),
            curve: addr(2),
            deployer: addr(3),
            pair_token: Address::ZERO,
            launch_config_id: 0,
            graduation_threshold: U256::from(42u64),
            block: 100,
            tx_hash: B256::repeat_byte(9),
            log_index: 0,
        }])
        .unwrap();
        if decoded {
            h.insert_enrichment(&[EnrichmentRow {
                token: addr(1),
                decoded: true,
                selector: None,
                name: Some("SpaceWaffle".into()),
                symbol: Some("WAFFLE".into()),
                description: Some(String::new()),
                logo: None,
                twitter_url: Some("https://x.com/x".into()),
                website_url: None,
                telegram_url: None,
                socials: Socials {
                    twitter: quarrel_core::features::Presence::Present,
                    website: quarrel_core::features::Presence::Absent,
                    telegram: quarrel_core::features::Presence::Absent,
                },
                exempt_wallets: Some(1),
                creator_fee_recipient: Some(addr(3)),
                creator_tax_bps: Some(100),
                declared_quote_in: None,
                dev_buy_quote: Some(U256::from(5u64)),
                dev_buy_tokens: Some(U256::from(5u64)),
                dev_buy_bps: Some(300),
            }])
            .unwrap();
        }
        h
    }

    #[test]
    fn a_decoded_launch_hydrates_every_filterable_field() {
        let h = store_with_one_launch(true);
        let c = &h.candidates(None).unwrap()[0];
        assert_eq!(c.features.pair, Pair::Eth);
        assert_eq!(c.features.symbol, "WAFFLE");
        assert_eq!(c.features.exempt_wallets, Some(1));
        assert_eq!(c.features.dev_buy_bps, Some(300));
        assert_eq!(c.features.creator_tax_bps, Some(100));
        assert_eq!(c.features.fee_recipient, FeeRecipient::Deployer);
    }

    /// The join is LEFT for a reason: this launch must still be in the universe.
    #[test]
    fn a_launch_with_no_enrichment_is_kept_and_reads_as_unknown_not_as_zero() {
        let h = store_with_one_launch(false);
        let rows = h.candidates(None).unwrap();
        assert_eq!(rows.len(), 1, "the launch must not vanish");
        let f = &rows[0].features;
        assert_eq!(f.socials, Socials::UNKNOWN);
        assert_eq!(f.exempt_wallets, None, "unknown, not an empty bundle");
        assert_eq!(f.creator_tax_bps, None);
        assert_eq!(f.dev_buy_bps, None);
        assert_eq!(f.fee_recipient, FeeRecipient::Unknown);
    }

    #[test]
    fn a_launch_with_no_outcome_row_has_no_entry_rather_than_a_zero_multiple() {
        let h = store_with_one_launch(true);
        let o = h.candidates(None).unwrap()[0].outcome;
        assert!(!o.has_entry);
        assert_eq!(o.max_multiple_bps, None, "absent, not 1.00x");
        assert!(!o.migrated);
    }

    #[test]
    fn a_third_party_fee_recipient_is_distinguished_from_the_deployer() {
        let h = store_with_one_launch(true);
        h.conn()
            .execute(
                "UPDATE enrichment SET creator_fee_recipient = ?1",
                [crate::types::addr_key(addr(7))],
            )
            .unwrap();
        let c = &h.candidates(None).unwrap()[0];
        assert_eq!(c.features.fee_recipient, FeeRecipient::ThirdParty);
    }

    #[test]
    fn the_window_falls_back_to_the_launches_when_no_phase_state_exists() {
        let h = store_with_one_launch(true);
        let w = h.window().unwrap().unwrap();
        assert_eq!(w.from_block, 100);
        assert_eq!(w.to_block, 100);
    }

    /// The defect this rule exists for, reproduced.
    #[test]
    fn a_resumed_index_does_not_shrink_the_window_to_a_point() {
        let mut h = store_with_one_launch(true);
        // A first run covering 0..1000.
        h.checkpoint(
            "launches",
            crate::history::PhaseState {
                from_block: 0,
                last_block: 1_000,
                target_block: 1_000,
                rows_written: 1,
            },
        )
        .unwrap();
        // Interrupted and resumed: this run starts where the last one stopped.
        h.checkpoint(
            "launches",
            crate::history::PhaseState {
                from_block: 1_000,
                last_block: 1_000,
                target_block: 1_000,
                rows_written: 1,
            },
        )
        .unwrap();

        let w = h.window().unwrap().unwrap();
        assert_eq!(w.from_block, 0, "the resume must not overwrite the start");
        assert_eq!(w.to_block, 1_000);
    }

    #[test]
    fn indexing_a_genuinely_new_window_replaces_the_old_one() {
        let mut h = store_with_one_launch(true);
        for (from, to) in [(0u64, 1_000u64), (2_000, 3_000)] {
            h.checkpoint(
                "launches",
                crate::history::PhaseState {
                    from_block: from,
                    last_block: to,
                    target_block: to,
                    rows_written: 1,
                },
            )
            .unwrap();
        }
        // A different target means a different window, not a resume. The launch at block
        // 100 still floors the start, because that block demonstrably was scanned.
        let w = h.window().unwrap().unwrap();
        assert_eq!(w.to_block, 3_000);
        assert_eq!(w.from_block, 100);
    }

    #[test]
    fn an_empty_store_has_no_window_rather_than_a_zero_length_one() {
        let h = History::in_memory().unwrap();
        assert_eq!(h.window().unwrap(), None);
    }
}
