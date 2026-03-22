# Cross-Venue Prediction Market Arbitrage Bot — Detailed Plan

## Status Summary (as of 2026-03-22)

### Completion by Phase

| Phase | Status | Notes |
|-------|--------|-------|
| **Phase 1: Read-Only Foundation** | **COMPLETE** | Rust implementation (ported from Python). Both adapters produce CanonicalBook. 6 manual mappings verified. SQLite logging. Polymarket WS + Kalshi REST polling working. |
| **Phase 2: Opportunity Detection** | **COMPLETE** | Detector runs in main loop with both directions. All filters implemented (freshness, depth, threshold). Opportunities logged to SQLite. CLI real-time output. |
| **Phase 3: Replay & Validation** | **NOT STARTED** | Need 24h+ of logged data before replay analysis. |
| **Phase 4: Execution** | **PARTIAL** | Kalshi order placement implemented (RSA auth, FOK). Polymarket execution not yet ported. Risk manager implemented but untested with real orders. |
| **Phase 5: Hardening** | **NOT STARTED** | Kalshi WS upgrade, error recovery, position reconciliation. |

### What's Built (Rust — `rust_arb/`)

- [x] Config loading (YAML + .env + RSA key parsing)
- [x] Kalshi REST adapter with RSA-PSS-SHA256 auth, orderbook inversion, retry/backoff
- [x] Polymarket CLOB REST + WebSocket adapter with book cache
- [x] Polymarket fee rate caching
- [x] Contract registry with manual mappings loader
- [x] Opportunity detector (both arb directions, fees, slippage, depth/freshness filters)
- [x] Risk manager (rate limiting, kill switch, notional limits)
- [x] Kalshi order execution (FOK, fragile-venue-first)
- [x] SQLite schema + book/opportunity logging
- [x] Main monitoring loop with graceful shutdown
- [x] 7 passing tests (unit + live API integration)
- [x] Auto-discovery tool (`bin/discover_pairs`) for finding new cross-venue candidates

### What's NOT Built Yet

- [ ] Polymarket authenticated trading (EIP-712 signing, HMAC API auth)
- [ ] Kalshi WebSocket (authenticated connection for real-time orderbooks)
- [ ] Replay evaluator (analyze logged opportunity persistence and false positive rate)
- [ ] Position reconciliation (verify local state matches venue state)
- [ ] Trade logging (realized edge vs. expected)
- [ ] Alerting / monitoring (kill switch notifications, daily PnL)

### Next Steps (Priority Order)

1. **Collect data**: Run the monitor for 24-48h to build a `books_log` and `opportunities` dataset
2. **Replay evaluation**: Build `replay_eval` to analyze how long detected opportunities persist, false positive rate, and optimal threshold calibration
3. **Expand mappings**: Run `discover_pairs` regularly to find new cross-venue pairs, verify and promote to manual_mappings.json
4. **Polymarket execution**: Port EIP-712 order signing and HMAC auth to Rust for the trading leg
5. **Paper trade**: Execute with `$0` notional (log would-be trades) to validate execution timing
6. **Live trade (tiny size)**: $5-10 per leg on clearly profitable (5c+ net edge) opportunities
7. **Kalshi WS upgrade**: Replace REST polling with authenticated WebSocket for lower latency

---

## 1. Core Thesis

Buy **YES** on Venue A and **NO** on Venue B (or vice versa) for the *same* binary event. The combined payout is always $1.00 at resolution regardless of outcome:

```
gross_edge = 1 - buy_yes_price_A - buy_no_price_B
net_edge   = gross_edge - fee_A - fee_B - slippage_buffer
```

This is a **locked arbitrage** — not a directional bet. The bot should never speculate. If the net edge after all costs is positive and both books are fresh, it trades. Otherwise it waits.

---

## 2. Venue Integration Summary

### 2.1 Polymarket

