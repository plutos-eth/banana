//! A store built by hand, so the guards can be tested without an indexed window.
//!
//! Shared by the guard, denominator and pushdown tests. Everything is written through the
//! real `quarrel-store` API rather than raw SQL, so a schema change breaks the fixture
//! rather than letting the tests drift away from what the indexer actually writes.

#![allow(dead_code)]

use alloy_primitives::{Address, B256, U256};
use quarrel_core::features::{Presence, Socials};
use quarrel_store::History;
use quarrel_store::history::{EnrichmentRow, LaunchRow, OutcomeRow, PhaseState, PitFeaturesRow};
use quarrel_store::types::EntryRule;

pub fn addr_n(n: u64) -> Address {
    let mut b = [0u8; 20];
    b[12..].copy_from_slice(&n.to_be_bytes());
    Address::from(b)
}

/// One launch, described by everything a test might want to vary.
#[derive(Debug, Clone)]
pub struct Launch {
    pub block: u64,
    pub deployer: u64,
    pub pair_token: Address,
    pub twitter: Presence,
    pub dev_buy_bps: Option<u32>,
    pub creator_tax_bps: Option<u32>,
    pub exempt_wallets: Option<u32>,
    /// Written to `enrichment` as decoded or not. An undecodable launch has no calldata
    /// fields at all, which is the case the `Unknown` states exist for.
    pub decoded: bool,
    pub name: String,
    pub deployer_launches: u32,
    pub deployer_graduations: u32,
    pub twins: u32,
    pub depth_blocks: u64,
    /// `false` writes an outcome with no entry price: the F9 refusal case.
    pub has_entry: bool,
    pub mult_5m_bps: Option<u64>,
    pub mult_30m_bps: Option<u64>,
    pub max_multiple_bps: Option<u64>,
    pub migrated: bool,
    /// No `outcomes` row at all.
    pub no_outcome: bool,
    /// No `enrichment` row at all, as when the calldata phase was skipped or interrupted.
    ///
    /// This is the case the SQL pre-filter got wrong: with the row absent every enrichment
    /// column is NULL through the LEFT JOIN, and SQLite's three-valued logic made
    /// `NOT (col = 1)` neither true nor false.
    pub no_enrichment: bool,
}

impl Default for Launch {
    fn default() -> Self {
        Self {
            block: 1_000,
            deployer: 1,
            pair_token: Address::ZERO,
            twitter: Presence::Present,
            dev_buy_bps: Some(300),
            creator_tax_bps: Some(100),
            exempt_wallets: Some(0),
            decoded: true,
            name: "SpaceWaffle".into(),
            deployer_launches: 0,
            deployer_graduations: 0,
            twins: 0,
            depth_blocks: 1_000_000,
            has_entry: true,
            mult_5m_bps: Some(9_600),
            mult_30m_bps: Some(9_000),
            max_multiple_bps: Some(10_000),
            migrated: false,
            no_outcome: false,
            no_enrichment: false,
        }
    }
}

impl Launch {
    pub fn at(block: u64) -> Self {
        Self {
            block,
            ..Default::default()
        }
    }

    pub fn by(mut self, deployer: u64) -> Self {
        self.deployer = deployer;
        self
    }

    pub fn winner(mut self) -> Self {
        self.mult_5m_bps = Some(30_000);
        self.mult_30m_bps = Some(25_000);
        self.max_multiple_bps = Some(40_000);
        self
    }

    pub fn no_twitter(mut self) -> Self {
        self.twitter = Presence::Absent;
        self
    }
}

/// Assembles a `history.db` in memory.
pub struct Fixture {
    pub history: History,
    next: u64,
    from_block: u64,
    to_block: u64,
    tokens: Vec<Address>,
}

impl Fixture {
    pub fn new(from_block: u64, to_block: u64) -> Self {
        Self {
            history: History::in_memory().unwrap(),
            next: 1,
            from_block,
            to_block,
            tokens: Vec::new(),
        }
    }

