from __future__ import annotations

import logging
from decimal import Decimal
from datetime import datetime, timezone

from .models import CanonicalBook, Opportunity, Venue
from .config import DetectorConfig
from .utils import new_id

logger = logging.getLogger(__name__)


class OpportunityDetector:
    def __init__(self, config: DetectorConfig) -> None:
        self.config = config
        # Track persistence: canonical_id → count of consecutive snapshots with edge
        self._persistence: dict[str, int] = {}

    def detect_for_pair(
        self, book_a: CanonicalBook, book_b: CanonicalBook
    ) -> list[Opportunity]:
        if book_a.canonical_id != book_b.canonical_id:
            return []
        if not (book_a.is_fresh(self.config.max_stale_ms) and book_b.is_fresh(self.config.max_stale_ms)):
            return []

        now = datetime.now(timezone.utc)
        opps: list[Opportunity] = []

        # Direction 1: buy YES on A, buy NO on B
        gross_1 = Decimal("1") - book_a.buy_yes - book_b.buy_no
        fee_1 = book_a.fee_rate + book_b.fee_rate
        net_1 = gross_1 - fee_1 - self.config.slippage_buffer
        size_1 = min(book_a.depth_buy_yes, book_b.depth_buy_no, self.config.max_trade_size)

        if (
            net_1 >= self.config.min_net_edge
            and size_1 >= self.config.min_trade_size
            and book_a.depth_buy_yes >= self.config.min_depth
            and book_b.depth_buy_no >= self.config.min_depth
        ):
            opps.append(Opportunity(
                opportunity_id=new_id(),
                canonical_id=book_a.canonical_id,
                yes_venue=book_a.venue,
                no_venue=book_b.venue,
                buy_yes_price=book_a.buy_yes,
                buy_no_price=book_b.buy_no,
                gross_edge=gross_1,
                net_edge=net_1,
                max_size=size_1,
                detected_at=now,
                yes_book_age_ms=book_a.age_ms(),
                no_book_age_ms=book_b.age_ms(),
            ))

        # Direction 2: buy NO on A, buy YES on B
        gross_2 = Decimal("1") - book_a.buy_no - book_b.buy_yes
        fee_2 = book_a.fee_rate + book_b.fee_rate
        net_2 = gross_2 - fee_2 - self.config.slippage_buffer
        size_2 = min(book_a.depth_buy_no, book_b.depth_buy_yes, self.config.max_trade_size)

        if (
            net_2 >= self.config.min_net_edge
            and size_2 >= self.config.min_trade_size
            and book_a.depth_buy_no >= self.config.min_depth
            and book_b.depth_buy_yes >= self.config.min_depth
        ):
            opps.append(Opportunity(
                opportunity_id=new_id(),
                canonical_id=book_a.canonical_id,
                yes_venue=book_b.venue,
                no_venue=book_a.venue,
                buy_yes_price=book_b.buy_yes,
                buy_no_price=book_a.buy_no,
                gross_edge=gross_2,
                net_edge=net_2,
                max_size=size_2,
                detected_at=now,
                yes_book_age_ms=book_b.age_ms(),
                no_book_age_ms=book_a.age_ms(),
            ))

        return opps


def find_all_opportunities(
    books: list[CanonicalBook], detector: OpportunityDetector
) -> list[Opportunity]:
    grouped: dict[str, list[CanonicalBook]] = {}
    for b in books:
        grouped.setdefault(b.canonical_id, []).append(b)

    opps: list[Opportunity] = []
    for cid, contract_books in grouped.items():
        for i in range(len(contract_books)):
            for j in range(i + 1, len(contract_books)):
                opps.extend(detector.detect_for_pair(contract_books[i], contract_books[j]))

    opps.sort(key=lambda x: x.net_edge, reverse=True)
    return opps