| Aspect | Detail |
|--------|--------|
| **APIs** | Gamma (market discovery), CLOB (orderbook + trading), Data (positions/trades) |
| **Base URLs** | Gamma: `gamma-api.polymarket.com`, CLOB: `clob.polymarket.com` |
| **Auth (read)** | None for market data (REST + WebSocket) |
| **Auth (trade)** | Two-tier: L1 (EIP-712 wallet signature to derive API key) → L2 (HMAC-SHA256 per request) |
| **Order types** | GTC, GTD, FOK, FAK — all are limit orders; FOK/FAK simulate market orders |
| **WebSocket** | `wss://ws-subscriptions-clob.polymarket.com/ws/market` — public, no auth |
| **WS events** | `book` (snapshot), `price_change` (delta), `last_trade_price`, `tick_size_change` |
| **WS heartbeat** | Send `PING` every 10s, expect `PONG` |
| **Trading heartbeat** | Must POST heartbeat every 10s or ALL open orders are auto-cancelled |
| **Token model** | ERC-1155 on Polygon; each binary market has YES and NO token IDs |
| **Collateral** | USDC.e on Polygon (`0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174`) |
| **Fees** | Most markets: **zero fees**. Crypto: ~1.56% peak. NCAAB/Serie A: ~0.44% peak. Fee formula: `C * p * feeRate * (p*(1-p))^exponent` |
| **Fee endpoint** | `GET /fee-rate?token_id=TOKEN_ID` → `{ "base_fee": 30 }` (bps) |
| **Tick sizes** | 0.1, 0.01, 0.001, or 0.0001 — varies per market |
| **Rate limits** | CLOB general: 9,000/10s. Book endpoint: 1,500/10s. Order placement: 3,500/10s burst |
| **Neg risk** | Some multi-outcome events use `negRisk: true` — different exchange contract, must flag in orders |
| **SDKs** | Python: `py-clob-client`. TypeScript: `@polymarket/clob-client` |

### 2.2 Kalshi

| Aspect | Detail |
|--------|--------|
| **Base URL** | `https://api.elections.kalshi.com/trade-api/v2` (production — covers all markets, not just elections) |
| **Auth (read)** | REST market data is public (no auth for `GET /markets`, `GET /markets/{ticker}/orderbook`) |
| **Auth (trade)** | RSA-PSS signature per request: sign `{timestamp_ms}{METHOD}{path}` with SHA-256 |
| **Auth headers** | `KALSHI-ACCESS-KEY`, `KALSHI-ACCESS-TIMESTAMP` (ms), `KALSHI-ACCESS-SIGNATURE` (base64) |
| **Order types** | Limit, market. TIF: FOK, GTC, IOC. Also: post_only, reduce_only |
| **WebSocket** | `wss://api.elections.kalshi.com/trade-api/ws/v2` — **requires auth for connection** |
| **WS channels** | `orderbook_delta` (snapshot + incremental), `ticker`, `trade`, `fill`, `user_orders` |
| **WS heartbeat** | Server sends ping ("heartbeat") every 10s; client must respond with pong |
| **Price format** | FixedPointDollars strings (e.g. `"0.4200"`, up to 6 decimals) |
| **Quantity format** | FixedPointCount strings (e.g. `"10.00"`, 0-2 decimals) |
| **Tick structures** | `linear_cent` ($0.01), `deci_cent` ($0.001), `tapered_deci_cent` (mixed) — per-market `price_ranges` array |
| **Fees** | Trade fee (rounded up to $0.0001) + rounding fee − rebate. Separated into `taker_fees_dollars` / `maker_fees_dollars` |
| **Rate limits** | Basic: 20 read/s, 10 write/s. Advanced: 30/30. Premier: 100/100. Prime: 400/400 |
| **Orderbook** | Only shows bids for YES and NO (no asks) — YES bid at $X = NO ask at $(1-X) |
| **Sandbox** | `demo-api.kalshi.co` — separate credentials, safe for testing |
| **Max open orders** | 200,000 per user |

### 2.3 Key Asymmetries Driving Architecture

| Decision | Rationale |
|----------|-----------|
| **Polymarket via WebSocket, Kalshi via REST polling (initially)** | Polymarket's market WS is fully public and push-based. Kalshi's WS requires auth for the connection itself. Start simple: poll Kalshi REST orderbook every 1–2s, then upgrade to authenticated WS later. |
| **Decimal everywhere** | Kalshi uses dollar strings with up to 6 decimal places. Polymarket prices are strings. Use `Decimal` from the start — never `float`. |
| **Separate fee models** | Polymarket's fee depends on price level and is often zero. Kalshi's fee has rounding + rebate mechanics. Each adapter must compute its own effective cost. |
| **Different order signing** | Polymarket: EIP-712 signed orders + HMAC API auth. Kalshi: RSA-PSS request signing. Execution adapters must fully encapsulate this. |

---

## 3. Architecture

