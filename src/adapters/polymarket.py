from __future__ import annotations

import json
import logging
from decimal import Decimal, ROUND_UP
from datetime import datetime, timezone

import httpx

from .base import VenueAdapter
from ..config import PolymarketConfig
from ..models import CanonicalBook, Side, Venue
from ..utils import to_decimal

logger = logging.getLogger(__name__)


class PolymarketAdapter(VenueAdapter):
    def __init__(self, config: PolymarketConfig) -> None:
        self.config = config
        self._client: httpx.AsyncClient | None = None
        self._fee_cache: dict[str, Decimal] = {}

    async def connect(self) -> None:
        self._client = httpx.AsyncClient(timeout=10.0)
        logger.info("Polymarket adapter connected")

    async def disconnect(self) -> None:
        if self._client:
            await self._client.aclose()
            self._client = None

    # ── Market Discovery (Gamma API) ──

    async def fetch_active_markets(self, limit: int = 100, max_pages: int = 20) -> list[dict]:
        all_markets = []
        offset = 0

        for _ in range(max_pages):
            url = f"{self.config.gamma_url}/markets"
            params = {"limit": limit, "offset": offset, "closed": "false", "active": "true"}
            resp = await self._client.get(url, params=params)
            resp.raise_for_status()
            markets = resp.json()
            if not markets:
                break
            all_markets.extend(markets)
            offset += limit

        logger.info("Polymarket: fetched %d active markets", len(all_markets))
        return all_markets

    async def get_market(self, condition_id: str) -> dict:
        url = f"{self.config.gamma_url}/markets/{condition_id}"
        resp = await self._client.get(url)
        resp.raise_for_status()
        return resp.json()

    @staticmethod
    def parse_market(raw: dict) -> dict:
        """Normalize a Gamma API market response into a standard dict."""
        clob_tokens_raw = raw.get("clobTokenIds", "[]")
        if isinstance(clob_tokens_raw, str):
            clob_tokens = json.loads(clob_tokens_raw)
        else:
            clob_tokens = clob_tokens_raw

        outcomes_raw = raw.get("outcomes", "[]")
        if isinstance(outcomes_raw, str):
            outcomes = json.loads(outcomes_raw)
        else:
            outcomes = outcomes_raw

        yes_token = clob_tokens[0] if len(clob_tokens) > 0 else ""
        no_token = clob_tokens[1] if len(clob_tokens) > 1 else ""

        # Map outcomes to tokens — default is [Yes, No] → [token0, token1]
        if len(outcomes) >= 2 and len(clob_tokens) >= 2:
            for i, outcome in enumerate(outcomes):
                if outcome.lower() == "yes":
                    yes_token = clob_tokens[i]
                elif outcome.lower() == "no":
                    no_token = clob_tokens[i]

        return {
            "condition_id": raw.get("conditionId", ""),
            "question": raw.get("question", ""),
            "slug": raw.get("slug", ""),
            "end_date": raw.get("endDate", ""),
            "active": raw.get("active", False),
            "closed": raw.get("closed", False),
            "neg_risk": raw.get("negRisk", False),
            "yes_token_id": yes_token,
            "no_token_id": no_token,
            "outcomes": outcomes,
            "outcome_prices": raw.get("outcomePrices", ""),
            "volume": raw.get("volumeClob", raw.get("volume", "0")),
            "liquidity": raw.get("liquidityClob", raw.get("liquidity", "0")),
            "tick_size": raw.get("orderPriceMinTickSize", "0.01"),
            "min_order_size": raw.get("orderMinSize", "1"),
            "fees_enabled": raw.get("feesEnabled", False),
        }

    # ── Orderbook (CLOB API, no auth) ──

    async def get_clob_book(self, token_id: str) -> dict:
        url = f"{self.config.clob_url}/book"
        resp = await self._client.get(url, params={"token_id": token_id})
        resp.raise_for_status()
        return resp.json()

    async def get_book(self, native_market_id: str, yes_token_id: str = "", no_token_id: str = "") -> CanonicalBook:
        """Fetch books for both YES and NO tokens and build a CanonicalBook.

        native_market_id is the condition_id. yes_token_id and no_token_id
        must be provided (from the venue mapping).
        """
        now = datetime.now(timezone.utc)

        buy_yes = Decimal("1")
        depth_buy_yes = Decimal("0")
        buy_no = Decimal("1")
        depth_buy_no = Decimal("0")
        tick_size = Decimal("0.01")
        min_order_size = Decimal("1")

        if yes_token_id:
            yes_book = await self.get_clob_book(yes_token_id)
            asks = yes_book.get("asks", [])
            if asks:
                # asks sorted ascending by price — first is best (lowest) ask
                best_ask = asks[0]
                buy_yes = to_decimal(best_ask["price"])
                depth_buy_yes = to_decimal(best_ask["size"])
            tick_size = to_decimal(yes_book.get("tick_size", "0.01"))
            min_order_size = to_decimal(yes_book.get("min_order_size", "1"))

        if no_token_id:
            no_book = await self.get_clob_book(no_token_id)
            asks = no_book.get("asks", [])
            if asks:
                best_ask = asks[0]
                buy_no = to_decimal(best_ask["price"])
                depth_buy_no = to_decimal(best_ask["size"])

        fee_rate = await self.get_fee_rate(yes_token_id or no_token_id)

        return CanonicalBook(
            venue=Venue.POLYMARKET,
            native_market_id=native_market_id,
            canonical_id="",  # filled in by registry
            buy_yes=buy_yes,
            buy_no=buy_no,
            depth_buy_yes=depth_buy_yes,
            depth_buy_no=depth_buy_no,
            fee_rate=fee_rate,
            tick_size=tick_size,
            min_order_size=min_order_size,
            ts_exchange=None,
            ts_received=now,
        )

    # ── Fee ──

    async def get_fee_rate(self, token_id: str) -> Decimal:
        if not token_id:
            return Decimal("0")
        if token_id in self._fee_cache:
            return self._fee_cache[token_id]

        url = f"{self.config.clob_url}/fee-rate"
        try:
            resp = await self._client.get(url, params={"token_id": token_id})
            resp.raise_for_status()
            data = resp.json()
            # base_fee is in bps, convert to decimal rate
            bps = to_decimal(data.get("base_fee", "0"))
            rate = bps / Decimal("10000")
        except Exception:
            rate = Decimal("0")

        self._fee_cache[token_id] = rate
        return rate

    # ── Trading (requires auth — placeholder for Phase 4) ──

    async def place_order(
        self,
        native_market_id: str,
        side: Side,
        price: Decimal,
        size: Decimal,
    ) -> str:
        raise NotImplementedError("Polymarket trading requires wallet auth — not yet implemented")

    async def cancel_order(self, native_order_id: str) -> bool:
        raise NotImplementedError("Polymarket trading not yet implemented")
