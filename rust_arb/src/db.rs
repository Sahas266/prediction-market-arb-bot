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

CREATE INDEX IF NOT EXISTS idx_books_log_canonical_id ON books_log(canonical_id);
CREATE INDEX IF NOT EXISTS idx_books_log_ts_received ON books_log(ts_received);
CREATE INDEX IF NOT EXISTS idx_opportunities_canonical_id ON opportunities(canonical_id);
CREATE INDEX IF NOT EXISTS idx_opportunities_detected_at ON opportunities(detected_at);
CREATE INDEX IF NOT EXISTS idx_orders_opportunity_id ON orders(opportunity_id);
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

pub fn log_order(
    conn: &Connection,
    order_id_local: &str,
    venue: &str,
    native_order_id: &str,
    opportunity_id: &str,
    side: &str,
    action: &str,
    price: &str,
    size: &str,
    status: &str,
) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO orders (order_id_local, venue, native_order_id, opportunity_id, \
         side, action, price, size, status, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        rusqlite::params![
            order_id_local,
            venue,
            native_order_id,
            opportunity_id,
            side,
            action,
            price,
            size,
            status,
            now,
            now,
        ],
    )?;
    Ok(())
}

pub fn update_order_status(conn: &Connection, order_id_local: &str, status: &str) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE orders SET status = ?1, updated_at = ?2 WHERE order_id_local = ?3",
        rusqlite::params![status, now, order_id_local],
    )?;
    Ok(())
}

/// Delete books_log rows older than `retention_days` days.
/// Returns the number of rows deleted.
pub fn prune_books_log(conn: &Connection, retention_days: u32) -> Result<usize> {
    if retention_days == 0 {
        return Ok(0);
    }
    let rows = conn.execute(
        "DELETE FROM books_log WHERE ts_received < datetime('now', ?1)",
        rusqlite::params![format!("-{} days", retention_days)],
    )?;
    Ok(rows)
}

pub fn log_fill(
    conn: &Connection,
    fill_id: &str,
    order_id_local: &str,
    venue: &str,
    price: &str,
    size: &str,
    fee: &str,
) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO fills (fill_id, order_id_local, venue, price, size, fee, filled_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![fill_id, order_id_local, venue, price, size, fee, now],
    )?;
    Ok(())
}
