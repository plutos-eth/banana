//! The `history.db` schema and its migrations.
//!
//! Append-only indexed facts. Never rewritten (spec §4.1), so `index --update` can add
//! blocks without ever touching a row it wrote before.
//!
//! # How money is stored (PLAN.md D1)
//!
//! Raw amounts are `BLOB(32)`, big-endian `U256`. Derived analytical figures are `INTEGER`
//! basis points. Both, deliberately:
//!
//! * At ~1M trade rows per indexed day the blob is materially smaller than decimal text
//!   and needs no parse on the hot indexing path.
//! * SQLite cannot sum a blob -- but it never needs to, because the Lab aggregates
//!   `outcomes`, whose columns are already integer bps. That keeps analytics in SQL rather
//!   than in Rust, as §4.1 requires.
//!
//! There is no `REAL` column anywhere in this file, and there never should be.
//!
//! # Why the analytical tables are one row per token
//!
//! `outcomes` and `pit_features` carry one row per launch -- roughly 20,000 a day, 600,000
//! a month. `trades` carries ~1M a day. The Lab reads only the first two, which is the
//! concrete reason SQLite is sufficient and DuckDB is not needed (spec §4.1, PLAN.md D2).

use rusqlite::Connection;

/// Bumped whenever a migration is added. `user_version` in the database is compared
/// against this.
pub const SCHEMA_VERSION: i64 = 1;

/// Apply any migrations the database is missing.
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

/// Settings applied to every connection.
pub fn apply_pragmas(conn: &Connection) -> rusqlite::Result<()> {
    // WAL is what lets the desktop app read `history.db` while a CLI index writes it,
    // which spec §10 requires and a directory-wide lock would have made impossible.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // NORMAL is the right trade for an append-only store that can always be re-indexed:
    // a power cut can lose the last transaction, not the database.
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    // 64 MB page cache. The trade scan writes in large batches.
    conn.pragma_update(None, "cache_size", -64_000)?;
    Ok(())
}

const V1: &str = r#"
-- ---------------------------------------------------------------------------------------
-- launches: one row per TokenLaunched.
-- ---------------------------------------------------------------------------------------
CREATE TABLE launches (
    token                TEXT    PRIMARY KEY,
    curve                TEXT    NOT NULL,
    deployer             TEXT    NOT NULL,
    pair_token           TEXT    NOT NULL,
    launch_config_id     INTEGER NOT NULL,
    graduation_threshold BLOB    NOT NULL,
    block                INTEGER NOT NULL,
    tx_hash              TEXT    NOT NULL,
    log_index            INTEGER NOT NULL,
    -- Interpolated from block_anchors, so approximate by construction (PLAN.md F2).
    -- Nullable because a launch can be recorded before the anchor scan reaches it.
    ts                   INTEGER,
    UNIQUE (tx_hash, log_index)
) STRICT;

CREATE INDEX launches_block          ON launches (block);
CREATE INDEX launches_curve          ON launches (curve);
CREATE INDEX launches_deployer_block ON launches (deployer, block);

-- ---------------------------------------------------------------------------------------
-- trades: every CurveBuy and CurveSell.
--
-- The natural key is (tx_hash, log_index), which is what makes re-indexing an already
-- covered range write zero rows (spec §6.2).
--
-- snipe_tax is the amount from a SnipeTaxCharged emitted by the same curve in the same
-- transaction, or NULL when there was none. Its ABSENCE on the first post-launch buy is
-- what identifies the end of the opening-tax window, and therefore the entry price,
-- without needing any block timestamps (PLAN.md F2).
-- ---------------------------------------------------------------------------------------
CREATE TABLE trades (
    tx_hash    TEXT    NOT NULL,
    log_index  INTEGER NOT NULL,
    curve      TEXT    NOT NULL,
    block      INTEGER NOT NULL,
    tx_index   INTEGER NOT NULL,
    -- 0 = buy, 1 = sell.
    side       INTEGER NOT NULL,
    actor      TEXT    NOT NULL,
    recipient  TEXT    NOT NULL,
    -- buy: quote_in / tokens_out.  sell: tokens_in / quote_out.
    amount_in  BLOB    NOT NULL,
    amount_out BLOB    NOT NULL,
    fee        BLOB    NOT NULL,
    tax        BLOB    NOT NULL,
    snipe_tax  BLOB,
    PRIMARY KEY (tx_hash, log_index)
) STRICT, WITHOUT ROWID;

