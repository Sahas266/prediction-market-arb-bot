from __future__ import annotations

from abc import ABC, abstractmethod
from decimal import Decimal
from typing import Callable, Awaitable

from ..models import CanonicalBook, Side, Position


class VenueAdapter(ABC):
    @abstractmethod
    async def connect(self) -> None: ...

    @abstractmethod
    async def disconnect(self) -> None: ...

    @abstractmethod
    async def fetch_active_markets(self) -> list[dict]: ...

    @abstractmethod
    async def get_book(self, native_market_id: str) -> CanonicalBook: ...

    @abstractmethod
    async def place_order(
        self, native_market_id: str, side: Side, price: Decimal, size: Decimal
    ) -> str: ...

    @abstractmethod
    async def cancel_order(self, native_order_id: str) -> bool: ...

    @abstractmethod
    async def get_fee_rate(self, native_market_id: str) -> Decimal: ...
