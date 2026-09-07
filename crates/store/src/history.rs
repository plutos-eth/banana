//! `history.db` — append-only indexed facts.
//!
//! Writes are idempotent by construction: every insert is `ON CONFLICT DO NOTHING` against
//! the natural key `(tx_hash, log_index)`, so re-indexing a range that is already covered
//! writes zero rows rather than duplicating or erroring (spec §6.2).
//!
//! Batches run inside one transaction. At ~1M trade rows per indexed day, committing per
//! row would make the trade scan disk-bound rather than network-bound.

use std::path::{Path, PathBuf};

use alloy_primitives::{Address, B256, U256};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::types::{
    Side, addr_key, hash_key, presence_from_i64, presence_to_i64, u256_from_blob, u256_to_blob,
};
use crate::{Result, StoreError, schema};

/// One `TokenLaunched`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchRow {
    pub token: Address,
    pub curve: Address,
    pub deployer: Address,
    pub pair_token: Address,
    pub launch_config_id: u64,
    pub graduation_threshold: U256,
    pub block: u64,
    pub tx_hash: B256,
    pub log_index: u64,
}

/// One `CurveBuy` or `CurveSell`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TradeRow {
    pub tx_hash: B256,
    pub log_index: u64,
    pub curve: Address,
    pub block: u64,
    pub tx_index: u64,
    pub side: Side,
    pub actor: Address,
    pub recipient: Address,
    pub amount_in: U256,
    pub amount_out: U256,
    pub fee: U256,
    pub tax: U256,
    /// From a `SnipeTaxCharged` by the same curve in the same transaction.
    ///
    /// `None` means the buy was **not** snipe-taxed, which is how the end of the opening
    /// tax window is identified (PLAN.md F2).
    pub snipe_tax: Option<U256>,
}

/// What the launch calldata declared, plus the dev buy read from the launch transaction's
/// own `CurveBuy`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrichmentRow {
    pub token: Address,
    pub decoded: bool,
    pub selector: Option<String>,
    pub name: Option<String>,
    pub symbol: Option<String>,
    pub description: Option<String>,
    pub logo: Option<String>,
    pub twitter_url: Option<String>,
    pub website_url: Option<String>,
    pub telegram_url: Option<String>,
    pub socials: quarrel_core::features::Socials,
    /// `None` when the launch did not decode: an unknown bundle size, not a zero one.
    pub exempt_wallets: Option<u32>,
    pub creator_fee_recipient: Option<Address>,
    pub creator_tax_bps: Option<u32>,
    pub declared_quote_in: Option<U256>,
    pub dev_buy_quote: Option<U256>,
    pub dev_buy_tokens: Option<U256>,
    pub dev_buy_bps: Option<u32>,
}

/// Where a phase got to, so it can resume there (spec §6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhaseState {
    pub from_block: u64,
    pub last_block: u64,
    pub target_block: u64,
    pub rows_written: u64,
}

/// The append-only history store.
#[derive(Debug)]
pub struct History {
    conn: Connection,
    path: PathBuf,
    read_only: bool,
}