```
┌──────────────────────────────────────────────────────┐
│                    Orchestrator                       │
│            (async event loop / scheduler)             │
└──────────┬───────────────┬───────────────┬───────────┘
           │               │               │
           v               v               v
┌──────────────┐  ┌────────────────┐  ┌──────────────┐
│  Market      │  │  Contract      │  │  Config &    │
│  Discovery   │  │  Registry      │  │  Secrets     │
│  Service     │  │  + Matcher     │  │  Manager     │
└──────┬───────┘  └───────┬────────┘  └──────────────┘
       │                  │
       v                  v
┌─────────────────────────────────────────────────────┐
│              Venue Adapter Layer                      │
│  ┌─────────────────────┐  ┌──────────────────────┐  │
│  │  PolymarketAdapter  │  │   KalshiAdapter      │  │
│  │  • WS book stream   │  │   • REST poll loop   │  │
│  │  • REST fallback    │  │   • WS (phase 2)     │  │
│  │  • CLOB execution   │  │   • REST execution   │  │
│  │  • Fee calculator   │  │   • Fee calculator    │  │
│  └─────────┬───────────┘  └──────────┬───────────┘  │
└────────────┼─────────────────────────┼──────────────┘
             │                         │
             v                         v
┌─────────────────────────────────────────────────────┐
│           Canonical Book Cache (in-memory)            │
│   keyed by (canonical_id, venue) → CanonicalBook      │
│   + staleness tracking per entry                      │
└──────────────────────┬──────────────────────────────┘
                       │
                       v
┌─────────────────────────────────────────────────────┐
│              Opportunity Detector                     │
│   • For each canonical pair: compute net_edge both   │
│     directions                                        │
│   • Filter: freshness, depth, threshold, cooldown    │
│   • Emit Opportunity events                           │
└──────────────────────┬──────────────────────────────┘
                       │
                       v
┌─────────────────────────────────────────────────────┐
│                 Risk Manager                          │
│   • Position limits per contract                     │
│   • Max notional per trade                           │
│   • Max open residual / legging risk                 │
│   • Rate limiting (trades/min)                       │
│   • Kill switch on repeated failures                 │
│   • Settlement-window blackout                       │
└──────────────────────┬──────────────────────────────┘
                       │
                       v
┌─────────────────────────────────────────────────────┐
│               Execution Engine                        │
│   • Submit fragile leg first (less liquid venue)     │
│   • Immediately submit hedge leg                     │
│   • Handle partial fills: resize or unwind           │
│   • Log all order state transitions                  │
└──────────────────────┬──────────────────────────────┘
                       │
                       v
┌─────────────────────────────────────────────────────┐
│              Database (SQLite → Postgres)             │
│   opportunities, orders, fills, positions, books_log │
└─────────────────────────────────────────────────────┘
```

---

## 4. Project Structure

> **Note:** Project was ported from Python to Rust for performance. Structure below reflects current Rust implementation.

```
PBC-Hackathon-2026/
├── config.yaml                 # runtime config (thresholds, limits, polling intervals)
├── .env.example                # template for secrets (Kalshi RSA keys)
├── mappings/
│   └── manual_mappings.json    # 6 hand-verified canonical contract mappings
├── rust_arb/
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs             # monitoring loop + orchestrator + tests
│       ├── bin/
│       │   └── discover_pairs.rs  # auto-discovery of cross-venue candidates
│       ├── adapters/
│       │   ├── mod.rs
│       │   ├── kalshi.rs       # Kalshi REST + RSA-PSS auth + order execution
│       │   └── polymarket.rs   # Polymarket WS + CLOB REST + fee caching
│       ├── config.rs           # YAML + .env config loading
│       ├── db.rs               # SQLite schema + CRUD helpers
│       ├── detector.rs         # opportunity detection (both arb directions)
│       ├── executor.rs         # two-leg execution engine (fragile venue first)
│       ├── models.rs           # Venue, CanonicalBook, Opportunity, VenueMapping
│       ├── registry.rs         # contract registry + manual mappings loader
│       └── risk.rs             # risk manager + kill switch
└── data/
    └── arb.db                  # SQLite database (gitignored)
```

---

## 5. Data Models

### 5.1 Core Dataclasses