    pub fn add(&mut self, l: Launch) -> Address {
        let token = addr_n(self.next + 1_000_000);
        let curve = addr_n(self.next + 2_000_000);
        let deployer = addr_n(l.deployer);
        self.next += 1;
        self.tokens.push(token);

        self.history
            .insert_launches(&[LaunchRow {
                token,
                curve,
                deployer,
                pair_token: l.pair_token,
                launch_config_id: 0,
                graduation_threshold: U256::from(4_200_000_000_000_000_000u64),
                block: l.block,
                tx_hash: B256::from(U256::from(self.next)),
                log_index: 0,
            }])
            .unwrap();

        if !l.no_enrichment {
            let socials = if l.decoded {
                Socials {
                    twitter: l.twitter,
                    website: Presence::Absent,
                    telegram: Presence::Absent,
                }
            } else {
                Socials::UNKNOWN
            };
            self.history
                .insert_enrichment(&[EnrichmentRow {
                    token,
                    decoded: l.decoded,
                    selector: None,
                    name: l.decoded.then(|| l.name.clone()),
                    symbol: l.decoded.then(|| "WAFFLE".to_string()),
                    description: l.decoded.then(String::new),
                    logo: None,
                    twitter_url: None,
                    website_url: None,
                    telegram_url: None,
                    socials,
                    exempt_wallets: l.decoded.then_some(l.exempt_wallets).flatten(),
                    creator_fee_recipient: l.decoded.then_some(deployer),
                    creator_tax_bps: l.decoded.then_some(l.creator_tax_bps).flatten(),
                    declared_quote_in: None,
                    dev_buy_quote: None,
                    dev_buy_tokens: None,
                    dev_buy_bps: l.dev_buy_bps,
                }])
                .unwrap();
        }

        self.history
            .upsert_pit_features(&PitFeaturesRow {
                token,
                deployer_launches: l.deployer_launches,
                deployer_graduations: l.deployer_graduations,
                deployer_grad_rate_bps: (l.deployer_launches > 0)
                    .then(|| l.deployer_graduations * 10_000 / l.deployer_launches),
                fingerprint: format!("{}|{}|1??|?", l.dev_buy_bps.unwrap_or(0), l.deployer),
                fingerprint_twins_30m: l.twins,
                deployer_history_depth_blocks: l.depth_blocks,
            })
            .unwrap();

        if !l.no_outcome {
            self.history
                .upsert_outcome(&OutcomeRow {
                    token,
                    entry_rule: EntryRule::ObservedUntaxedBuy,
                    entry_block: l.has_entry.then_some(l.block + 40),
                    entry_price: l.has_entry.then(|| U256::from(1_000_000_000u64)),
                    entry_tokens: l.has_entry.then(|| U256::from(1_000u64)),
                    ath_price: None,
                    ath_block: None,
                    max_multiple_bps: l.has_entry.then_some(l.max_multiple_bps).flatten(),
                    time_to_ath_s: None,
                    mult_after_5m_bps: l.has_entry.then_some(l.mult_5m_bps).flatten(),
                    mult_after_30m_bps: l.has_entry.then_some(l.mult_30m_bps).flatten(),
                    migrated: l.migrated,
                    died: false,
                    distinct_buyers_1m: None,
                    every_early_buy_taxed: None,
                    post_entry_trades: 4,
                    last_trade_block: Some(l.block + 2_000),
                    observed_blocks: 2_000,
                })
                .unwrap();
        }
        token
    }

    /// Record the window, the way a real index does, and hand back the store.
    pub fn finish(mut self) -> History {
        self.history
            .checkpoint(
                "launches",
                PhaseState {
                    from_block: self.from_block,
                    last_block: self.to_block,
                    target_block: self.to_block,
                    rows_written: self.tokens.len() as u64,
                },
            )
            .unwrap();
        self.history
    }
}
