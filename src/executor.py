from __future__ import annotations

import logging
from decimal import Decimal

from .adapters.base import VenueAdapter
from .models import Opportunity, Side, Venue

logger = logging.getLogger(__name__)


class Executor:
    def __init__(self, adapters: dict[str, VenueAdapter]) -> None:
        self.adapters = adapters

    async def execute_locked_arb(
        self,
        opp: Opportunity,
        venue_market_map: dict[tuple[str, str], str],
        size: Decimal,
        fragile_venue_first: bool = True,
    ) -> tuple[str | None, str | None]:
        yes_venue = opp.yes_venue.value
        no_venue = opp.no_venue.value
        yes_market = venue_market_map.get((yes_venue, opp.canonical_id), "")
        no_market = venue_market_map.get((no_venue, opp.canonical_id), "")

        if not yes_market or not no_market:
            logger.error("Missing market mapping for %s", opp.canonical_id)
            return None, None

        # Determine leg order: fragile (less liquid) first
        if fragile_venue_first and opp.yes_venue == Venue.KALSHI:
            first = ("yes", yes_venue, yes_market, opp.buy_yes_price, Side.YES)
            second = ("no", no_venue, no_market, opp.buy_no_price, Side.NO)
        elif fragile_venue_first and opp.no_venue == Venue.KALSHI:
            first = ("no", no_venue, no_market, opp.buy_no_price, Side.NO)
            second = ("yes", yes_venue, yes_market, opp.buy_yes_price, Side.YES)
        else:
            first = ("yes", yes_venue, yes_market, opp.buy_yes_price, Side.YES)
            second = ("no", no_venue, no_market, opp.buy_no_price, Side.NO)

        # Leg 1
        logger.info("Leg 1 (%s): %s %s at %s size %s",
                     first[0], first[1], first[2], first[3], size)
        try:
            first_order_id = await self.adapters[first[1]].place_order(
                first[2], first[4], first[3], size
            )
        except Exception as e:
            logger.error("Leg 1 failed: %s", e)
            return None, None

        if not first_order_id:
            logger.warning("Leg 1 returned no order ID — aborting")
            return None, None

        # Leg 2
        logger.info("Leg 2 (%s): %s %s at %s size %s",
                     second[0], second[1], second[2], second[3], size)
        try:
            second_order_id = await self.adapters[second[1]].place_order(
                second[2], second[4], second[3], size
            )
        except Exception as e:
            logger.error("Leg 2 failed after leg 1 filled: %s — RESIDUAL EXPOSURE", e)
            return first_order_id, None

        logger.info("Both legs executed: %s, %s | net_edge=%s",
                     first_order_id, second_order_id, opp.net_edge)
        return first_order_id, second_order_id
