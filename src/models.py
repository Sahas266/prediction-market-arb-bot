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
    canonical_id: str
    title: str
    subject_key: str
    resolution_source: str
    cutoff_time_utc: datetime
    category: str


@dataclass(frozen=True)
class VenueMapping:
    canonical_id: str
    venue: Venue
    native_market_id: str
    yes_token_id: Optional[str] = None
    no_token_id: Optional[str] = None
    neg_risk: bool = False
    confidence: Decimal = Decimal("1.0")
    method: str = "manual"
    is_verified: bool = True


@dataclass
class CanonicalBook:
    venue: Venue
    native_market_id: str
    canonical_id: str
    buy_yes: Decimal
    buy_no: Decimal
    depth_buy_yes: Decimal
    depth_buy_no: Decimal
    fee_rate: Decimal
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
    action: str
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
    yes_qty: Decimal = Decimal("0")
    no_qty: Decimal = Decimal("0")
    avg_yes_cost: Decimal = Decimal("0")
    avg_no_cost: Decimal = Decimal("0")