```python
from __future__ import annotations
from dataclasses import dataclass, field
from decimal import Decimal
from datetime import datetime, timezone
from enum import Enum
from typing import Optional


class Venue(str, Enum):
    POLYMARKET = "polymarket"
    KALSHI = "kalshi"


class Side(str, Enum):
    YES = "yes"
    NO = "no"


class OrderStatus(str, Enum):
    PENDING = "pending"
    SUBMITTED = "submitted"
    FILLED = "filled"
    PARTIAL = "partial"
    CANCELLED = "cancelled"
    FAILED = "failed"


@dataclass(frozen=True)
class CanonicalContract:
    canonical_id: str               # deterministic hash or manual key
    title: str                      # human-readable description
    subject_key: str                # normalized: "duke_beats_tcu"
    resolution_source: str          # "official_ncaa_result"
    cutoff_time_utc: datetime
    category: str                   # "sports", "politics", "crypto", etc.


@dataclass(frozen=True)
class VenueMapping:
    canonical_id: str
    venue: Venue
    native_market_id: str           # ticker (Kalshi) or condition_id (Polymarket)
    yes_token_id: Optional[str]     # Polymarket token ID for YES outcome
    no_token_id: Optional[str]      # Polymarket token ID for NO outcome
    neg_risk: bool                  # Polymarket neg-risk flag
    confidence: Decimal             # 0.0–1.0
    method: str                     # "manual", "rule", "nlp"
    is_verified: bool


@dataclass
class CanonicalBook:
    venue: Venue
    native_market_id: str
    canonical_id: str
    buy_yes: Decimal                # best price to BUY a YES (ask side)
    buy_no: Decimal                 # best price to BUY a NO (ask side)
    depth_buy_yes: Decimal          # visible size at best ask for YES
    depth_buy_no: Decimal           # visible size at best ask for NO
    fee_rate: Decimal               # effective fee rate for this venue+market
    tick_size: Decimal
    min_order_size: Decimal
    ts_exchange: Optional[datetime]
    ts_received: datetime

    def age_ms(self) -> float:
        delta = datetime.now(timezone.utc) - self.ts_received
        return delta.total_seconds() * 1000

    def is_fresh(self, max_age_ms: int = 2000) -> bool:
        return self.age_ms() <= max_age_ms


@dataclass
class Opportunity:
    opportunity_id: str
    canonical_id: str
    yes_venue: Venue
    no_venue: Venue
    buy_yes_price: Decimal
    buy_no_price: Decimal
    gross_edge: Decimal
    net_edge: Decimal
    max_size: Decimal
    detected_at: datetime
    yes_book_age_ms: float
    no_book_age_ms: float


@dataclass
class Order:
    order_id_local: str
    venue: Venue
    native_order_id: Optional[str]
    opportunity_id: str
    side: Side
    action: str                     # "buy"
    price: Decimal
    size: Decimal
    status: OrderStatus
    created_at: datetime
    updated_at: datetime


@dataclass
class Fill:
    fill_id: str
    order_id_local: str
    venue: Venue
    price: Decimal
    size: Decimal
    fee: Decimal
    filled_at: datetime


@dataclass
class Position:
    canonical_id: str
    venue: Venue
    yes_qty: Decimal
    no_qty: Decimal
    avg_yes_cost: Decimal
    avg_no_cost: Decimal
```

### 5.2 SQLite Schema

```sql
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
```

---

## 6. Venue Adapter Design

### 6.1 Abstract Base

```python
class VenueAdapter(ABC):
    """Each venue adapter must implement these methods."""

    @abstractmethod
    async def connect(self) -> None: ...

    @abstractmethod
    async def disconnect(self) -> None: ...

    @abstractmethod
    async def fetch_active_markets(self) -> list[dict]: ...

    @abstractmethod
    async def get_book(self, native_market_id: str) -> CanonicalBook: ...

    @abstractmethod
    async def subscribe_books(self, market_ids: list[str], callback) -> None: ...

    @abstractmethod
    async def place_order(self, market_id: str, side: Side, price: Decimal, size: Decimal) -> str: ...

    @abstractmethod
    async def cancel_order(self, native_order_id: str) -> bool: ...

    @abstractmethod
    async def get_positions(self) -> list[Position]: ...

    @abstractmethod
    async def get_fee_rate(self, native_market_id: str) -> Decimal: ...
```

### 6.2 Polymarket Adapter — Key Details

**Market discovery:**
- `GET gamma-api.polymarket.com/markets?limit=100&offset=0&closed=false`
- Paginate with offset. Each market returns `condition_id`, `tokens` array (with `token_id` and `outcome` for YES/NO), `neg_risk`, `active`, `closed`.

**Book data (primary — WebSocket):**
- Connect to `wss://ws-subscriptions-clob.polymarket.com/ws/market`
- Subscribe: `{ "assets_ids": [yes_token_id, no_token_id], "type": "market" }`
- Receive `book` snapshots and `price_change` deltas
- Must send `PING` every 10s

**Book data (fallback — REST):**
- `GET clob.polymarket.com/book?token_id=YES_TOKEN_ID`
- Batch: `POST clob.polymarket.com/books` with up to 500 token IDs

**Price conversion:**
- Orderbook gives bids/asks for a specific token (YES or NO)
- `buy_yes` = lowest ask on the YES token book
- `buy_no` = lowest ask on the NO token book
- Or equivalently: `buy_no` = `1 - highest_bid` on the YES token (since YES bid at X ≈ NO ask at 1-X)
- **Best practice:** Fetch books for both YES and NO token IDs independently to get accurate prices

**Fee calculation:**
```python
def polymarket_fee(shares: Decimal, price: Decimal, fee_rate: Decimal, exponent: int) -> Decimal:
    if fee_rate == 0:
        return Decimal("0")
    raw = shares * price * fee_rate * (price * (1 - price)) ** exponent
    return raw.quantize(Decimal("0.0001"), rounding=ROUND_UP)
```

**Order execution:**
- Use `py-clob-client` SDK — handles EIP-712 signing, HMAC auth, feeRateBps inclusion
- For arb: use **FOK** (fill-or-kill) orders to get immediate certainty
- Must maintain trading heartbeat (POST every 10s) while orders are open

