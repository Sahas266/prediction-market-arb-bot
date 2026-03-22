# Cross-Venue Prediction Market Arbitrage Monitor

A high-performance Rust system that detects locked arbitrage opportunities across Polymarket and Kalshi prediction markets in real-time.

## How It Works

Binary prediction markets price YES and NO outcomes that must resolve to exactly $1.00. When the same event is listed on two venues, pricing inefficiencies create **locked arbitrage**: buying YES on one venue and NO on the other guarantees a $1 payout regardless of outcome.

```
gross_edge = 1.00 - buy_yes_price_A - buy_no_price_B
net_edge   = gross_edge - fees - slippage_buffer
```

If `net_edge > 0`, the trade is risk-free profit.

## Architecture

```
Polymarket (WS + REST)  ─┐
                          ├─→ CanonicalBook ─→ OpportunityDetector ─→ RiskManager ─→ SQLite log
Kalshi (REST polling)   ─┘
```

- **Polymarket**: WebSocket push for real-time book updates, REST fallback
- **Kalshi**: REST orderbook polling every 2s with RSA-PSS-SHA256 auth
- **Detection**: Checks both arbitrage directions for every cross-venue pair
- **All arithmetic uses `rust_decimal`** — never floating point for prices

## Currently Monitored Markets

| Market | Category |
|--------|----------|
| US Recession by end of 2026 | Economics |
| Fed rate cut before 2027 | Economics |
| Democrats win Senate 2026 | Politics |
| Republicans win Senate 2026 | Politics |
| Democrats win House 2026 | Politics |
| Republicans win House 2026 | Politics |

## Setup

1. Clone and configure credentials:
```bash
cp .env.example .env
# Edit .env with your Kalshi RSA keys
```

2. Build and run:
```bash
cd rust_arb
cargo build --release
cargo run --release
```

3. Discover new market pairs:
```bash
cargo run --release --bin discover_pairs
# Review output in mappings/candidate_pairs.json
# Verify and move approved pairs into mappings/manual_mappings.json
```

## Testing

```bash
cd rust_arb
cargo test                     # All tests (requires network + .env)
cargo test test_detector       # Run a single test by name
```

Tests include live API integration tests against both Polymarket and Kalshi.

## Configuration

| File | Purpose |
|------|---------|
| `config.yaml` | Venue URLs, detector thresholds, risk limits |
| `.env` | Kalshi RSA keys, Polymarket wallet keys |
| `mappings/manual_mappings.json` | Verified cross-venue market pairs |

## Project Structure

```
PBC-Hackathon-2026/
├── rust_arb/src/
│   ├── main.rs              # Monitoring loop + orchestrator
│   ├── bin/discover_pairs.rs # Auto-discovery of cross-venue pairs
│   ├── adapters/
│   │   ├── kalshi.rs        # Kalshi REST + RSA auth
│   │   └── polymarket.rs    # Polymarket WS + CLOB REST
│   ├── config.rs            # YAML + .env config loading
│   ├── db.rs                # SQLite schema + logging
│   ├── detector.rs          # Arbitrage opportunity detection
│   ├── executor.rs          # Two-leg execution engine
│   ├── models.rs            # Core data types (Venue, CanonicalBook, Opportunity)
│   ├── registry.rs          # Contract registry + manual mappings
│   └── risk.rs              # Risk manager + kill switch
├── config.yaml
├── mappings/
│   └── manual_mappings.json # 6 verified cross-venue pairs
└── data/
    └── arb.db               # SQLite database (gitignored)
```
