//! `live.db` — what the sniper did, and why.
//!
//! `history.db` is the world; this is us. It answers three questions a user will actually
//! ask, and the third is the one that is usually forgotten:
//!
//! 1. **What am I holding?** Open positions survive a restart, so a session that crashed
//!    mid-trade can be reconciled against the chain rather than guessed at.
//! 2. **What did I do?** Every fill, with the rule that caused it and what it actually
//!    paid — not what it intended to pay.
//! 3. **Why did nothing happen?** Every launch that was refused, with the rule and the
//!    values. "Nothing fired all afternoon" is a question the product has to be able to
//!    answer, and it cannot answer it from data it did not keep (spec §3.4).
//!
//! Its own database and its own lock, so the engine writing here never blocks an index
//! writing `history.db` (PLAN.md C3).
//!
//! # Money
//!
//! `BLOB(32)` big-endian `U256` for raw amounts, `INTEGER` basis points for anything
//! derived, and no `REAL` column anywhere — the same rule as `history.db` and for the same
//! reason: a rounding error here is money (spec §12).

use std::path::{Path, PathBuf};

use alloy_primitives::{Address, B256, U256};
use rusqlite::{Connection, OptionalExtension, params};

use crate::types::{Side, addr_key, hash_key, u256_from_blob, u256_to_blob};
use crate::{Result, StoreError, schema};

pub const SCHEMA_VERSION: i64 = 1;

/// One run of the sniper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSession {
    /// `"test"` or `"live"`. A string rather than the enum, because `quarrel-store` must
    /// not depend on `quarrel-live` — that is the direction the trust boundary forbids.
    pub mode: String,
    pub wallet: Address,
    pub started_at: u64,
    pub session_budget_wei: U256,
    pub size_per_buy_wei: U256,
    pub position_cap_wei: U256,
    pub max_open_positions: u32,
    /// The `StrategyConfig` as JSON, so a journal read months later says what the rules
    /// were rather than what they have since become.
    pub strategy_json: String,
}

/// A position being opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPosition {
    pub session_id: i64,
    pub token: Address,
    pub curve: Address,
    pub symbol: String,
    pub pair: String,
    pub opened_at: u64,
    pub launch_block: u64,
}

/// One buy or sell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fill {
    pub position_id: i64,
    pub at: u64,
    pub side: Side,
    /// Quote asset in (a buy) or out (a sell), in wei.
    pub quote_wei: U256,
    pub tokens: U256,
    /// `None` in TEST, where nothing was sent. That is the field that tells a simulated
    /// fill from a real one, and it is why TEST rows are not quietly indistinguishable.
    pub tx_hash: Option<B256>,
    /// `"curve"` or `"pool"`.
    pub venue: String,
    /// `"entry"` for a buy, or the exit rule that fired.
    pub rule: String,
    pub detail: String,
    /// What the opening tax actually took, read from the buy's own logs (PLAN.md F7).
    pub snipe_tax_wei: Option<U256>,
    /// What the tax was when the decision to buy was made.
    pub tax_bps_at_decision: Option<u32>,
    /// Gas paid, in wei. `None` in TEST.
    pub gas_wei: Option<U256>,
}

/// A launch that did not become a position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub session_id: i64,
    pub at: u64,
    pub token: Address,
    pub symbol: String,
    pub block: u64,
    pub rule: String,
    pub detail: String,
}

/// A position as it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Position {
    pub id: i64,
    pub session_id: i64,
    pub token: Address,
    pub curve: Address,
    pub symbol: String,
    pub pair: String,
    pub opened_at: u64,
    pub closed_at: Option<u64>,
    pub cost_wei: U256,
    pub proceeds_wei: U256,
    pub tokens_bought: U256,
    pub tokens_held: U256,
    pub peak_bps: u64,
    pub last_mark_bps: Option<u64>,
    pub close_reason: Option<String>,
}

impl Position {
    /// Fraction of the original position still held, in basis points.
    ///
    /// This is what the partial-exit rules count against, so it has to be of the
    /// **original** size and not of what is left.
    pub fn remaining_bps(&self) -> u32 {
        if self.tokens_bought.is_zero() {
            return 0;
        }
        let bps = self
            .tokens_held
            .saturating_mul(U256::from(quarrel_core::BPS))
            / self.tokens_bought;
        bps.try_into().unwrap_or(quarrel_core::BPS)
    }