impl History {
    /// Open for writing. The caller is expected to already hold the writer lock.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&path)?;
        schema::apply_pragmas(&conn)?;
        schema::migrate(&conn)?;
        Ok(Self {
            conn,
            path,
            read_only: false,
        })
    }

    /// Open read-only, for the app to read while a CLI index holds the writer lock.
    ///
    /// WAL is what makes this give consistent reads throughout the write (PLAN.md C3).
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Ok(Self {
            conn,
            path,
            read_only: true,
        })
    }

    /// In-memory, for tests.
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        schema::migrate(&conn)?;
        Ok(Self {
            conn,
            path: PathBuf::from(":memory:"),
            read_only: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    // --- writes -------------------------------------------------------------------------

    /// Insert launches, ignoring any already present.
    ///
    /// Returns how many rows were actually new, which is what the idempotency test checks.
    pub fn insert_launches(&mut self, rows: &[LaunchRow]) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut written = 0;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO launches
                   (token, curve, deployer, pair_token, launch_config_id,
                    graduation_threshold, block, tx_hash, log_index)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT DO NOTHING",
            )?;
            for r in rows {
                written += stmt.execute(params![
                    addr_key(r.token),
                    addr_key(r.curve),
                    addr_key(r.deployer),
                    addr_key(r.pair_token),
                    r.launch_config_id as i64,
                    u256_to_blob(r.graduation_threshold).as_slice(),
                    r.block as i64,
                    hash_key(r.tx_hash),
                    r.log_index as i64,
                ])?;
            }
        }
        tx.commit()?;
        Ok(written)
    }

    pub fn insert_trades(&mut self, rows: &[TradeRow]) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut written = 0;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO trades
                   (tx_hash, log_index, curve, block, tx_index, side, actor, recipient,
                    amount_in, amount_out, fee, tax, snipe_tax)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT DO NOTHING",
            )?;
            for r in rows {
                written += stmt.execute(params![
                    hash_key(r.tx_hash),
                    r.log_index as i64,
                    addr_key(r.curve),
                    r.block as i64,
                    r.tx_index as i64,
                    r.side as i64,
                    addr_key(r.actor),
                    addr_key(r.recipient),
                    u256_to_blob(r.amount_in).as_slice(),
                    u256_to_blob(r.amount_out).as_slice(),
                    u256_to_blob(r.fee).as_slice(),
                    u256_to_blob(r.tax).as_slice(),
                    r.snipe_tax.map(|v| u256_to_blob(v).to_vec()),
                ])?;
            }
        }
        tx.commit()?;
        Ok(written)
    }

    /// Attach a snipe-tax amount to an already-stored buy in the same transaction.
    ///
    /// `SnipeTaxCharged` arrives in the same log scan but at a different log index, so it
    /// is matched to its buy after the fact.
    pub fn set_snipe_tax(&mut self, tx_hash: B256, curve: Address, amount: U256) -> Result<usize> {
        let n = self.conn.execute(
            "UPDATE trades SET snipe_tax = ?1
             WHERE tx_hash = ?2 AND curve = ?3 AND side = 0",
            params![
                u256_to_blob(amount).as_slice(),
                hash_key(tx_hash),
                addr_key(curve)
            ],
        )?;
        Ok(n)
    }

    pub fn insert_graduations(&mut self, rows: &[(Address, u64, B256, u64)]) -> Result<usize> {
        self.insert_token_events("graduations", rows)
    }

    pub fn insert_sweeps(&mut self, rows: &[(Address, u64, B256, u64)]) -> Result<usize> {
        self.insert_token_events("sweeps", rows)
    }

    fn insert_token_events(
        &mut self,
        table: &str,
        rows: &[(Address, u64, B256, u64)],
    ) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut written = 0;
        {
            let sql = format!(
                "INSERT INTO {table} (token, block, tx_hash, log_index)
                 VALUES (?1, ?2, ?3, ?4) ON CONFLICT DO NOTHING"
            );
            let mut stmt = tx.prepare(&sql)?;
            for (token, block, hash, log_index) in rows {
                written += stmt.execute(params![
                    addr_key(*token),
                    *block as i64,
                    hash_key(*hash),
                    *log_index as i64,
                ])?;
            }
        }
        tx.commit()?;
        Ok(written)
    }

    pub fn insert_block_anchors(&mut self, anchors: &[(u64, u64)]) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut written = 0;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO block_anchors (block, ts) VALUES (?1, ?2) ON CONFLICT DO NOTHING",
            )?;
            for (b, t) in anchors {
                written += stmt.execute(params![*b as i64, *t as i64])?;
            }
        }
        tx.commit()?;
        Ok(written)
    }

    pub fn upsert_launch_config(
        &mut self,
        id: u64,
        cfg: &quarrel_core::curve::LaunchConfig,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO launch_configs (id, supply, curve_fee_bps, phantom_quote, graduation_threshold)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
               supply = excluded.supply,
               curve_fee_bps = excluded.curve_fee_bps,
               phantom_quote = excluded.phantom_quote,
               graduation_threshold = excluded.graduation_threshold",
            params![
                id as i64,
                u256_to_blob(cfg.supply).as_slice(),
                cfg.curve_fee_bps as i64,
                u256_to_blob(cfg.phantom_quote).as_slice(),
                u256_to_blob(cfg.graduation_threshold).as_slice(),
            ],
        )?;
        Ok(())
    }

    pub fn get_launch_config(&self, id: u64) -> Result<Option<quarrel_core::curve::LaunchConfig>> {
        let row = self
            .conn
            .query_row(
                "SELECT supply, curve_fee_bps, phantom_quote, graduation_threshold
                 FROM launch_configs WHERE id = ?1",
                params![id as i64],
                |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Vec<u8>>(2)?,
                        r.get::<_, Vec<u8>>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((supply, fee, phantom, threshold)) = row else {
            return Ok(None);
        };
        Ok(Some(quarrel_core::curve::LaunchConfig {
            supply: blob(&supply, "launch_configs.supply")?,
            curve_fee_bps: fee as u32,
            phantom_quote: blob(&phantom, "launch_configs.phantom_quote")?,
            graduation_threshold: blob(&threshold, "launch_configs.graduation_threshold")?,
        }))
    }

    pub fn insert_enrichment(&mut self, rows: &[EnrichmentRow]) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut written = 0;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO enrichment
                   (token, decoded, selector, name, symbol, description, logo,
                    twitter_url, website_url, telegram_url,
                    twitter, website, telegram, exempt_wallets,
                    creator_fee_recipient, creator_tax_bps, declared_quote_in,
                    dev_buy_quote, dev_buy_tokens, dev_buy_bps)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)
                 ON CONFLICT DO NOTHING",
            )?;
            for r in rows {
                written += stmt.execute(params![
                    addr_key(r.token),
                    r.decoded as i64,
                    r.selector,
                    r.name,
                    r.symbol,
                    r.description,
                    r.logo,
                    r.twitter_url,
                    r.website_url,
                    r.telegram_url,
                    presence_to_i64(r.socials.twitter),
                    presence_to_i64(r.socials.website),
                    presence_to_i64(r.socials.telegram),
                    r.exempt_wallets.map(|v| v as i64),
                    r.creator_fee_recipient.map(addr_key),
                    r.creator_tax_bps.map(|v| v as i64),
                    r.declared_quote_in.map(|v| u256_to_blob(v).to_vec()),
                    r.dev_buy_quote.map(|v| u256_to_blob(v).to_vec()),
                    r.dev_buy_tokens.map(|v| u256_to_blob(v).to_vec()),
                    r.dev_buy_bps.map(|v| v as i64),
                ])?;
            }
        }
        tx.commit()?;
        Ok(written)
    }

    // --- resume state -------------------------------------------------------------------

    /// Record where a phase got to. Called after every chunk, so a failure at 70% resumes
    /// at 70% rather than restarting.
    ///
    /// `from_block` keeps the **earliest** start for a given target, because a resumed run
    /// starts at its resume point and would otherwise overwrite the real beginning of the
    /// window. That is not academic: after one interruption the Lab reported its window as
    /// `56867943..56867943`, zero hours long, because every phase had resumed at the end.
    /// A run at a genuinely different target replaces it, which is what a new window means.
    pub fn checkpoint(&mut self, phase: &str, state: PhaseState) -> Result<()> {
        self.conn.execute(
            "INSERT INTO index_state
               (phase, from_block, last_block, target_block, rows_written, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(phase) DO UPDATE SET
               from_block   = CASE
                                WHEN index_state.target_block = excluded.target_block
                                THEN min(index_state.from_block, excluded.from_block)
                                ELSE excluded.from_block
                              END,
               last_block   = excluded.last_block,
               target_block = excluded.target_block,
               rows_written = excluded.rows_written,
               updated_at   = excluded.updated_at",
            params![
                phase,
                state.from_block as i64,
                state.last_block as i64,
                state.target_block as i64,
                state.rows_written as i64,
                now_secs() as i64,
            ],
        )?;
        Ok(())
    }

    pub fn phase_state(&self, phase: &str) -> Result<Option<PhaseState>> {
        let row = self
            .conn
            .query_row(
                "SELECT from_block, last_block, target_block, rows_written
                 FROM index_state WHERE phase = ?1",
                params![phase],
                |r| {
                    Ok(PhaseState {
                        from_block: r.get::<_, i64>(0)? as u64,
                        last_block: r.get::<_, i64>(1)? as u64,
                        target_block: r.get::<_, i64>(2)? as u64,
                        rows_written: r.get::<_, i64>(3)? as u64,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    // --- reads --------------------------------------------------------------------------

    pub fn count(&self, table: &str) -> Result<u64> {
        // `table` is only ever a literal from this crate; never user input.
        let n: i64 = self
            .conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?;
        Ok(n as u64)
    }

    pub fn launch_count(&self) -> Result<u64> {
        self.count("launches")
    }

    pub fn trade_count(&self) -> Result<u64> {
        self.count("trades")
    }

    /// Every `(tx_hash, log_index)` in `trades`, for the resume equivalence test.
    pub fn trade_keys(&self) -> Result<Vec<(String, u64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT tx_hash, log_index FROM trades ORDER BY tx_hash, log_index")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn launch_keys(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT token FROM launches ORDER BY token")?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Trades on one curve, in chain order. This is what the outcome replay walks.
    /// The dev buy: the first buy on `curve` **within the launch transaction**.
    ///
    /// Restricting to the launch transaction is what makes this point-in-time. The first
    /// buy on the curve is not the same thing — when the deployer launches without buying,
    /// that buy belongs to a sniper, in a later block, and using it would feed a filter a
    /// fact from after the launch (spec §5.3, `docs/FINDINGS.md` §10).
    ///
    /// A targeted query rather than a scan of `trades_for_curve`, because the calldata
    /// phase runs this once per launch — ~25,000 times on a 24-hour window — and a busy
    /// curve has hundreds of trades whose money blobs would be decoded and thrown away.
    pub fn dev_buy(&self, curve: Address, launch_tx: B256) -> Result<Option<TradeRow>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT tx_hash, log_index, curve, block, tx_index, side, actor, recipient,
                    amount_in, amount_out, fee, tax, snipe_tax
             FROM trades
             WHERE curve = ?1 AND tx_hash = ?2 AND side = ?3
             ORDER BY block, log_index
             LIMIT 1",
        )?;
        let mut rows = stmt.query(params![
            addr_key(curve),
            hash_key(launch_tx),
            Side::Buy as i64
        ])?;
        match rows.next()? {
            None => Ok(None),
            Some(r) => Ok(Some(trade_from_row(r)?)),
        }
    }

    pub fn trades_for_curve(&self, curve: Address) -> Result<Vec<TradeRow>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT tx_hash, log_index, curve, block, tx_index, side, actor, recipient,
                    amount_in, amount_out, fee, tax, snipe_tax
             FROM trades WHERE curve = ?1
             ORDER BY block, log_index",
        )?;
        let rows = stmt
            .query_map(params![addr_key(curve)], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, Vec<u8>>(8)?,
                    r.get::<_, Vec<u8>>(9)?,
                    r.get::<_, Vec<u8>>(10)?,
                    r.get::<_, Vec<u8>>(11)?,
                    r.get::<_, Option<Vec<u8>>>(12)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        rows.into_iter()
            .map(|t| {
                Ok(TradeRow {
                    tx_hash: parse_hash(&t.0)?,
                    log_index: t.1 as u64,
                    curve: parse_addr(&t.2)?,
                    block: t.3 as u64,
                    tx_index: t.4 as u64,
                    side: Side::from_i64(t.5)
                        .ok_or_else(|| StoreError::Corrupt(format!("trades.side = {}", t.5)))?,
                    actor: parse_addr(&t.6)?,
                    recipient: parse_addr(&t.7)?,
                    amount_in: blob(&t.8, "trades.amount_in")?,
                    amount_out: blob(&t.9, "trades.amount_out")?,
                    fee: blob(&t.10, "trades.fee")?,
                    tax: blob(&t.11, "trades.tax")?,
                    snipe_tax: match t.12 {
                        Some(b) => Some(blob(&b, "trades.snipe_tax")?),
                        None => None,
                    },
                })
            })
            .collect()
    }

    pub fn enrichment_for(&self, token: Address) -> Result<Option<EnrichmentRow>> {
        let row = self
            .conn
            .query_row(
                "SELECT decoded, selector, name, symbol, description, logo,
                        twitter_url, website_url, telegram_url,
                        twitter, website, telegram, exempt_wallets,
                        creator_fee_recipient, creator_tax_bps, declared_quote_in,
                        dev_buy_quote, dev_buy_tokens, dev_buy_bps
                 FROM enrichment WHERE token = ?1",
                params![addr_key(token)],
                |r| {
                    Ok(EnrichmentRow {
                        token,
                        decoded: r.get::<_, i64>(0)? != 0,
                        selector: r.get(1)?,
                        name: r.get(2)?,
                        symbol: r.get(3)?,
                        description: r.get(4)?,
                        logo: r.get(5)?,
                        twitter_url: r.get(6)?,
                        website_url: r.get(7)?,
                        telegram_url: r.get(8)?,
                        socials: quarrel_core::features::Socials {
                            twitter: presence_from_i64(r.get(9)?),
                            website: presence_from_i64(r.get(10)?),
                            telegram: presence_from_i64(r.get(11)?),
                        },
                        exempt_wallets: r.get::<_, Option<i64>>(12)?.map(|v| v as u32),
                        creator_fee_recipient: r
                            .get::<_, Option<String>>(13)?
                            .and_then(|s| s.parse().ok()),
                        creator_tax_bps: r.get::<_, Option<i64>>(14)?.map(|v| v as u32),
                        declared_quote_in: r
                            .get::<_, Option<Vec<u8>>>(15)?
                            .and_then(|b| u256_from_blob(&b)),
                        dev_buy_quote: r
                            .get::<_, Option<Vec<u8>>>(16)?
                            .and_then(|b| u256_from_blob(&b)),
                        dev_buy_tokens: r
                            .get::<_, Option<Vec<u8>>>(17)?
                            .and_then(|b| u256_from_blob(&b)),
                        dev_buy_bps: r.get::<_, Option<i64>>(18)?.map(|v| v as u32),
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Interpolate a block's timestamp from the sampled anchors.
    ///
    /// Approximate by construction, and labelled as such wherever it is displayed
    /// (PLAN.md F2, C4). Never used for the entry point, which needs no timestamp.
    pub fn timestamp_at(&self, block: u64) -> Result<Option<u64>> {
        let below: Option<(i64, i64)> = self
            .conn
            .query_row(
                "SELECT block, ts FROM block_anchors WHERE block <= ?1 ORDER BY block DESC LIMIT 1",
                params![block as i64],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let above: Option<(i64, i64)> = self
            .conn
            .query_row(
                "SELECT block, ts FROM block_anchors WHERE block >= ?1 ORDER BY block ASC LIMIT 1",
                params![block as i64],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;

        Ok(match (below, above) {
            (Some((b0, t0)), Some((b1, t1))) => {
                if b0 == b1 {
                    Some(t0 as u64)
                } else {
                    // Linear between the two nearest anchors.
                    let span = (b1 - b0) as i128;
                    let into = (block as i64 - b0) as i128;
                    let dt = (t1 - t0) as i128;
                    Some((t0 as i128 + dt * into / span) as u64)
                }
            }
            // Outside the anchored range: extrapolating would invent data, so say nothing.
            _ => None,
        })
    }

    // --- queries the indexer needs ------------------------------------------------------
    //
    // These live here rather than in `quarrel-indexer` because spec §4.1 puts every query
    // behind the store's API: analytics belong in SQL, and a columnar backend must be
    // addable later without touching callers.

    /// Every launch in ascending block order.
    ///
    /// The order is not a convenience. The point-in-time feature builder depends on seeing
    /// launches oldest-first, because that is what makes it structurally unable to read
    /// the future.
    pub fn all_launches_lite(&self) -> Result<Vec<LaunchLite>> {
        let mut stmt = self.conn.prepare(
            "SELECT token, curve, deployer, block, tx_hash, pair_token, graduation_threshold
             FROM launches ORDER BY block, token",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, Vec<u8>>(6)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(t, c, d, b, h, p, g)| {
                Ok(LaunchLite {
                    token: parse_addr(&t)?,
                    curve: parse_addr(&c)?,
                    deployer: parse_addr(&d)?,
                    block: b as u64,
                    tx_hash: parse_hash(&h)?,
                    pair_token: parse_addr(&p)?,
                    graduation_threshold: blob(&g, "launches.graduation_threshold")?,
                })
            })
            .collect()
    }

    /// Launches with no enrichment row yet, so a resumed calldata phase does not refetch
    /// what it already has.
    pub fn launches_needing_calldata(&self) -> Result<Vec<(Address, B256, Address)>> {
        let mut stmt = self.conn.prepare(
            "SELECT l.token, l.tx_hash, l.curve
             FROM launches l LEFT JOIN enrichment e ON e.token = l.token
             WHERE e.token IS NULL
             ORDER BY l.block",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(t, h, c)| Ok((parse_addr(&t)?, parse_hash(&h)?, parse_addr(&c)?)))
            .collect()
    }

    /// Every token that graduated, and the block it did so.
    ///
    /// The block matters: a graduation is only visible to a launch that came after it.
    pub fn graduation_blocks(&self) -> Result<std::collections::HashMap<Address, u64>> {
        let mut stmt = self.conn.prepare("SELECT token, block FROM graduations")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(t, b)| Ok((parse_addr(&t)?, b)))
            .collect()
    }

    /// Distinct pair tokens with no economics row yet.
    pub fn pair_tokens_needing_economics(&self) -> Result<Vec<Address>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT l.pair_token FROM launches l
             LEFT JOIN pair_economics p ON p.pair_token = l.pair_token
             WHERE p.pair_token IS NULL",
        )?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.iter().map(|s| parse_addr(s)).collect()
    }

    pub fn upsert_pair_economics(
        &self,
        pair_token: Address,
        phantom_quote: U256,
        graduation_threshold: U256,
        decimals: u8,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO pair_economics (pair_token, phantom_quote, graduation_threshold, decimals)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(pair_token) DO UPDATE SET
               phantom_quote = excluded.phantom_quote,
               graduation_threshold = excluded.graduation_threshold,
               decimals = excluded.decimals",
            params![
                addr_key(pair_token),
                u256_to_blob(phantom_quote).as_slice(),
                u256_to_blob(graduation_threshold).as_slice(),
                decimals as i64,
            ],
        )?;
        Ok(())
    }

    /// Phantom quote per pair token, for replaying curves.
    pub fn pair_phantoms(&self) -> Result<std::collections::HashMap<Address, U256>> {
        let mut stmt = self
            .conn
            .prepare("SELECT pair_token, phantom_quote FROM pair_economics")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(a, b)| Ok((parse_addr(&a)?, blob(&b, "pair_economics.phantom_quote")?)))
            .collect()
    }

    pub fn upsert_pit_features(&self, p: &PitFeaturesRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO pit_features
               (token, deployer_launches, deployer_graduations, deployer_grad_rate_bps,
                fingerprint, fingerprint_twins_30m, deployer_history_depth_blocks)
             VALUES (?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(token) DO UPDATE SET
               deployer_launches = excluded.deployer_launches,
               deployer_graduations = excluded.deployer_graduations,
               deployer_grad_rate_bps = excluded.deployer_grad_rate_bps,
               fingerprint = excluded.fingerprint,
               fingerprint_twins_30m = excluded.fingerprint_twins_30m,
               deployer_history_depth_blocks = excluded.deployer_history_depth_blocks",
            params![
                addr_key(p.token),
                p.deployer_launches as i64,
                p.deployer_graduations as i64,
                p.deployer_grad_rate_bps.map(|v| v as i64),
                p.fingerprint,
                p.fingerprint_twins_30m as i64,
                p.deployer_history_depth_blocks as i64,
            ],
        )?;
        Ok(())
    }

    pub fn upsert_outcome(&self, o: &OutcomeRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO outcomes
               (token, entry_rule, entry_block, entry_price, entry_tokens,
                ath_price, ath_block, max_multiple_bps, time_to_ath_s,
                mult_after_5m_bps, mult_after_30m_bps, migrated, died,
                distinct_buyers_1m, every_early_buy_taxed,
                post_entry_trades, last_trade_block, observed_blocks)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)
             ON CONFLICT(token) DO UPDATE SET
               entry_rule = excluded.entry_rule,
               entry_block = excluded.entry_block,
               entry_price = excluded.entry_price,
               entry_tokens = excluded.entry_tokens,
               ath_price = excluded.ath_price,
               ath_block = excluded.ath_block,
               max_multiple_bps = excluded.max_multiple_bps,
               time_to_ath_s = excluded.time_to_ath_s,
               mult_after_5m_bps = excluded.mult_after_5m_bps,
               mult_after_30m_bps = excluded.mult_after_30m_bps,
               migrated = excluded.migrated,
               died = excluded.died,
               distinct_buyers_1m = excluded.distinct_buyers_1m,
               every_early_buy_taxed = excluded.every_early_buy_taxed,
               post_entry_trades = excluded.post_entry_trades,
               last_trade_block = excluded.last_trade_block,
               observed_blocks = excluded.observed_blocks",
            params![
                addr_key(o.token),
                o.entry_rule as i64,
                o.entry_block.map(|v| v as i64),
                o.entry_price.map(|v| u256_to_blob(v).to_vec()),
                o.entry_tokens.map(|v| u256_to_blob(v).to_vec()),
                o.ath_price.map(|v| u256_to_blob(v).to_vec()),
                o.ath_block.map(|v| v as i64),
                o.max_multiple_bps.map(|v| v as i64),
                o.time_to_ath_s.map(|v| v as i64),
                o.mult_after_5m_bps.map(|v| v as i64),
                o.mult_after_30m_bps.map(|v| v as i64),
                o.migrated as i64,
                o.died as i64,
                o.distinct_buyers_1m.map(|v| v as i64),
                o.every_early_buy_taxed.map(|v| v as i64),
                o.post_entry_trades as i64,
                o.last_trade_block.map(|v| v as i64),
                o.observed_blocks as i64,
            ],
        )?;
        Ok(())
    }
}

/// A launch reduced to what feature and outcome computation need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchLite {
    pub token: Address,
    pub curve: Address,
    pub deployer: Address,
    pub block: u64,
    pub tx_hash: B256,
    pub pair_token: Address,
    /// Per launch, from the `TokenLaunched` event. The phantom reserve derives from it,
    /// so a curve cannot be replayed without it.
    pub graduation_threshold: U256,
}

/// One `pit_features` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PitFeaturesRow {
    pub token: Address,
    pub deployer_launches: u32,
    pub deployer_graduations: u32,
    pub deployer_grad_rate_bps: Option<u32>,
    pub fingerprint: String,
    pub fingerprint_twins_30m: u32,
    pub deployer_history_depth_blocks: u64,
}

