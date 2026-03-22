#!/usr/bin/env python3
"""Find potential cross-venue pairs by comparing market titles.

Usage:
    python -m scripts.match_candidates [--threshold 0.6]
"""
from __future__ import annotations

import argparse
import asyncio
import sys
from difflib import SequenceMatcher
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from src.config import load_config
from src.adapters.kalshi import KalshiAdapter
from src.adapters.polymarket import PolymarketAdapter
from src.matcher.normalizer import normalize_text
from src.utils import setup_logging


def similarity(a: str, b: str) -> float:
    return SequenceMatcher(None, normalize_text(a), normalize_text(b)).ratio()


async def find_candidates(threshold: float = 0.6) -> None:
    setup_logging("INFO")
    config = load_config()

    kalshi = KalshiAdapter(config.kalshi)
    poly = PolymarketAdapter(config.polymarket)
    await kalshi.connect()
    await poly.connect()

    try:
        kalshi_markets = await kalshi.fetch_active_markets()
        poly_raw = await poly.fetch_active_markets(limit=100, max_pages=5)
        poly_markets = [PolymarketAdapter.parse_market(r) for r in poly_raw]

        print(f"\nComparing {len(kalshi_markets)} Kalshi × {len(poly_markets)} Polymarket markets...")
        print("=" * 100)

        candidates = []
        for km in kalshi_markets:
            k_title = km.get("title", "") + " " + km.get("subtitle", "")
            k_ticker = km.get("ticker", "")
            for pm in poly_markets:
                p_title = pm.get("question", "")
                p_cid = pm.get("condition_id", "")
                score = similarity(k_title, p_title)
                if score >= threshold:
                    candidates.append((score, k_ticker, k_title.strip(), p_cid[:20], p_title, pm))

        candidates.sort(key=lambda x: x[0], reverse=True)

        for score, k_tick, k_title, p_cid, p_title, pm in candidates[:50]:
            print(f"\n  Score: {score:.2f}")
            print(f"  Kalshi:      {k_tick} — {k_title[:80]}")
            print(f"  Polymarket:  {p_cid}... — {p_title[:80]}")
            print(f"    YES token: {pm['yes_token_id'][:30]}...")
            print(f"    NO token:  {pm['no_token_id'][:30]}...")

        print(f"\n\nFound {len(candidates)} candidates above {threshold} threshold")

    finally:
        await kalshi.disconnect()
        await poly.disconnect()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--threshold", type=float, default=0.6)
    args = parser.parse_args()
    asyncio.run(find_candidates(args.threshold))


if __name__ == "__main__":
    main()