CREATE INDEX trades_curve_block ON trades (curve, block, log_index);
CREATE INDEX trades_block       ON trades (block);

-- ---------------------------------------------------------------------------------------
-- graduations and sweeps: factory-address-filtered, so cheap to scan.
-- ---------------------------------------------------------------------------------------
CREATE TABLE graduations (
    token     TEXT    PRIMARY KEY,
    block     INTEGER NOT NULL,
    tx_hash   TEXT    NOT NULL,
    log_index INTEGER NOT NULL
) STRICT;

CREATE INDEX graduations_block ON graduations (block);

CREATE TABLE sweeps (
    token     TEXT    PRIMARY KEY,
    block     INTEGER NOT NULL,
    tx_hash   TEXT    NOT NULL,
    log_index INTEGER NOT NULL
) STRICT;

-- ---------------------------------------------------------------------------------------
-- launch_configs: read once per config id, not once per launch.
-- ---------------------------------------------------------------------------------------
CREATE TABLE launch_configs (
    id                   INTEGER PRIMARY KEY,
    supply               BLOB    NOT NULL,
    curve_fee_bps        INTEGER NOT NULL,
    phantom_quote        BLOB    NOT NULL,
    graduation_threshold BLOB    NOT NULL
) STRICT;

-- ---------------------------------------------------------------------------------------
-- pair_economics: phantom quote and graduation threshold, PER PAIR TOKEN.
--
-- Spec sec.2 gives "4.2 ETH real quote against a 1.68 ETH phantom reserve", which holds
-- only for ETH-paired launches -- measured at 40% of the universe. The rest pair against
-- 40-odd other tokens, each with its own economics, and a curve cannot be replayed without
-- the right phantom reserve for its pair.
--
-- Read once per distinct pair token, not once per launch.
-- ---------------------------------------------------------------------------------------
CREATE TABLE pair_economics (
    pair_token           TEXT PRIMARY KEY,
    phantom_quote        BLOB NOT NULL,
    graduation_threshold BLOB NOT NULL,
    decimals             INTEGER NOT NULL
) STRICT;

-- ---------------------------------------------------------------------------------------
-- block_anchors: sampled (block, timestamp) pairs.
--
-- Times between anchors are interpolated. Block production measured extremely regular
-- (100.87 ms mean over 800,000 blocks), so a sample every few hundred blocks holds error
-- well under a second -- which is fine for 5m/30m holds and the 6h maturity cutoff, and
-- is never used for the entry point, which needs no timestamp at all (PLAN.md F2).
-- ---------------------------------------------------------------------------------------
CREATE TABLE block_anchors (
    block INTEGER PRIMARY KEY,
    ts    INTEGER NOT NULL
) STRICT;

-- ---------------------------------------------------------------------------------------
-- enrichment: what the launch transaction declared.
--
-- Point-in-time by construction: this is the transaction that created the token, not a
-- current-state read (docs/FINDINGS.md §4).
--
-- Presence columns use 0 = absent, 1 = present, 2 = UNKNOWN. The third value is not
-- decoration: a launch that did not decode must never be recorded as "has no Twitter",
-- which is a lie in the direction that flatters a backtest.
-- ---------------------------------------------------------------------------------------
CREATE TABLE enrichment (
    token                 TEXT    PRIMARY KEY REFERENCES launches (token),
    -- 0 when the launch did not go through launchAndBuy.
    decoded               INTEGER NOT NULL,
    selector              TEXT,
    name                  TEXT,
    symbol                TEXT,
    description           TEXT,
    -- Deployer-controlled URL. Stored and displayed as text; NEVER fetched (PLAN.md C1).
    logo                  TEXT,
    twitter_url           TEXT,
    website_url           TEXT,
    telegram_url          TEXT,
    twitter               INTEGER NOT NULL,
    website               INTEGER NOT NULL,
    telegram              INTEGER NOT NULL,
    -- NULL when unreadable. Recording it as 0 would let max_exempt_wallets pass a launch
    -- whose bundle was never seen.
    exempt_wallets        INTEGER,
    creator_fee_recipient TEXT,
    creator_tax_bps       INTEGER,
    declared_quote_in     BLOB,
    -- Ground truth from the CurveBuy in the launch transaction, whichever route created
    -- the token. Independent of whether the calldata decoded.
    dev_buy_quote         BLOB,
    dev_buy_tokens        BLOB,
    dev_buy_bps           INTEGER
) STRICT;