/// One `outcomes` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutcomeRow {
    pub token: Address,
    pub entry_rule: crate::types::EntryRule,
    pub entry_block: Option<u64>,
    pub entry_price: Option<U256>,
    pub entry_tokens: Option<U256>,
    pub ath_price: Option<U256>,
    pub ath_block: Option<u64>,
    pub max_multiple_bps: Option<u64>,
    pub time_to_ath_s: Option<u64>,
    pub mult_after_5m_bps: Option<u64>,
    pub mult_after_30m_bps: Option<u64>,
    pub migrated: bool,
    pub died: bool,
    pub distinct_buyers_1m: Option<u32>,
    pub every_early_buy_taxed: Option<bool>,
    pub post_entry_trades: u64,
    pub last_trade_block: Option<u64>,
    pub observed_blocks: u64,
}

/// One `trades` row from a statement selecting the columns in schema order.
fn trade_from_row(r: &rusqlite::Row<'_>) -> Result<TradeRow> {
    let side_raw: i64 = r.get(5)?;
    Ok(TradeRow {
        tx_hash: parse_hash(&r.get::<_, String>(0)?)?,
        log_index: r.get::<_, i64>(1)? as u64,
        curve: parse_addr(&r.get::<_, String>(2)?)?,
        block: r.get::<_, i64>(3)? as u64,
        tx_index: r.get::<_, i64>(4)? as u64,
        side: Side::from_i64(side_raw)
            .ok_or_else(|| StoreError::Corrupt(format!("trades.side = {side_raw}")))?,
        actor: parse_addr(&r.get::<_, String>(6)?)?,
        recipient: parse_addr(&r.get::<_, String>(7)?)?,
        amount_in: blob(&r.get::<_, Vec<u8>>(8)?, "trades.amount_in")?,
        amount_out: blob(&r.get::<_, Vec<u8>>(9)?, "trades.amount_out")?,
        fee: blob(&r.get::<_, Vec<u8>>(10)?, "trades.fee")?,
        tax: blob(&r.get::<_, Vec<u8>>(11)?, "trades.tax")?,
        snipe_tax: match r.get::<_, Option<Vec<u8>>>(12)? {
            Some(b) => Some(blob(&b, "trades.snipe_tax")?),
            None => None,
        },
    })
}

