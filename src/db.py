from __future__ import annotations

import sqlite3
from pathlib import Path

from .config import PROJECT_ROOT

DB_PATH = PROJECT_ROOT / "data" / "arb.db"

SCHEMA = """
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
"""


def get_connection() -> sqlite3.Connection:
    DB_PATH.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(str(DB_PATH))
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA journal_mode=WAL")
    return conn


def init_db() -> None:
    conn = get_connection()
    conn.executescript(SCHEMA)
    conn.close()


def log_book(conn: sqlite3.Connection, venue: str, native_market_id: str,
             canonical_id: str | None, buy_yes: str, buy_no: str,
             depth_buy_yes: str, depth_buy_no: str, fee_rate: str,
             ts_exchange: str | None, ts_received: str) -> None:
    conn.execute(
        "INSERT INTO books_log (venue, native_market_id, canonical_id, buy_yes, buy_no, "
        "depth_buy_yes, depth_buy_no, fee_rate, ts_exchange, ts_received) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (venue, native_market_id, canonical_id, buy_yes, buy_no,
         depth_buy_yes, depth_buy_no, fee_rate, ts_exchange, ts_received),
    )
    conn.commit()


def log_opportunity(conn: sqlite3.Connection, opp_id: str, canonical_id: str,
                    yes_venue: str, no_venue: str, buy_yes_price: str,
                    buy_no_price: str, gross_edge: str, net_edge: str,
                    max_size: str, detected_at: str) -> None:
    conn.execute(
        "INSERT INTO opportunities (opportunity_id, canonical_id, yes_venue, no_venue, "
        "buy_yes_price, buy_no_price, gross_edge, net_edge, max_size, detected_at) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (opp_id, canonical_id, yes_venue, no_venue, buy_yes_price,
         buy_no_price, gross_edge, net_edge, max_size, detected_at),
    )
    conn.commit()