    pub fn is_open(&self) -> bool {
        self.closed_at.is_none()
    }
}

/// What a session added up to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Totals {
    pub positions: u64,
    pub open: u64,
    pub spent_wei: U256,
    pub returned_wei: U256,
    pub refusals: u64,
}

/// The live journal.
#[derive(Debug)]
pub struct Journal {
    conn: Connection,
    path: PathBuf,
}

impl Journal {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&path)?;
        schema::apply_pragmas(&conn)?;
        migrate(&conn)?;
        Ok(Self { conn, path })
    }

    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&conn)?;
        Ok(Self {
            conn,
            path: PathBuf::from(":memory:"),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    // --- sessions ---------------------------------------------------------------------

    pub fn begin_session(&mut self, s: &NewSession) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO sessions
                (mode, wallet, started_at, session_budget_wei, size_per_buy_wei,
                 position_cap_wei, max_open_positions, strategy_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                s.mode,
                addr_key(s.wallet),
                s.started_at as i64,
                u256_to_blob(s.session_budget_wei).as_slice(),
                u256_to_blob(s.size_per_buy_wei).as_slice(),
                u256_to_blob(s.position_cap_wei).as_slice(),
                s.max_open_positions as i64,
                s.strategy_json,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn end_session(&mut self, id: i64, at: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET ended_at = ?2 WHERE id = ?1 AND ended_at IS NULL",
            params![id, at as i64],
        )?;
        Ok(())
    }

    /// Sessions that were never closed, newest first.
    ///
    /// A session with open positions and no `ended_at` is one that died holding something.
    /// Surfacing it is how a user finds out they still own a token the program forgot.
    pub fn unfinished_sessions(&self) -> Result<Vec<(i64, String, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.mode, s.started_at
             FROM sessions s
             WHERE s.ended_at IS NULL
               AND EXISTS (SELECT 1 FROM positions p
                           WHERE p.session_id = s.id AND p.closed_at IS NULL)
             ORDER BY s.started_at DESC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? as u64))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // --- positions --------------------------------------------------------------------

    /// Open a position, or return the one already open for this token in this session.
    ///
    /// Adding to a position is a second fill against the same row, not a second row: the
    /// guards cap cost per **token**, and two rows for one token would let that cap be
    /// passed twice.
    pub fn open_position(&mut self, p: &NewPosition) -> Result<i64> {
        if let Some(id) = self.position_id(p.session_id, p.token)? {
            return Ok(id);
        }
        self.conn.execute(
            "INSERT INTO positions
                (session_id, token, curve, symbol, pair, opened_at, launch_block,
                 cost_wei, proceeds_wei, tokens_bought, tokens_held, peak_bps)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9, ?9, ?10)",
            params![
                p.session_id,
                addr_key(p.token),
                addr_key(p.curve),
                p.symbol,
                p.pair,
                p.opened_at as i64,
                p.launch_block as i64,
                u256_to_blob(U256::ZERO).as_slice(),
                u256_to_blob(U256::ZERO).as_slice(),
                quarrel_core::BPS as i64,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn position_id(&self, session_id: i64, token: Address) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM positions WHERE session_id = ?1 AND token = ?2",
                params![session_id, addr_key(token)],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Record a fill and move the position's money with it, in one transaction.
    ///
    /// One transaction because the two must never disagree: a fill written without its
    /// effect on the position would make the journal claim a sale that left the tokens
    /// behind.
    pub fn record_fill(&mut self, f: &Fill) -> Result<i64> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO fills
                (position_id, at, side, quote_wei, tokens, tx_hash, venue, rule, detail,
                 snipe_tax_wei, tax_bps_at_decision, gas_wei)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                f.position_id,
                f.at as i64,
                f.side as i64,
                u256_to_blob(f.quote_wei).as_slice(),
                u256_to_blob(f.tokens).as_slice(),
                f.tx_hash.map(hash_key),
                f.venue,
                f.rule,
                f.detail,
                f.snipe_tax_wei.map(|v| u256_to_blob(v).to_vec()),
                f.tax_bps_at_decision.map(|v| v as i64),
                f.gas_wei.map(|v| u256_to_blob(v).to_vec()),
            ],
        )?;
        let id = tx.last_insert_rowid();

        let (cost, proceeds, bought, held): (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) = tx.query_row(
            "SELECT cost_wei, proceeds_wei, tokens_bought, tokens_held
                 FROM positions WHERE id = ?1",
            params![f.position_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        let cost = blob(&cost)?;
        let proceeds = blob(&proceeds)?;
        let bought = blob(&bought)?;
        let held = blob(&held)?;

        let (cost, proceeds, bought, held) = match f.side {
            Side::Buy => (
                cost.saturating_add(f.quote_wei),
                proceeds,
                bought.saturating_add(f.tokens),
                held.saturating_add(f.tokens),
            ),
            // Saturating: a sale of more than is held would be a bug upstream, and going
            // to zero is the safe way to be wrong about it.
            Side::Sell => (
                cost,
                proceeds.saturating_add(f.quote_wei),
                bought,
                held.saturating_sub(f.tokens),
            ),
        };

        tx.execute(
            "UPDATE positions
                SET cost_wei = ?2, proceeds_wei = ?3, tokens_bought = ?4, tokens_held = ?5
              WHERE id = ?1",
            params![
                f.position_id,
                u256_to_blob(cost).as_slice(),
                u256_to_blob(proceeds).as_slice(),
                u256_to_blob(bought).as_slice(),
                u256_to_blob(held).as_slice(),
            ],
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// Record a mark, carrying the peak forward.
    ///
    /// The peak is stored rather than recomputed because it is what the trailing stop
    /// reads, and a restart that forgot the peak would silently widen every trailing stop
    /// to the current price.
    pub fn mark(&mut self, position_id: i64, mult_bps: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE positions
                SET last_mark_bps = ?2, peak_bps = max(peak_bps, ?2)
              WHERE id = ?1",
            params![position_id, mult_bps as i64],
        )?;
        Ok(())
    }

    pub fn close_position(&mut self, id: i64, at: u64, reason: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE positions SET closed_at = ?2, close_reason = ?3 WHERE id = ?1",
            params![id, at as i64, reason],
        )?;
        Ok(())
    }

    pub fn positions(&self, session_id: i64, open_only: bool) -> Result<Vec<Position>> {
        let sql = format!(
            "SELECT id, session_id, token, curve, symbol, pair, opened_at, closed_at,
                    cost_wei, proceeds_wei, tokens_bought, tokens_held, peak_bps,
                    last_mark_bps, close_reason
             FROM positions
             WHERE session_id = ?1 {}
             ORDER BY opened_at DESC",
            if open_only {
                "AND closed_at IS NULL"
            } else {
                ""
            }
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![session_id], row_to_position)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every open position across every session, for the reconciliation a restart needs.
    pub fn all_open_positions(&self) -> Result<Vec<Position>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, token, curve, symbol, pair, opened_at, closed_at,
                    cost_wei, proceeds_wei, tokens_bought, tokens_held, peak_bps,
                    last_mark_bps, close_reason
             FROM positions WHERE closed_at IS NULL ORDER BY opened_at DESC",
        )?;
        let rows = stmt
            .query_map([], row_to_position)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn fills(&self, position_id: i64) -> Result<Vec<Fill>> {
        let mut stmt = self.conn.prepare(
            "SELECT position_id, at, side, quote_wei, tokens, tx_hash, venue, rule, detail,
                    snipe_tax_wei, tax_bps_at_decision, gas_wei
             FROM fills WHERE position_id = ?1 ORDER BY id",
        )?;
        let rows = stmt
            .query_map(params![position_id], |r| {
                Ok(Fill {
                    position_id: r.get(0)?,
                    at: r.get::<_, i64>(1)? as u64,
                    side: Side::from_i64(r.get(2)?).unwrap_or(Side::Buy),
                    quote_wei: blob_row(r.get::<_, Vec<u8>>(3)?),
                    tokens: blob_row(r.get::<_, Vec<u8>>(4)?),
                    tx_hash: r.get::<_, Option<String>>(5)?.and_then(|s| s.parse().ok()),
                    venue: r.get(6)?,
                    rule: r.get(7)?,
                    detail: r.get(8)?,
                    snipe_tax_wei: r.get::<_, Option<Vec<u8>>>(9)?.map(blob_row),
                    tax_bps_at_decision: r.get::<_, Option<i64>>(10)?.map(|v| v as u32),
                    gas_wei: r.get::<_, Option<Vec<u8>>>(11)?.map(blob_row),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // --- refusals ---------------------------------------------------------------------

    pub fn record_refusal(&mut self, r: &Refusal) -> Result<()> {
        self.conn.execute(
            "INSERT INTO refusals (session_id, at, token, symbol, block, rule, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                r.session_id,
                r.at as i64,
                addr_key(r.token),
                r.symbol,
                r.block as i64,
                r.rule,
                r.detail,
            ],
        )?;
        Ok(())
    }

    pub fn recent_refusals(&self, session_id: i64, limit: u32) -> Result<Vec<Refusal>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, at, token, symbol, block, rule, detail
             FROM refusals WHERE session_id = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![session_id, limit as i64], |r| {
                Ok(Refusal {
                    session_id: r.get(0)?,
                    at: r.get::<_, i64>(1)? as u64,
                    token: r.get::<_, String>(2)?.parse().unwrap_or(Address::ZERO),
                    symbol: r.get(3)?,
                    block: r.get::<_, i64>(4)? as u64,
                    rule: r.get(5)?,
                    detail: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Refusals grouped by rule, commonest first.
    ///
    /// This is the answer to "why did nothing fire": one line per rule, with a count.
    pub fn refusals_by_rule(&self, session_id: i64) -> Result<Vec<(String, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT rule, count(*) FROM refusals WHERE session_id = ?1
             GROUP BY rule ORDER BY count(*) DESC",
        )?;
        let rows = stmt
            .query_map(params![session_id], |r| {
                Ok((r.get(0)?, r.get::<_, i64>(1)? as u64))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn totals(&self, session_id: i64) -> Result<Totals> {
        let (positions, open): (i64, i64) = self.conn.query_row(
            "SELECT count(*), coalesce(sum(CASE WHEN closed_at IS NULL THEN 1 ELSE 0 END), 0)
             FROM positions WHERE session_id = ?1",
            params![session_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        // Summed in Rust, not SQL: these are 32-byte blobs precisely so that no rounding
        // can happen, and SQLite cannot add them.
        let mut spent = U256::ZERO;
        let mut returned = U256::ZERO;
        let mut stmt = self
            .conn
            .prepare("SELECT cost_wei, proceeds_wei FROM positions WHERE session_id = ?1")?;
        let mut rows = stmt.query(params![session_id])?;
        while let Some(r) = rows.next()? {
            spent = spent.saturating_add(blob_row(r.get::<_, Vec<u8>>(0)?));
            returned = returned.saturating_add(blob_row(r.get::<_, Vec<u8>>(1)?));
        }
        let refusals: i64 = self.conn.query_row(
            "SELECT count(*) FROM refusals WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?;
        Ok(Totals {
            positions: positions.max(0) as u64,
            open: open.max(0) as u64,
            spent_wei: spent,
            returned_wei: returned,
            refusals: refusals.max(0) as u64,
        })
    }
}

fn row_to_position(r: &rusqlite::Row<'_>) -> rusqlite::Result<Position> {
    Ok(Position {
        id: r.get(0)?,
        session_id: r.get(1)?,
        token: r.get::<_, String>(2)?.parse().unwrap_or(Address::ZERO),
        curve: r.get::<_, String>(3)?.parse().unwrap_or(Address::ZERO),
        symbol: r.get(4)?,
        pair: r.get(5)?,
        opened_at: r.get::<_, i64>(6)? as u64,
        closed_at: r.get::<_, Option<i64>>(7)?.map(|v| v as u64),
        cost_wei: blob_row(r.get::<_, Vec<u8>>(8)?),
        proceeds_wei: blob_row(r.get::<_, Vec<u8>>(9)?),
        tokens_bought: blob_row(r.get::<_, Vec<u8>>(10)?),
        tokens_held: blob_row(r.get::<_, Vec<u8>>(11)?),
        peak_bps: r.get::<_, i64>(12)?.max(0) as u64,
        last_mark_bps: r.get::<_, Option<i64>>(13)?.map(|v| v.max(0) as u64),
        close_reason: r.get(14)?,
    })
}

fn blob(b: &[u8]) -> Result<U256> {
    u256_from_blob(b).ok_or_else(|| StoreError::Corrupt(format!("{} byte money blob", b.len())))
}

/// Inside a row mapper, where the error type is rusqlite's. A corrupt blob reads as zero
/// rather than taking down the query; the write path is what guarantees 32 bytes.
fn blob_row(b: Vec<u8>) -> U256 {
    u256_from_blob(&b).unwrap_or(U256::ZERO)
}

pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if current >= SCHEMA_VERSION {
        return Ok(());
    }
    if current < 1 {
        conn.execute_batch(V1)?;
    }
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

const V1: &str = r#"
-- ---------------------------------------------------------------------------------------
-- sessions: one row per run of the sniper, with the limits it ran under.
--
-- The guards are copied in rather than referenced, because `strategy.json` changes and a
-- journal that says "inside the limits" without saying which limits says nothing.
-- ---------------------------------------------------------------------------------------
CREATE TABLE sessions (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    mode                TEXT    NOT NULL,
    wallet              TEXT    NOT NULL,
    started_at          INTEGER NOT NULL,
    ended_at            INTEGER,
    session_budget_wei  BLOB    NOT NULL,
    size_per_buy_wei    BLOB    NOT NULL,
    position_cap_wei    BLOB    NOT NULL,
    max_open_positions  INTEGER NOT NULL,
    strategy_json       TEXT    NOT NULL
) STRICT;

-- ---------------------------------------------------------------------------------------
-- positions: one row per token per session. Adding to a position is another fill against
-- this row, never a second row -- the guards cap cost per token, and two rows would let
-- that cap be passed twice.
-- ---------------------------------------------------------------------------------------
CREATE TABLE positions (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id     INTEGER NOT NULL REFERENCES sessions(id),
    token          TEXT    NOT NULL,
    curve          TEXT    NOT NULL,
    symbol         TEXT    NOT NULL,
    pair           TEXT    NOT NULL,
    opened_at      INTEGER NOT NULL,
    launch_block   INTEGER NOT NULL,
    closed_at      INTEGER,
    cost_wei       BLOB    NOT NULL,
    proceeds_wei   BLOB    NOT NULL,
    tokens_bought  BLOB    NOT NULL,
    tokens_held    BLOB    NOT NULL,
    -- Best multiple seen since entry, in bps. Stored rather than recomputed: a restart
    -- that forgot the peak would silently widen every trailing stop to the current price.
    peak_bps       INTEGER NOT NULL,
    last_mark_bps  INTEGER,
    close_reason   TEXT,
    UNIQUE (session_id, token)
) STRICT;

CREATE INDEX positions_open ON positions (closed_at);

-- ---------------------------------------------------------------------------------------
-- fills: every buy and sell, with the rule that caused it and what it actually paid.
--
-- `tx_hash` is NULL in TEST, where nothing was sent. That is deliberate: a simulated fill
-- must not be indistinguishable from a real one in the record.
-- ---------------------------------------------------------------------------------------
CREATE TABLE fills (
    id                   INTEGER PRIMARY KEY AUTOINCREMENT,
    position_id          INTEGER NOT NULL REFERENCES positions(id),
    at                   INTEGER NOT NULL,
    side                 INTEGER NOT NULL,
    quote_wei            BLOB    NOT NULL,
    tokens               BLOB    NOT NULL,
    tx_hash              TEXT,
    venue                TEXT    NOT NULL,
    rule                 TEXT    NOT NULL,
    detail               TEXT    NOT NULL,
    snipe_tax_wei        BLOB,
    tax_bps_at_decision  INTEGER,
    gas_wei              BLOB
) STRICT;

CREATE INDEX fills_position ON fills (position_id);

-- ---------------------------------------------------------------------------------------
-- refusals: every launch that did not become a position, and why.
--
-- The refusal is the product (spec sec.3.4). "Nothing fired all afternoon" is a question
-- the program has to be able to answer, and it cannot answer it from data it did not keep.
-- ---------------------------------------------------------------------------------------
CREATE TABLE refusals (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  INTEGER NOT NULL REFERENCES sessions(id),
    at          INTEGER NOT NULL,
    token       TEXT    NOT NULL,
    symbol      TEXT    NOT NULL,
    block       INTEGER NOT NULL,
    rule        TEXT    NOT NULL,
    detail      TEXT    NOT NULL
) STRICT;

CREATE INDEX refusals_session_rule ON refusals (session_id, rule);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn wei(n: u64) -> U256 {
        U256::from(n).saturating_mul(U256::from(10_000_000_000_000_000u64))
    }

    fn journal() -> (Journal, i64) {
        let mut j = Journal::in_memory().unwrap();
        let id = j
            .begin_session(&NewSession {
                mode: "test".into(),
                wallet: Address::repeat_byte(1),
                started_at: 1_000,
                session_budget_wei: wei(100),
                size_per_buy_wei: wei(1),
                position_cap_wei: wei(2),
                max_open_positions: 3,
                strategy_json: "{}".into(),
            })
            .unwrap();
        (j, id)
    }

    fn position(j: &mut Journal, session: i64, token: u8) -> i64 {
        j.open_position(&NewPosition {
            session_id: session,
            token: Address::repeat_byte(token),
            curve: Address::repeat_byte(token + 100),
            symbol: "TKN".into(),
            pair: "ETH".into(),
            opened_at: 1_001,
            launch_block: 900_000,
        })
        .unwrap()
    }

    fn buy(pos: i64, quote: U256, tokens: u64) -> Fill {
        Fill {
            position_id: pos,
            at: 1_002,
            side: Side::Buy,
            quote_wei: quote,
            tokens: U256::from(tokens),
            tx_hash: None,
            venue: "curve".into(),
            rule: "entry".into(),
            detail: "passed all rules".into(),
            snipe_tax_wei: Some(U256::from(7u64)),
            tax_bps_at_decision: Some(250),
            gas_wei: None,
        }
    }

    #[test]
    fn a_buy_moves_the_position_it_belongs_to() {
        let (mut j, s) = journal();
        let p = position(&mut j, s, 1);
        j.record_fill(&buy(p, wei(1), 5_000)).unwrap();
        let pos = &j.positions(s, true).unwrap()[0];
        assert_eq!(pos.cost_wei, wei(1));
        assert_eq!(pos.tokens_bought, U256::from(5_000u64));
        assert_eq!(pos.tokens_held, U256::from(5_000u64));
        assert_eq!(pos.remaining_bps(), 10_000);
    }

    /// Partial exits are counted against the ORIGINAL size, which is what the exit rules
    /// read. Counting against what is left would make every partial fire again.
    #[test]
    fn a_partial_sale_leaves_remaining_bps_of_the_original() {
        let (mut j, s) = journal();
        let p = position(&mut j, s, 1);
        j.record_fill(&buy(p, wei(1), 10_000)).unwrap();
        j.record_fill(&Fill {
            side: Side::Sell,
            quote_wei: wei(1),
            tokens: U256::from(3_000u64),
            rule: "partial".into(),
            ..buy(p, wei(1), 0)
        })
        .unwrap();
        let pos = &j.positions(s, true).unwrap()[0];
        assert_eq!(pos.remaining_bps(), 7_000);
        assert_eq!(
            pos.tokens_bought,
            U256::from(10_000u64),
            "the original stands"
        );
        assert_eq!(pos.proceeds_wei, wei(1));
    }

    /// Adding to a position is another fill, not another row: two rows for one token would
    /// let the per-token cap be passed twice.
    #[test]
    fn adding_to_a_position_does_not_open_a_second_one() {
        let (mut j, s) = journal();
        let a = position(&mut j, s, 1);
        let b = position(&mut j, s, 1);
        assert_eq!(a, b);
        j.record_fill(&buy(a, wei(1), 5_000)).unwrap();
        j.record_fill(&buy(b, wei(1), 4_000)).unwrap();
        let all = j.positions(s, false).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].cost_wei, wei(2));
        assert_eq!(all[0].tokens_held, U256::from(9_000u64));
    }

    /// The peak carries forward, because it is what the trailing stop reads.
    #[test]
    fn the_peak_only_ever_rises() {
        let (mut j, s) = journal();
        let p = position(&mut j, s, 1);
        j.mark(p, 25_000).unwrap();
        j.mark(p, 12_000).unwrap();
        let pos = &j.positions(s, true).unwrap()[0];
        assert_eq!(pos.peak_bps, 25_000);
        assert_eq!(pos.last_mark_bps, Some(12_000));
    }

    /// A position survives a restart, which is what makes reconciliation possible.
    #[test]
    fn open_positions_are_findable_without_knowing_the_session() {
        let (mut j, s) = journal();
        let p = position(&mut j, s, 1);
        j.record_fill(&buy(p, wei(1), 5_000)).unwrap();
        assert_eq!(j.all_open_positions().unwrap().len(), 1);
        j.close_position(p, 2_000, "take_profit at 2.00x").unwrap();
        assert!(j.all_open_positions().unwrap().is_empty());
        let closed = &j.positions(s, false).unwrap()[0];
        assert_eq!(closed.close_reason.as_deref(), Some("take_profit at 2.00x"));
        assert!(!closed.is_open());
    }

    /// A session that died holding something is findable, so the user is told rather than
    /// left owning a token the program forgot.
    #[test]
    fn a_session_that_ended_holding_something_is_surfaced() {
        let (mut j, s) = journal();
        let p = position(&mut j, s, 1);
        j.record_fill(&buy(p, wei(1), 5_000)).unwrap();
        assert_eq!(j.unfinished_sessions().unwrap().len(), 1);
        j.close_position(p, 2_000, "sold").unwrap();
        assert!(j.unfinished_sessions().unwrap().is_empty());
    }

    /// A TEST fill has no transaction hash, so the record cannot pass a simulation off as
    /// a trade.
    #[test]
    fn a_test_fill_carries_no_transaction_hash() {
        let (mut j, s) = journal();
        let p = position(&mut j, s, 1);
        j.record_fill(&buy(p, wei(1), 5_000)).unwrap();
        let f = &j.fills(p).unwrap()[0];
        assert_eq!(f.tx_hash, None);
        assert_eq!(f.snipe_tax_wei, Some(U256::from(7u64)));
        assert_eq!(f.tax_bps_at_decision, Some(250));
    }

    /// The answer to "why did nothing fire this afternoon".
    #[test]
    fn refusals_group_by_rule_commonest_first() {
        let (mut j, s) = journal();
        for (i, rule) in [
            "creator_tax",
            "require_twitter",
            "creator_tax",
            "creator_tax",
        ]
        .iter()
        .enumerate()
        {
            j.record_refusal(&Refusal {
                session_id: s,
                at: 1_000 + i as u64,
                token: Address::repeat_byte(i as u8),
                symbol: "TKN".into(),
                block: 900_000 + i as u64,
                rule: (*rule).into(),
                detail: format!("{rule} said no"),
            })
            .unwrap();
        }
        let by_rule = j.refusals_by_rule(s).unwrap();
        assert_eq!(by_rule[0], ("creator_tax".into(), 3));
        assert_eq!(by_rule[1], ("require_twitter".into(), 1));
        assert_eq!(j.recent_refusals(s, 2).unwrap().len(), 2);
    }

    #[test]
    fn totals_add_up_in_u256_not_in_sqlite() {
        let (mut j, s) = journal();
        let a = position(&mut j, s, 1);
        let b = position(&mut j, s, 2);
        j.record_fill(&buy(a, wei(1), 5_000)).unwrap();
        j.record_fill(&buy(b, wei(2), 5_000)).unwrap();
        j.close_position(b, 2_000, "stop_loss").unwrap();
        let t = j.totals(s).unwrap();
        assert_eq!(t.positions, 2);
        assert_eq!(t.open, 1);
        assert_eq!(t.spent_wei, wei(3));
    }

    #[test]
    fn a_journal_reopens_with_its_rows_intact() {
        let dir = std::env::temp_dir().join(format!("quarrel-journal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("live.db");
        {
            let mut j = Journal::open(&path).unwrap();
            let s = j
                .begin_session(&NewSession {
                    mode: "live".into(),
                    wallet: Address::repeat_byte(1),
                    started_at: 1_000,
                    session_budget_wei: wei(100),
                    size_per_buy_wei: wei(1),
                    position_cap_wei: wei(2),
                    max_open_positions: 3,
                    strategy_json: "{}".into(),
                })
                .unwrap();
            let p = position(&mut j, s, 1);
            j.record_fill(&buy(p, wei(1), 5_000)).unwrap();
        }
        let j = Journal::open(&path).unwrap();
        assert_eq!(j.all_open_positions().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fresh_journal_is_at_the_current_schema_version() {
        let j = Journal::in_memory().unwrap();
        let v: i64 = j
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }
}
