"""Main orchestrator — runs the arbitrage detection loop."""
from __future__ import annotations

import asyncio
import logging
import signal
import sys

from .config import load_config
from .db import init_db, get_connection, log_book, log_opportunity
from .models import CanonicalBook, Venue
from .adapters.kalshi import KalshiAdapter
from .adapters.polymarket import PolymarketAdapter
from .matcher.registry import ContractRegistry
from .detector import OpportunityDetector, find_all_opportunities
from .risk import RiskManager
from .executor import Executor
from .utils import setup_logging

logger = logging.getLogger(__name__)


async def run() -> None:
    config = load_config()
    setup_logging(level="INFO", log_file="logs/arb.log")
    init_db()

    # Load contract registry
    registry = ContractRegistry()
    n = registry.load_manual_mappings()
    if n == 0:
        logger.warning("No manual mappings loaded — run discover_markets.py first to find pairs")

    # Initialize adapters
    kalshi = KalshiAdapter(config.kalshi)
    polymarket = PolymarketAdapter(config.polymarket)
    await kalshi.connect()
    await polymarket.connect()

    adapters = {
        Venue.KALSHI.value: kalshi,
        Venue.POLYMARKET.value: polymarket,
    }

    # Initialize detector, risk, executor
    detector = OpportunityDetector(config.detector)
    risk = RiskManager(config.risk)
    executor = Executor(adapters)

    conn = get_connection()
    pairs = registry.get_paired_contracts()
    logger.info("Monitoring %d paired contracts", len(pairs))

    # Graceful shutdown
    stop = asyncio.Event()

    def handle_signal(*_):
        logger.info("Shutdown signal received")
        stop.set()

    for sig in (signal.SIGINT, signal.SIGTERM):
        try:
            asyncio.get_event_loop().add_signal_handler(sig, handle_signal)
        except NotImplementedError:
            signal.signal(sig, handle_signal)

    # Main loop
    cycle = 0
    while not stop.is_set() and not risk.is_killed:
        cycle += 1
        books: list[CanonicalBook] = []

        for cid, pm_mapping, km_mapping in pairs:
            try:
                # Fetch Kalshi book
                kb = await kalshi.get_book(km_mapping.native_market_id)
                kb.canonical_id = cid
                books.append(kb)

                # Fetch Polymarket book
                pb = await polymarket.get_book(
                    pm_mapping.native_market_id,
                    yes_token_id=pm_mapping.yes_token_id or "",
                    no_token_id=pm_mapping.no_token_id or "",
                )
                pb.canonical_id = cid
                books.append(pb)

                # Log books
                if config.detector.max_stale_ms:  # always log
                    for b in (kb, pb):
                        log_book(conn, b.venue.value, b.native_market_id, b.canonical_id,
                                 str(b.buy_yes), str(b.buy_no),
                                 str(b.depth_buy_yes), str(b.depth_buy_no),
                                 str(b.fee_rate), None, b.ts_received.isoformat())

            except Exception as e:
                logger.error("Error fetching books for %s: %s", cid, e)
                risk.record_api_error()

        # Detect opportunities
        opps = find_all_opportunities(books, detector)
        for opp in opps:
            logger.info(
                "OPPORTUNITY: %s | YES@%s(%s) NO@%s(%s) | gross=%.4f net=%.4f size=%s",
                opp.canonical_id,
                opp.yes_venue.value, opp.buy_yes_price,
                opp.no_venue.value, opp.buy_no_price,
                opp.gross_edge, opp.net_edge, opp.max_size,
            )
            log_opportunity(
                conn, opp.opportunity_id, opp.canonical_id,
                opp.yes_venue.value, opp.no_venue.value,
                str(opp.buy_yes_price), str(opp.buy_no_price),
                str(opp.gross_edge), str(opp.net_edge),
                str(opp.max_size), opp.detected_at.isoformat(),
            )

        if not opps and cycle % 30 == 0:
            logger.info("Cycle %d: no opportunities (monitoring %d pairs)", cycle, len(pairs))

        # Sleep before next poll
        try:
            await asyncio.wait_for(stop.wait(), timeout=config.kalshi.poll_interval_s)
        except asyncio.TimeoutError:
            pass

    # Cleanup
    await kalshi.disconnect()
    await polymarket.disconnect()
    conn.close()
    logger.info("Shutdown complete")


def main() -> None:
    asyncio.run(run())


if __name__ == "__main__":
    main()