### 6.3 Kalshi Adapter — Key Details

**Market discovery:**
- `GET /markets?status=open&limit=200` — paginate with cursor
- Response includes `ticker`, `event_ticker`, `yes_bid_dollars`, `yes_ask_dollars`, `no_bid_dollars`, `no_ask_dollars`, `price_ranges`, `fractional_trading_enabled`

**Book data (phase 1 — REST polling):**
- `GET /markets/{ticker}/orderbook?depth=5`
- Returns `orderbook_fp.yes_dollars` and `orderbook_fp.no_dollars` — arrays of `[price_dollars, count_fp]`
- Sorted ascending; best bid is the **last** element
- **Important:** Kalshi only shows bids (not asks). YES bid at $X implies NO ask at $(1.00-X).

**Price conversion (Kalshi orderbook → CanonicalBook):**
```python
# Kalshi orderbook only has bids for YES and NO
# buy_yes = cheapest ask for YES = 1.00 - best_no_bid
# buy_no  = cheapest ask for NO  = 1.00 - best_yes_bid

best_yes_bid = Decimal(yes_dollars[-1][0])  # last element = best bid
best_no_bid  = Decimal(no_dollars[-1][0])

buy_yes = Decimal("1.00") - best_no_bid
buy_no  = Decimal("1.00") - best_yes_bid
```

**Book data (phase 2 — WebSocket):**
- Connect to `wss://api.elections.kalshi.com/trade-api/ws/v2` with auth headers
- Subscribe to `orderbook_delta` channel for specific tickers
- Receive `orderbook_snapshot` then incremental `orderbook_delta` messages
- Must respond to server pings with pong

**Order execution:**
- `POST /portfolio/orders` with `ticker`, `side` (yes/no), `action` (buy), `yes_price_dollars`, `count_fp`, `type` (limit), `time_in_force` (fill_or_kill), `client_order_id` (UUID for idempotency)
- Use `client_order_id` to prevent duplicate orders on retries

**Rate limit strategy (Basic tier: 20 read/s, 10 write/s):**
- Poll 20–30 tracked markets every 2s → ~15 req/s read (within limit)
- Batch candlestick/trade endpoints where possible
- Apply for Advanced tier (30/30) early

---

## 7. Contract Matching

### 7.1 Principles

Contract identity is the highest-risk component. A wrong match means you're long both sides of *different* events — pure loss.

**Rules:**
1. Auto-match only at very high confidence (all fields agree)
2. Everything else goes to a review queue
3. Manual overrides are permanent and take priority
4. Never allow implicit matching in production

### 7.2 Matching Pipeline

```
Polymarket markets ──┐
                     ├──→ Normalize ──→ Candidate Pairs ──→ Score ──→ Auto-match / Review Queue
Kalshi markets ──────┘
```

**Normalization steps:**
1. Lowercase, strip whitespace, remove punctuation
2. Extract structured fields: team names, dates, league, event type
3. Normalize dates to UTC ISO-8601
4. Normalize team/candidate names via alias table (e.g., "Duke Blue Devils" → "duke")
5. Identify contract type (binary yes/no, over/under, spread)

**Scoring criteria (all must match for auto-approval):**
- Subject key exact match (e.g., `duke_beats_tcu`)
- Category match (sports/politics/crypto)
- Cutoff date within 24h window
- Binary outcome semantics match
- Resolution source compatible (both "official result")
- Special conditions match (overtime, runoff, etc.)

**MVP approach:**
- Start with a hand-curated `manual_mappings.json` for 20–50 known pairs
- Build rule-based parsers for one domain (e.g., NCAAB moneyline)
- Do NOT use NLP/LLM matching in v1 — too unreliable for financial decisions

### 7.3 Manual Mappings Format

```json
{
  "mappings": [
    {
      "canonical_id": "ncaab_duke_tcu_20260321",
      "title": "Duke beats TCU - March 21, 2026",
      "subject_key": "duke_beats_tcu",
      "category": "sports",
      "cutoff_time_utc": "2026-03-21T23:59:59Z",
      "resolution_source": "official_ncaa_result",
      "venues": {
        "polymarket": {
          "condition_id": "0xabc...",
          "yes_token_id": "12345...",
          "no_token_id": "67890...",
          "neg_risk": false
        },
        "kalshi": {
          "ticker": "NCAAB-DUKE-TCU-21MAR26"
        }
      }
    }
  ]
}
```

---

## 8. Opportunity Detection

### 8.1 Logic

For each canonical contract with verified mappings on both venues:

```python
def detect(book_a: CanonicalBook, book_b: CanonicalBook, config: DetectorConfig) -> list[Opportunity]:
    if not (book_a.is_fresh(config.max_stale_ms) and book_b.is_fresh(config.max_stale_ms)):
        return []

    opportunities = []

    # Direction 1: buy YES on A, buy NO on B
    gross_1 = Decimal("1") - book_a.buy_yes - book_b.buy_no
    fee_1 = compute_fee(book_a, "yes") + compute_fee(book_b, "no")
    net_1 = gross_1 - fee_1 - config.slippage_buffer
    size_1 = min(book_a.depth_buy_yes, book_b.depth_buy_no, config.max_trade_size)

    if net_1 >= config.min_net_edge and size_1 >= config.min_trade_size:
        opportunities.append(make_opportunity(book_a, book_b, "yes_a_no_b", net_1, size_1))

    # Direction 2: buy NO on A, buy YES on B
    gross_2 = Decimal("1") - book_a.buy_no - book_b.buy_yes
    fee_2 = compute_fee(book_a, "no") + compute_fee(book_b, "yes")
    net_2 = gross_2 - fee_2 - config.slippage_buffer
    size_2 = min(book_a.depth_buy_no, book_b.depth_buy_yes, config.max_trade_size)

    if net_2 >= config.min_net_edge and size_2 >= config.min_trade_size:
        opportunities.append(make_opportunity(book_b, book_a, "no_a_yes_b", net_2, size_2))

    return opportunities
```

### 8.2 Filters Before Execution

An opportunity must pass ALL of these:

| Filter | Rationale |
|--------|-----------|
| `net_edge >= threshold` | Must exceed fees + slippage + safety margin |
| Both books fresh (< 2s old) | Stale books = phantom arb |
| Both legs have depth ≥ min size | Can't trade into empty books |
| Contract not within settlement blackout | Last 30 min before resolution is dangerous |
| No existing conflicting position | Don't double up on same contract |
| Rate limit not exceeded | Max N trades per minute |
| Signal persisted ≥ 2 consecutive snapshots | Avoid firing on transient blips |

### 8.3 Threshold Calibration

Start deliberately conservative:

```yaml
detector:
  min_net_edge: "0.03"        # 3 cents minimum after all costs
  slippage_buffer: "0.01"     # 1 cent slippage assumption
  max_stale_ms: 2000
  min_trade_size: "5"         # minimum 5 contracts
  max_trade_size: "50"        # cap at 50 contracts initially
  min_depth: "10"             # require 10 contracts visible
  persistence_snapshots: 2    # signal must appear twice
  settlement_blackout_min: 30 # no trades within 30 min of resolution
```

Do NOT fire on sub-cent edges. Wait for obviously real opportunities (3+ cents net) while you build confidence in book freshness and matching quality.

---

## 9. Execution Policy

### 9.1 Execution Flow

```
Opportunity detected
        │
        v
   Risk check passed?
        │ yes
        v
   Determine leg order:
   • fragile leg first (less liquid venue / wider spread)
        │
        v
   Submit leg 1 as FOK (fill-or-kill)
        │
        ├── FILLED → immediately submit leg 2 as FOK
        │       │
        │       ├── FILLED → log success, update positions
        │       └── REJECTED → residual exposure, queue unwind
        │
        ├── PARTIAL → do NOT submit leg 2, log residual
        │
        └── REJECTED → no action needed, log miss
```

### 9.2 Key Execution Rules

1. **FOK on both legs.** This eliminates partial fill complexity in v1. You either get the full size or nothing.
2. **Fragile leg first.** Usually Kalshi (lower rate limits, potentially thinner books). If Kalshi fills, Polymarket's deeper liquidity makes the hedge leg more reliable.
3. **Pre-funded on both venues.** No cross-venue capital movement during execution. Split capital 50/50 initially.
4. **Cap size to min of both books.** Never try to execute more than the smaller visible depth.
5. **Idempotent order IDs.** Use `client_order_id` (Kalshi) and track `orderID` (Polymarket) to prevent double-sends on retries.
6. **Emergency unwind.** If leg 1 fills but leg 2 fails, the bot has residual directional exposure. Options:
   - Place a resting limit order to unwind at breakeven on the same venue
   - Accept the small residual and let it resolve (if size is within risk limits)
   - Alert operator for manual intervention

### 9.3 Timing Budget

| Step | Target | Notes |
|------|--------|-------|
| Detect opportunity | 0ms | Triggered by book update |
| Risk check | <1ms | In-memory checks |
| Submit leg 1 | <200ms | REST round-trip |
| Submit leg 2 | <200ms | REST round-trip |
| **Total** | **<500ms** | End-to-end from detection to both legs submitted |

This is not HFT. The edges we're targeting (3+ cents) should persist for seconds, not milliseconds. Speed matters for reducing legging risk, but not at the sub-millisecond level.

---

## 10. Risk Controls

### 10.1 Hard Limits

