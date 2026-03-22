from __future__ import annotations

import logging
import time
from collections import deque
from decimal import Decimal

from .config import RiskConfig
from .models import Opportunity, Position

logger = logging.getLogger(__name__)


class RiskManager:
    def __init__(self, config: RiskConfig) -> None:
        self.config = config
        self._trade_timestamps: deque[float] = deque()
        self._consecutive_failures: int = 0
        self._api_error_timestamps: deque[float] = deque()
        self._killed: bool = False
        self._positions: dict[str, dict[str, Decimal]] = {}  # canonical_id → {venue: notional}

    @property
    def is_killed(self) -> bool:
        return self._killed

    def kill(self, reason: str) -> None:
        self._killed = True
        logger.critical("KILL SWITCH activated: %s", reason)

    def record_trade(self) -> None:
        self._trade_timestamps.append(time.time())
        self._consecutive_failures = 0

    def record_failure(self) -> None:
        self._consecutive_failures += 1
        if self._consecutive_failures >= self.config.max_consecutive_failures:
            self.kill(f"{self._consecutive_failures} consecutive execution failures")

    def record_api_error(self) -> None:
        now = time.time()
        self._api_error_timestamps.append(now)
        cutoff = now - 60
        while self._api_error_timestamps and self._api_error_timestamps[0] < cutoff:
            self._api_error_timestamps.popleft()
        if len(self._api_error_timestamps) >= self.config.max_api_errors_per_minute:
            self.kill(f"{len(self._api_error_timestamps)} API errors in 1 minute")

    def _trades_per_minute(self) -> int:
        now = time.time()
        cutoff = now - 60
        while self._trade_timestamps and self._trade_timestamps[0] < cutoff:
            self._trade_timestamps.popleft()
        return len(self._trade_timestamps)

    def check_opportunity(self, opp: Opportunity) -> tuple[bool, str]:
        if self._killed:
            return False, "kill switch active"

        if self._trades_per_minute() >= self.config.max_trades_per_minute:
            return False, "rate limit: too many trades per minute"

        # Check per-contract notional
        existing = self._positions.get(opp.canonical_id, {})
        total_existing = sum(existing.values())
        new_notional = opp.max_size * opp.buy_yes_price + opp.max_size * opp.buy_no_price
        if total_existing + new_notional > self.config.max_notional_per_contract:
            return False, f"per-contract limit exceeded ({total_existing + new_notional})"

        # Check total notional
        grand_total = sum(sum(v.values()) for v in self._positions.values())
        if grand_total + new_notional > self.config.max_notional_total:
            return False, f"total notional limit exceeded ({grand_total + new_notional})"

        return True, "approved"

    def approved_size(self, opp: Opportunity) -> Decimal:
        return min(opp.max_size, self.config.max_notional_per_contract)
