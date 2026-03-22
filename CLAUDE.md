# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build, Test, Run

All commands run from `rust_arb/`:

```bash
cargo build --release          # Build optimized binary
cargo test                     # Run all tests (includes live API integration tests)
cargo test test_detector       # Run a single test by name substring
cargo run --release            # Run the monitoring loop
```

The binary can also be run directly: `./rust_arb/target/release/arb_monitor.exe`

Tests hit live Kalshi and Polymarket APIs — they require network access and a valid `.env` with Kalshi credentials.

## Architecture

This is a **cross-venue prediction market arbitrage monitor**. It exploits the locked arbitrage identity: buying YES on one venue + NO on another for the same event guarantees a $1 payout if the combined cost is < $1.

### Data Flow

```
Registry (manual_mappings.json)
    → loads canonical_id ↔ venue market pairs
    → each pair has Polymarket token_ids + Kalshi ticker

Main Loop (every 2s):
    For each canonical pair:
        KalshiAdapter.get_book()       → REST poll orderbook
        PolymarketAdapter.get_book()   → WS cache or REST fallback
    → CanonicalBook (unified format)
    → OpportunityDetector.detect_for_pair()
        gross_edge = 1 - buy_yes_A - buy_no_B
        net_edge = gross_edge - fees - slippage_buffer
    → RiskManager.check_opportunity()
    → log to SQLite
```

### Key Design Decisions

- **Decimal arithmetic everywhere** (`rust_decimal`) — never use `f64` for prices, sizes, or edges.
- **Kalshi orderbook inversion**: Kalshi shows bid-side only. `buy_yes = 1 - best_no_bid`, `buy_no = 1 - best_yes_bid`.
- **Polymarket neg_risk markets** (Senate/House control) use a different exchange contract structure. The `neg_risk` and `neg_risk_market_id` fields in mappings track this.
- **Fragile venue first** execution: Kalshi (less liquid) leg executes before Polymarket to minimize residual exposure.
- **WebSocket for Polymarket** book data (push-based, cached in `Arc<RwLock<HashMap>>`), REST polling for Kalshi.

## Configuration

- `config.yaml` (repo root) — venue URLs, detector thresholds, risk limits
- `.env` (repo root) — secrets: `KALSHI_RSA_PUBLIC_KEY`, `KALSHI_RSA_PRIVATE_KEY`, `POLYGON_PRIVATE_KEY`
- `mappings/manual_mappings.json` — 6 verified cross-venue pairs with Polymarket condition_ids/token_ids and Kalshi tickers

The `config::project_root()` function walks up from CWD to find `config.yaml`, so the binary works whether run from `rust_arb/` or the repo root.

## Adding New Market Pairs

Add entries to `mappings/manual_mappings.json`. Each entry needs:
- Polymarket: `condition_id`, `yes_token_id`, `no_token_id`, `neg_risk` (from Gamma API)
- Kalshi: `ticker` (from Kalshi markets API)
- Both markets must have matching resolution criteria and timeframes.

Polymarket Gamma API: `GET https://gamma-api.polymarket.com/markets/{slug}`
Kalshi API: `GET https://api.elections.kalshi.com/trade-api/v2/markets?series_ticker=TICKER`

## Database

SQLite at `data/arb.db` with WAL mode. Key tables: `books_log` (every orderbook snapshot), `opportunities` (detected arbs), `orders`, `fills`, `positions`.