```yaml
risk:
  max_notional_per_contract: "200"      # max $200 exposure per canonical contract
  max_notional_total: "2000"            # max $2000 total across all positions
  max_residual_per_contract: "50"       # max $50 unhedged after legging failure
  max_trades_per_minute: 5
  max_consecutive_failures: 3           # kill switch after 3 failed executions
  max_api_errors_per_minute: 10         # kill switch on API instability
  settlement_blackout_minutes: 30
  max_book_age_ms: 2000
```

### 10.2 Kill Switch Triggers

The bot should halt ALL trading and alert the operator if any of these occur:
- 3+ consecutive execution failures
- 10+ API errors in 1 minute from either venue
- Unhedged residual exceeds limit
- Book data stops updating for >10s
- Any unexpected exception in the execution path

### 10.3 Logging & Observability

Log every state transition for post-mortem analysis:

| Metric | Purpose |
|--------|---------|
| Time: detection → leg 1 submission | Measure execution latency |
| Time: leg 1 submission → fill confirmation | Measure venue latency |
| Time: leg 1 fill → leg 2 submission | Measure legging gap |
| Slippage: executed price vs. book price at detection | Book staleness indicator |
| Net edge realized vs. expected | Strategy quality |
| Fill rate (% of opportunities that fully execute) | Execution quality |
| Residual positions over time | Legging risk tracker |

---

## 11. Configuration

```yaml
# config.yaml

venues:
  polymarket:
    gamma_url: "https://gamma-api.polymarket.com"
    clob_url: "https://clob.polymarket.com"
    ws_url: "wss://ws-subscriptions-clob.polymarket.com/ws/market"
    ws_heartbeat_interval_s: 9
    trading_heartbeat_interval_s: 9
  kalshi:
    rest_url: "https://api.elections.kalshi.com/trade-api/v2"
    ws_url: "wss://api.elections.kalshi.com/trade-api/ws/v2"
    poll_interval_s: 2
    orderbook_depth: 5

detector:
  min_net_edge: "0.03"
  slippage_buffer: "0.01"
  max_stale_ms: 2000
  min_trade_size: "5"
  max_trade_size: "50"
  min_depth: "10"
  persistence_snapshots: 2
  settlement_blackout_min: 30

risk:
  max_notional_per_contract: "200"
  max_notional_total: "2000"
  max_residual_per_contract: "50"
  max_trades_per_minute: 5
  max_consecutive_failures: 3
  max_api_errors_per_minute: 10

execution:
  use_fok: true
  fragile_venue_first: true
  unwind_strategy: "limit_at_breakeven"

logging:
  level: "INFO"
  file: "logs/arb.log"
  log_all_books: false         # true = log every book update (verbose)
  log_all_opportunities: true  # log every detected opportunity even if not traded
```

---

## 12. Dependencies

```toml
[project]
name = "pbc-arb"
requires-python = ">=3.11"

[project.dependencies]
# Venue SDKs
py-clob-client = "*"          # Polymarket CLOB client (handles EIP-712 signing)

# HTTP / WebSocket
httpx = "*"                    # async HTTP client for Kalshi REST
websockets = "*"               # WebSocket connections

# Data
pyyaml = "*"                   # config loading
python-dotenv = "*"            # .env secrets

# Core
# (stdlib: decimal, sqlite3, dataclasses, asyncio, logging, uuid, json)
```

No heavy frameworks. Stdlib `asyncio` for the event loop, `sqlite3` for persistence, `Decimal` for all arithmetic, `logging` for observability.

---

## 13. Build Phases

### Phase 1: Read-Only Foundation (Week 1) — COMPLETE

**Goal:** Prove you can ingest and normalize market data from both venues.

| Task | Detail | Status |
|------|--------|--------|
| Project scaffolding | Rust crate with Cargo.toml, config, directory structure | DONE |
| Polymarket adapter (read) | CLOB REST book fetching + WebSocket real-time | DONE |
| Kalshi adapter (read) | REST orderbook polling with RSA auth | DONE |
| Book normalization | Both adapters produce `CanonicalBook` objects | DONE |
| Manual mappings | JSON file with 6 hand-verified cross-venue pairs | DONE |
| Contract registry | Load mappings, look up canonical_id by (venue, native_id) | DONE |
| SQLite setup | Schema creation, book logging | DONE |
| Discovery script | `bin/discover_pairs` — auto-discover cross-venue candidates | DONE |

**Exit criteria:** You can run a loop that prints normalized book snapshots for matched pairs from both venues every 2 seconds. **MET** — monitor runs live.

### Phase 2: Opportunity Detection (Week 2) — COMPLETE

**Goal:** Detect and log arbitrage opportunities without trading.

