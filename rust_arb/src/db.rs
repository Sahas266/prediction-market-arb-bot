use anyhow::Result;
use rusqlite::Connection;
use std::path::PathBuf;

use crate::config::project_root;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS canonical_contracts (
    canonical_id   TEXT PRIMARY KEY,
    title          TEXT NOT NULL,
    subject_key    TEXT NOT NULL,
    resolution_source TEXT,
    cutoff_time_utc TEXT NOT NULL,
    category       TEXT,
    created_at     TEXT DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS venue_mappings (
    canonical_id     TEXT NOT NULL,
    venue            TEXT NOT NULL,
    native_market_id TEXT NOT NULL,
    yes_token_id     TEXT,
    no_token_id      TEXT,
    neg_risk         INTEGER DEFAULT 0,
    confidence       TEXT NOT NULL,
    method           TEXT NOT NULL,
    is_verified      INTEGER DEFAULT 0,
    PRIMARY KEY (canonical_id, venue),
    FOREIGN KEY (canonical_id) REFERENCES canonical_contracts(canonical_id)
);

CREATE TABLE IF NOT EXISTS books_log (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    venue            TEXT NOT NULL,
    native_market_id TEXT NOT NULL,
    canonical_id     TEXT,
    buy_yes          TEXT NOT NULL,
    buy_no           TEXT NOT NULL,
    depth_buy_yes    TEXT,
    depth_buy_no     TEXT,
    fee_rate         TEXT,
    ts_exchange      TEXT,
    ts_received      TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS opportunities (
    opportunity_id TEXT PRIMARY KEY,
    canonical_id   TEXT NOT NULL,
    yes_venue      TEXT NOT NULL,
    no_venue       TEXT NOT NULL,
    buy_yes_price  TEXT NOT NULL,
    buy_no_price   TEXT NOT NULL,
    gross_edge     TEXT NOT NULL,
    net_edge       TEXT NOT NULL,
    max_size       TEXT NOT NULL,
    detected_at    TEXT NOT NULL,
    status         TEXT DEFAULT 'detected'
);

CREATE TABLE IF NOT EXISTS orders (
    order_id_local   TEXT PRIMARY KEY,
    venue            TEXT NOT NULL,
    native_order_id  TEXT,
    opportunity_id   TEXT NOT NULL,
    side             TEXT NOT NULL,
    action           TEXT NOT NULL,
    price            TEXT NOT NULL,
    size             TEXT NOT NULL,
    status           TEXT NOT NULL,
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL,
    FOREIGN KEY (opportunity_id) REFERENCES opportunities(opportunity_id)
);

CREATE TABLE IF NOT EXISTS fills (
    fill_id        TEXT PRIMARY KEY,
    order_id_local TEXT NOT NULL,
    venue          TEXT NOT NULL,
    price          TEXT NOT NULL,
    size           TEXT NOT NULL,
    fee            TEXT NOT NULL,
    filled_at      TEXT NOT NULL,
    FOREIGN KEY (order_id_local) REFERENCES orders(order_id_local)
);

CREATE TABLE IF NOT EXISTS positions (
    canonical_id TEXT NOT NULL,
    venue        TEXT NOT NULL,
    yes_qty      TEXT DEFAULT '0',
    no_qty       TEXT DEFAULT '0',
    avg_yes_cost TEXT DEFAULT '0',
    avg_no_cost  TEXT DEFAULT '0',
    PRIMARY KEY (canonical_id, venue)
);
"#;

fn db_path() -> PathBuf {
    project_root().join("data").join("arb.db")
}

pub fn get_connection() -> Result<Connection> {
    let path = db_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(&path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    Ok(conn)
}

pub fn init_db() -> Result<()> {
    let conn = get_connection()?;
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

pub fn log_book(conn: &Connection, book: &crate::models::CanonicalBook) -> Result<()> {
    let ts_exchange: Option<String> = book.ts_exchange.map(|t| t.to_rfc3339());
    conn.execute(
        "INSERT INTO books_log (venue, native_market_id, canonical_id, buy_yes, buy_no, \
         depth_buy_yes, depth_buy_no, fee_rate, ts_exchange, ts_received) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params![
            book.venue.to_string(),
            book.native_market_id,
            book.canonical_id,
            book.buy_yes.to_string(),
            book.buy_no.to_string(),
            book.depth_buy_yes.to_string(),
            book.depth_buy_no.to_string(),
            book.fee_rate.to_string(),
            ts_exchange,
            book.ts_received.to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn log_opportunity(conn: &Connection, opp: &crate::models::Opportunity) -> Result<()> {
    conn.execute(
        "INSERT INTO opportunities (opportunity_id, canonical_id, yes_venue, no_venue, \
         buy_yes_price, buy_no_price, gross_edge, net_edge, max_size, detected_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params![
            opp.opportunity_id,
            opp.canonical_id,
            opp.yes_venue.to_string(),
            opp.no_venue.to_string(),
            opp.buy_yes_price.to_string(),
            opp.buy_no_price.to_string(),
            opp.gross_edge.to_string(),
            opp.net_edge.to_string(),
            opp.max_size.to_string(),
            opp.detected_at.to_rfc3339(),
        ],
    )?;
    Ok(())
}