fn blob(b: &[u8], what: &str) -> Result<U256> {
    u256_from_blob(b).ok_or_else(|| StoreError::Corrupt(format!("{what} is not a 32-byte blob")))
}

fn parse_addr(s: &str) -> Result<Address> {
    s.parse()
        .map_err(|_| StoreError::Corrupt(format!("not an address: {s}")))
}

fn parse_hash(s: &str) -> Result<B256> {
    s.parse()
        .map_err(|_| StoreError::Corrupt(format!("not a hash: {s}")))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(n: u8) -> Address {
        Address::repeat_byte(n)
    }

    fn launch(n: u8, block: u64) -> LaunchRow {
        LaunchRow {
            token: addr(n),
            curve: addr(n.wrapping_add(100)),
            deployer: addr(n.wrapping_add(200)),
            pair_token: Address::ZERO,
            launch_config_id: 0,
            graduation_threshold: U256::from(4_200_000_000_000_000_000u64),
            block,
            tx_hash: B256::repeat_byte(n),
            log_index: 0,
        }
    }

    fn trade(n: u8, curve: Address, block: u64, log_index: u64, side: Side) -> TradeRow {
        TradeRow {
            tx_hash: B256::repeat_byte(n),
            log_index,
            curve,
            block,
            tx_index: 0,
            side,
            actor: addr(7),
            recipient: addr(7),
            amount_in: U256::from(1_000u64),
            amount_out: U256::from(2_000u64),
            fee: U256::from(10u64),
            tax: U256::ZERO,
            snipe_tax: None,
        }
    }

    #[test]
    fn launches_round_trip() {
        let mut h = History::in_memory().unwrap();
        assert_eq!(
            h.insert_launches(&[launch(1, 10), launch(2, 20)]).unwrap(),
            2
        );
        assert_eq!(h.launch_count().unwrap(), 2);
    }

    /// Spec §6.2: re-indexing an already-covered range must not duplicate rows.
    #[test]
    fn reindexing_the_same_range_writes_zero_new_rows() {
        let mut h = History::in_memory().unwrap();
        let rows = vec![launch(1, 10), launch(2, 20), launch(3, 30)];
        assert_eq!(
            h.insert_launches(&rows).unwrap(),
            3,
            "first pass writes all"
        );
        assert_eq!(
            h.insert_launches(&rows).unwrap(),
            0,
            "second pass writes none"
        );
        assert_eq!(h.launch_count().unwrap(), 3);

        let curve = addr(101);
        let trades = vec![
            trade(1, curve, 11, 0, Side::Buy),
            trade(1, curve, 11, 1, Side::Sell),
        ];
        assert_eq!(h.insert_trades(&trades).unwrap(), 2);
        assert_eq!(
            h.insert_trades(&trades).unwrap(),
            0,
            "idempotent by natural key"
        );
        assert_eq!(h.trade_count().unwrap(), 2);
    }

    #[test]
    fn a_partial_overlap_writes_only_what_is_new() {
        // This is what a resume actually does: re-cover the last chunk, add the rest.
        let mut h = History::in_memory().unwrap();
        h.insert_launches(&[launch(1, 10), launch(2, 20)]).unwrap();
        let written = h
            .insert_launches(&[launch(2, 20), launch(3, 30), launch(4, 40)])
            .unwrap();
        assert_eq!(written, 2, "only the two genuinely new rows");
        assert_eq!(h.launch_count().unwrap(), 4);
    }

    fn buy(tx: B256, curve: Address, block: u64, log_index: u64, out: u64) -> TradeRow {
        TradeRow {
            tx_hash: tx,
            log_index,
            curve,
            block,
            tx_index: 0,
            side: Side::Buy,
            actor: addr(9),
            recipient: addr(9),
            amount_in: U256::from(1_000u64),
            amount_out: U256::from(out),
            fee: U256::from(10u64),
            tax: U256::ZERO,
            snipe_tax: None,
        }
    }

    /// The point-in-time rule this query exists to enforce.
    #[test]
    fn the_dev_buy_is_only_a_buy_inside_the_launch_transaction() {
        let mut h = History::in_memory().unwrap();
        let curve = addr(101);
        let launch_tx = B256::repeat_byte(1);

        // A sniper buys three blocks after the launch. There was no dev buy at all.
        h.insert_trades(&[buy(B256::repeat_byte(2), curve, 103, 0, 5_000)])
            .unwrap();

        assert_eq!(
            h.dev_buy(curve, launch_tx).unwrap(),
            None,
            "a sniper's buy in a later block is not the dev buy; treating it as one feeds              a filter a fact from after the launch"
        );
        assert_eq!(
            h.trades_for_curve(curve).unwrap().len(),
            1,
            "the trade is still there -- only the dev-buy question answers None"
        );
    }

    #[test]
    fn the_dev_buy_is_found_when_it_is_in_the_launch_transaction() {
        let mut h = History::in_memory().unwrap();
        let curve = addr(101);
        let launch_tx = B256::repeat_byte(1);
        h.insert_trades(&[
            buy(launch_tx, curve, 100, 4, 10_000),
            buy(B256::repeat_byte(2), curve, 103, 0, 5_000),
        ])
        .unwrap();

        let dev = h.dev_buy(curve, launch_tx).unwrap().expect("the dev buy");
        assert_eq!(dev.amount_out, U256::from(10_000u64));
        assert_eq!(dev.block, 100);
    }

    #[test]
    fn a_sell_in_the_launch_transaction_is_not_the_dev_buy() {
        let mut h = History::in_memory().unwrap();
        let curve = addr(101);
        let launch_tx = B256::repeat_byte(1);
        let mut sell = buy(launch_tx, curve, 100, 2, 1);
        sell.side = Side::Sell;
        h.insert_trades(&[sell]).unwrap();
        assert_eq!(h.dev_buy(curve, launch_tx).unwrap(), None);
    }

    #[test]
    fn trades_come_back_in_chain_order() {
        let mut h = History::in_memory().unwrap();
        let curve = addr(101);
        h.insert_trades(&[
            trade(3, curve, 30, 2, Side::Sell),
            trade(1, curve, 10, 5, Side::Buy),
            trade(2, curve, 10, 1, Side::Buy),
        ])
        .unwrap();
        let got = h.trades_for_curve(curve).unwrap();
        let order: Vec<(u64, u64)> = got.iter().map(|t| (t.block, t.log_index)).collect();
        assert_eq!(
            order,
            vec![(10, 1), (10, 5), (30, 2)],
            "the replay depends on chain order"
        );
    }

    #[test]
    fn a_trade_round_trips_every_field_including_the_absent_snipe_tax() {
        let mut h = History::in_memory().unwrap();
        let curve = addr(101);
        let mut taxed = trade(1, curve, 10, 0, Side::Buy);
        taxed.snipe_tax = Some(U256::from(999u64));
        let untaxed = trade(2, curve, 11, 0, Side::Buy);
        h.insert_trades(&[taxed.clone(), untaxed.clone()]).unwrap();

        let got = h.trades_for_curve(curve).unwrap();
        assert_eq!(got[0], taxed);
        assert_eq!(got[1], untaxed);
        assert_eq!(
            got[1].snipe_tax, None,
            "the ABSENCE of a snipe tax is the signal that the window has closed"
        );
    }

    #[test]
    fn a_snipe_tax_can_be_attached_after_the_buy_is_stored() {
        // The SnipeTaxCharged log arrives in the same scan but at a different log index.
        let mut h = History::in_memory().unwrap();
        let curve = addr(101);
        h.insert_trades(&[trade(1, curve, 10, 0, Side::Buy)])
            .unwrap();
        let n = h
            .set_snipe_tax(B256::repeat_byte(1), curve, U256::from(500u64))
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            h.trades_for_curve(curve).unwrap()[0].snipe_tax,
            Some(U256::from(500u64))
        );
    }

    #[test]
    fn checkpoints_survive_and_overwrite() {
        let mut h = History::in_memory().unwrap();
        assert_eq!(h.phase_state("trades").unwrap(), None, "nothing yet");

        let s1 = PhaseState {
            from_block: 100,
            last_block: 500,
            target_block: 1_000,
            rows_written: 42,
        };
        h.checkpoint("trades", s1).unwrap();
        assert_eq!(h.phase_state("trades").unwrap(), Some(s1));

        let s2 = PhaseState {
            last_block: 900,
            rows_written: 90,
            ..s1
        };
        h.checkpoint("trades", s2).unwrap();
        assert_eq!(
            h.phase_state("trades").unwrap(),
            Some(s2),
            "a later checkpoint replaces the earlier one"
        );
    }

    #[test]
    fn enrichment_round_trips_including_the_unknown_states() {
        use quarrel_core::features::{Presence, Socials};
        let mut h = History::in_memory().unwrap();
        h.insert_launches(&[launch(1, 10)]).unwrap();

        let row = EnrichmentRow {
            token: addr(1),
            decoded: false,
            selector: Some("0xdeadbeef".into()),
            name: None,
            symbol: None,
            description: None,
            logo: None,
            twitter_url: None,
            website_url: None,
            telegram_url: None,
            socials: Socials::UNKNOWN,
            exempt_wallets: None,
            creator_fee_recipient: None,
            creator_tax_bps: None,
            declared_quote_in: None,
            dev_buy_quote: Some(U256::from(5u64)),
            dev_buy_tokens: Some(U256::from(7u64)),
            dev_buy_bps: Some(300),
        };
        h.insert_enrichment(std::slice::from_ref(&row)).unwrap();

        let got = h.enrichment_for(addr(1)).unwrap().unwrap();
        assert_eq!(got, row);
        assert_eq!(got.socials.twitter, Presence::Unknown);
        assert_eq!(
            got.exempt_wallets, None,
            "an unreadable bundle size must not come back as zero"
        );
    }

    #[test]
    fn a_launch_config_round_trips_exactly() {
        let mut h = History::in_memory().unwrap();
        let cfg = quarrel_core::curve::LaunchConfig::live_id_0();
        h.upsert_launch_config(0, &cfg).unwrap();
        assert_eq!(h.get_launch_config(0).unwrap(), Some(cfg));
        assert_eq!(h.get_launch_config(9).unwrap(), None);
    }

    #[test]
    fn timestamps_interpolate_between_anchors() {
        let mut h = History::in_memory().unwrap();
        // 1000 blocks apart, 100 seconds apart: ~100 ms/block, as measured.
        h.insert_block_anchors(&[(1_000, 10_000), (2_000, 10_100)])
            .unwrap();

        assert_eq!(h.timestamp_at(1_000).unwrap(), Some(10_000), "on an anchor");
        assert_eq!(h.timestamp_at(2_000).unwrap(), Some(10_100), "on an anchor");
        assert_eq!(h.timestamp_at(1_500).unwrap(), Some(10_050), "halfway");
        assert_eq!(h.timestamp_at(1_250).unwrap(), Some(10_025), "a quarter in");
    }

    #[test]
    fn a_block_outside_the_anchored_range_has_no_timestamp_rather_than_a_guess() {
        // Extrapolating past the anchors would invent data and label it as measured.
        let mut h = History::in_memory().unwrap();
        h.insert_block_anchors(&[(1_000, 10_000), (2_000, 10_100)])
            .unwrap();
        assert_eq!(h.timestamp_at(999).unwrap(), None);
        assert_eq!(h.timestamp_at(2_001).unwrap(), None);
    }

    #[test]
    fn anchors_are_idempotent_too() {
        let mut h = History::in_memory().unwrap();
        assert_eq!(h.insert_block_anchors(&[(1, 5), (2, 6)]).unwrap(), 2);
        assert_eq!(h.insert_block_anchors(&[(1, 5), (2, 6)]).unwrap(), 0);
    }
}
