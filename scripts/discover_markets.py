#!/usr/bin/env python3
"""Discover active markets on both venues and print summary.

Usage:
    python -m scripts.discover_markets [--kalshi-only] [--poly-only] [--search TERM]
"""
from __future__ import annotations

import argparse
import asyncio
import json
import sys
from pathlib import Path

# Add project root to path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from src.config import load_config
from src.adapters.kalshi import KalshiAdapter
from src.adapters.polymarket import PolymarketAdapter
from src.utils import setup_logging


async def discover(args: argparse.Namespace) -> None:
    setup_logging("INFO")
    config = load_config()

    if not args.poly_only:
        print("\n" + "=" * 80)
        print("KALSHI MARKETS")
        print("=" * 80)
        kalshi = KalshiAdapter(config.kalshi)
        await kalshi.connect()
        try:
            markets = await kalshi.fetch_active_markets()
            for m in markets:
                ticker = m.get("ticker", "")
                title = m.get("title", m.get("subtitle", ""))
                yes_bid = m.get("yes_bid_dollars", m.get("yes_bid", "?"))
                yes_ask = m.get("yes_ask_dollars", m.get("yes_ask", "?"))
                status = m.get("status", "?")

                if args.search and args.search.lower() not in (ticker + title).lower():
                    continue

                print(f"  {ticker:<40} bid={yes_bid} ask={yes_ask}  {title[:60]}")

            print(f"\nTotal: {len(markets)} active markets")
        finally:
            await kalshi.disconnect()

    if not args.kalshi_only:
        print("\n" + "=" * 80)
        print("POLYMARKET MARKETS")
        print("=" * 80)
        poly = PolymarketAdapter(config.polymarket)
        await poly.connect()
        try:
            raw_markets = await poly.fetch_active_markets(limit=100, max_pages=5)
            for raw in raw_markets:
                m = poly.parse_market(raw)
                question = m["question"]
                cid = m["condition_id"][:16]

                if args.search and args.search.lower() not in question.lower():
                    continue

                print(f"  {cid}...  {question[:70]}")
                print(f"    YES: {m['yes_token_id'][:20]}...  NO: {m['no_token_id'][:20]}...")

            print(f"\nTotal: {len(raw_markets)} active markets")
        finally:
            await poly.disconnect()


def main() -> None:
    parser = argparse.ArgumentParser(description="Discover prediction markets")
    parser.add_argument("--kalshi-only", action="store_true")
    parser.add_argument("--poly-only", action="store_true")
    parser.add_argument("--search", "-s", type=str, default="", help="Filter by text")
    args = parser.parse_args()
    asyncio.run(discover(args))


if __name__ == "__main__":
    main()