| Task | Detail | Status |
|------|--------|--------|
| Opportunity detector | Both arb directions, fees, slippage, freshness/depth filters | DONE |
| Polymarket WebSocket | WS push-based book updates with heartbeat | DONE |
| Opportunity logger | Every detected opportunity written to SQLite | DONE |
| Dashboard / CLI output | Real-time logging with net_edge, size, venues | DONE |
| Matching script | `bin/discover_pairs` — auto-discover new cross-venue candidates | DONE |

**Exit criteria:** The bot logs 24h of opportunity data. You can analyze frequency, edge distribution, and persistence of signals. **PENDING** — detector works, need to collect data.

### Phase 3: Replay & Validation (Week 2–3) — NOT STARTED

**Goal:** Validate that logged opportunities were real and tradeable.

| Task | Detail | Status |
|------|--------|--------|
| Replay evaluator | For each logged opportunity, check if it still existed 1s, 5s, 30s later | TODO |
| False positive analysis | Identify phantom arbs from stale books | TODO |
| Threshold tuning | Adjust `min_net_edge`, `slippage_buffer`, `max_stale_ms` based on data | TODO |
| Expand mappings | Add more verified pairs using `discover_pairs` tool | TODO |

**Blocker:** Need 24h+ of logged data from Phase 2 before this phase can begin.

**Exit criteria:** You have confidence numbers: X% of opportunities above threshold Y persisted for Z seconds. You know your false positive rate.

### Phase 4: Execution (Week 3–4) — PARTIAL

**Goal:** Trade real money in small size.

| Task | Detail | Status |
|------|--------|--------|
| Kalshi execution | Authenticated trading via REST with FOK orders | DONE |
| Risk manager | Hard limits, kill switch, position tracking | DONE |
| Execution engine | Fragile-leg-first sequencing, residual handling | DONE (Kalshi leg) |
| Polymarket execution | Authenticated trading (EIP-712 signing, HMAC auth) | TODO |
| Trade logging | Orders, fills, realized edge vs. expected | TODO |
| Kalshi sandbox testing | Test execution flow against `demo-api.kalshi.co` first | TODO |

**Exit criteria:** Bot executes 10+ real locked arbs in tiny size ($5–10 per leg) with positive realized edge.

### Phase 5: Hardening (Week 4+) — NOT STARTED

**Goal:** Make it production-ready.

| Task | Detail | Status |
|------|--------|--------|
| Kalshi WebSocket upgrade | Replace REST polling with authenticated WS | TODO |
| Error recovery | Reconnect logic for WS drops, API timeouts, order state reconciliation | TODO |
| Position reconciliation | Periodically verify local position state matches venue state | TODO |
| More mappings | Scale to 50+ pairs using discover_pairs tool | TODO |
| Monitoring | Alerts on kill switch triggers, daily PnL summary | TODO |
| Size scaling | Gradually increase `max_trade_size` as confidence grows | TODO |

---

## 14. Key Risks & Mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| **Wrong contract match** | Total loss on both legs | Manual verification required before trading any pair. Auto-match only with 100% field agreement. |
| **Stale book → phantom arb** | Execute at worse price than expected | Freshness filter (2s max), persistence filter (2+ snapshots), slippage buffer |
| **Legging risk** | Leg 1 fills, leg 2 doesn't → directional exposure | FOK on both legs. Size cap. Residual limits. Emergency unwind. |
| **API downtime** | Can't execute hedge leg | Kill switch on API errors. Pre-funded on both venues. Don't enter if either venue is unstable. |
| **Rate limiting (Kalshi Basic tier)** | Slow book updates, missed opportunities | Apply for Advanced tier. Batch requests. Prioritize tracked markets. |
| **Fee model changes** | Edge calculation wrong | Fetch fee rates per-request, don't hardcode. Re-validate fee assumptions weekly. |
| **Settlement edge cases** | Resolution dispute, void, early close | Blackout period before resolution. Monitor settlement status. Avoid contracts with unusual conditions. |
| **Polymarket heartbeat miss** | All open orders auto-cancelled | Dedicated heartbeat task with jitter. Alert on missed heartbeat. |
| **Regulatory** | Prediction market legality varies | This is a technical plan, not legal advice. Verify compliance in your jurisdiction. |

---

## 15. What NOT to Build

- **NLP/LLM contract matching** — Use deterministic rules + manual verification. An LLM that's 95% accurate will blow up 5% of your trades.
- **Full order book reconstruction** — Top-of-book is sufficient for v1. Depth beyond best level is nice-to-have.
- **Multi-venue (3+)** — Two venues is complex enough. Don't add a third until the two-venue system is profitable.
- **Directional "stale price" bets** — The initial plan mentions this as secondary. Keep it secondary. Locked arb first.
- **Streaming analytics / fancy dashboards** — SQLite + CLI output is enough. Build dashboards when you have a month of data.
- **Automated contract discovery via NLP** — Manual curation scales fine for 50–100 pairs. Automate when you need 500+.