-- ---------------------------------------------------------------------------------------
-- pit_features: everything a filter may read, as of launch_block.
--
-- A regression test asserts nothing here is derived from a block >= launch_block.
-- ---------------------------------------------------------------------------------------
CREATE TABLE pit_features (
    token                         TEXT    PRIMARY KEY REFERENCES launches (token),
    deployer_launches             INTEGER NOT NULL,
    deployer_graduations          INTEGER NOT NULL,
    -- NULL when the deployer has no prior launches: "never launched" and "launched nine
    -- times and never graduated" are opposite signals and a zero would merge them.
    deployer_grad_rate_bps        INTEGER,
    fingerprint                   TEXT    NOT NULL,
    fingerprint_twins_30m         INTEGER NOT NULL,
    -- How much prior history actually existed for this launch (PLAN.md C2). Any strategy
    -- using a deployer feature restricts the universe by this, and says so in the funnel.
    deployer_history_depth_blocks INTEGER NOT NULL
) STRICT;

CREATE INDEX pit_fingerprint ON pit_features (fingerprint);

-- ---------------------------------------------------------------------------------------
-- outcomes: the precomputed fate of each token. One row, computed once at index time.
--
-- Prices are quote-wei per 10^18 tokens, as U256 blobs. Multiples are integer bps.
-- ---------------------------------------------------------------------------------------
CREATE TABLE outcomes (
    token              TEXT    PRIMARY KEY REFERENCES launches (token),

    -- 0 = ObservedUntaxedBuy: a real fill by a real buyer after the tax window.
    -- 1 = ReconstructedAtWindowEnd: no untaxed buy exists, so the curve was replayed to
    --     the end of the window and a reference-size buy quoted against it.
    --
    -- These rows must NOT be dropped. The live sniper would have entered them -- it waits
    -- for the decay and fires -- so dropping them removes exactly the worst outcomes from
    -- the denominator, which is the survivorship bias §5.5 exists to prevent (PLAN.md F9).
    entry_rule         INTEGER NOT NULL,
    entry_block        INTEGER,
    entry_price        BLOB,
    entry_tokens       BLOB,

    ath_price          BLOB,
    ath_block          INTEGER,
    max_multiple_bps   INTEGER,
    time_to_ath_s      INTEGER,

    mult_after_5m_bps  INTEGER,
    mult_after_30m_bps INTEGER,

    migrated           INTEGER NOT NULL DEFAULT 0,
    died               INTEGER NOT NULL DEFAULT 0,

    -- Post-entry facts. DISPLAY ONLY -- these live in a different Rust type from the
    -- filterable features so a rule cannot read them (spec §5.2, §5.3).
    distinct_buyers_1m    INTEGER,
    every_early_buy_taxed INTEGER,

    -- Supporting counts, so `died` and the maturity cutoff are reproducible rather than
    -- asserted (PLAN.md C4).
    post_entry_trades  INTEGER NOT NULL DEFAULT 0,
    last_trade_block   INTEGER,
    -- Blocks of trade history available after entry, which is what the maturity cutoff
    -- actually tests.
    observed_blocks    INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE INDEX outcomes_entry_rule ON outcomes (entry_rule);
CREATE INDEX outcomes_migrated   ON outcomes (migrated);

-- ---------------------------------------------------------------------------------------
-- index_state: last fully-processed block per phase, for resume (spec §6.2).
--
-- Checkpointed after every chunk, so a network failure at 70% resumes at 70%.
-- ---------------------------------------------------------------------------------------
CREATE TABLE index_state (
    phase       TEXT    PRIMARY KEY,
    from_block  INTEGER NOT NULL,
    last_block  INTEGER NOT NULL,
    target_block INTEGER NOT NULL,
    rows_written INTEGER NOT NULL DEFAULT 0,
    updated_at  INTEGER NOT NULL
) STRICT;
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        apply_pragmas_for_memory(&c);
        migrate(&c).unwrap();
        c
    }

    /// WAL is not available for an in-memory database, so tests set only what applies.
    fn apply_pragmas_for_memory(c: &Connection) {
        c.pragma_update(None, "foreign_keys", "ON").unwrap();
    }

    #[test]
    fn migration_is_idempotent() {
        let c = db();
        migrate(&c).unwrap();
        migrate(&c).unwrap();
        let v: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[test]
    fn every_expected_table_exists() {
        let c = db();
        let mut stmt = c
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        let tables: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        for want in [
            "block_anchors",
            "enrichment",
            "graduations",
            "index_state",
            "launch_configs",
            "launches",
            "outcomes",
            "pit_features",
            "sweeps",
            "trades",
        ] {
            assert!(tables.contains(&want.to_string()), "missing table {want}");
        }
    }

    #[test]
    fn no_column_anywhere_is_a_float() {
        // Spec §12: f64 creeping into the money path. A REAL column is how it would get
        // into the database, so the schema is checked rather than trusted.
        let c = db();
        let mut stmt = c
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            )
            .unwrap();
        let tables: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        for t in tables {
            let mut s = c.prepare(&format!("PRAGMA table_info({t})")).unwrap();
            let types: Vec<String> = s
                .query_map([], |r| r.get::<_, String>(2))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            for ty in types {
                assert_ne!(
                    ty.to_uppercase(),
                    "REAL",
                    "table {t} has a REAL column; money is U256 blobs and ratios are integer bps"
                );
            }
        }
    }

    #[test]
    fn the_trade_natural_key_rejects_a_duplicate() {
        // This is what makes re-indexing a covered range write zero rows (spec §6.2).
        let c = db();
        let insert = "INSERT INTO trades (tx_hash, log_index, curve, block, tx_index, side, \
                      actor, recipient, amount_in, amount_out, fee, tax) \
                      VALUES ('0xaa', 3, '0xc', 10, 0, 0, '0x1', '0x1', x'00', x'00', x'00', x'00')";
        c.execute(insert, []).unwrap();
        let err = c.execute(insert, []).unwrap_err();
        assert!(
            err.to_string().contains("UNIQUE"),
            "duplicate must be rejected: {err}"
        );
    }

    #[test]
    fn a_launch_cannot_be_recorded_twice_under_the_same_log() {
        let c = db();
        let ins = "INSERT INTO launches (token, curve, deployer, pair_token, launch_config_id, \
                   graduation_threshold, block, tx_hash, log_index) \
                   VALUES (?, '0xc', '0xd', '0x0', 0, x'00', 5, '0xtx', 1)";
        c.execute(ins, ["0xtoken1"]).unwrap();
        // Same (tx_hash, log_index), different token: still a duplicate log.
        let err = c.execute(ins, ["0xtoken2"]).unwrap_err();
        assert!(err.to_string().contains("UNIQUE"), "{err}");
    }

    #[test]
    fn strict_tables_reject_a_wrongly_typed_value() {
        // STRICT is why a stray float or string cannot land in an integer column.
        let c = db();
        let err = c
            .execute(
                "INSERT INTO block_anchors (block, ts) VALUES (1, 'not-a-number')",
                [],
            )
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("cannot store TEXT value in INTEGER column"),
            "{err}"
        );
    }
}
