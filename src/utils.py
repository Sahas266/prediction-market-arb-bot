from __future__ import annotations

import logging
import uuid
from datetime import datetime, timezone
from decimal import Decimal, ROUND_UP


def setup_logging(level: str = "INFO", log_file: str | None = None) -> None:
    handlers: list[logging.Handler] = [logging.StreamHandler()]
    if log_file:
        handlers.append(logging.FileHandler(log_file))
    logging.basicConfig(
        level=getattr(logging, level.upper(), logging.INFO),
        format="%(asctime)s %(name)s %(levelname)s %(message)s",
        handlers=handlers,
    )


def now_utc() -> datetime:
    return datetime.now(timezone.utc)


def new_id() -> str:
    return str(uuid.uuid4())


def to_decimal(value: str | int | float | Decimal) -> Decimal:
    if isinstance(value, Decimal):
        return value
    return Decimal(str(value))


def polymarket_fee(
    shares: Decimal,
    price: Decimal,
    fee_rate: Decimal,
    exponent: int = 1,
) -> Decimal:
    if fee_rate == 0:
        return Decimal("0")
    raw = shares * price * fee_rate * (price * (1 - price)) ** exponent
    return raw.quantize(Decimal("0.0001"), rounding=ROUND_UP)
