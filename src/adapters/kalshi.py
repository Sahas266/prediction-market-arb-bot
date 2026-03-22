from __future__ import annotations

import asyncio
import base64
import logging
import time
from decimal import Decimal
from datetime import datetime, timezone

import httpx
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import padding

from .base import VenueAdapter
from ..config import KalshiConfig
from ..models import CanonicalBook, Side, Venue
from ..utils import to_decimal

logger = logging.getLogger(__name__)

RATE_LIMIT_DELAY = 1.0  # seconds to wait after 429


class KalshiAdapter(VenueAdapter):
    def __init__(self, config: KalshiConfig) -> None:
        self.config = config
        self.base_url = config.rest_url
        self._client: httpx.AsyncClient | None = None
        self._private_key = None

        if config.rsa_private_key_b64:
            pem_body = config.rsa_private_key_b64.strip()
            if not pem_body.startswith("-----BEGIN"):
                pem_str = (
                    "-----BEGIN RSA PRIVATE KEY-----\n"
                    + pem_body
                    + "\n-----END RSA PRIVATE KEY-----"
                )
            else:
                pem_str = pem_body
            self._private_key = serialization.load_pem_private_key(
                pem_str.encode(), password=None
            )

    def _sign(self, timestamp_ms: int, method: str, path: str) -> str:
        message = f"{timestamp_ms}{method}{path}"
        signature = self._private_key.sign(
            message.encode(),
            padding.PSS(
                mgf=padding.MGF1(hashes.SHA256()),
                salt_length=hashes.SHA256().digest_size,
            ),
            hashes.SHA256(),
        )
        return base64.b64encode(signature).decode()

    def _auth_headers(self, method: str, path: str) -> dict[str, str]:
        ts = int(time.time() * 1000)
        sig = self._sign(ts, method, path)
        return {
            "KALSHI-ACCESS-KEY": self.config.api_key_id,
            "KALSHI-ACCESS-TIMESTAMP": str(ts),
            "KALSHI-ACCESS-SIGNATURE": sig,
        }

    async def connect(self) -> None:
        self._client = httpx.AsyncClient(timeout=10.0)
        logger.info("Kalshi adapter connected")

    async def disconnect(self) -> None:
        if self._client:
            await self._client.aclose()
            self._client = None

    async def _get(self, path: str, params: dict | None = None, auth: bool = False) -> dict:
        url = f"{self.base_url}{path}"
        headers = self._auth_headers("GET", path) if auth else {}
        for attempt in range(3):
            resp = await self._client.get(url, params=params, headers=headers)
            if resp.status_code == 429:
                wait = RATE_LIMIT_DELAY * (attempt + 1)
                logger.warning("Kalshi rate limited on GET %s, waiting %.1fs", path, wait)
                await asyncio.sleep(wait)
                continue
            resp.raise_for_status()
            return resp.json()
        resp.raise_for_status()
        return {}

    async def _post(self, path: str, json_body: dict) -> dict:
        url = f"{self.base_url}{path}"
        headers = self._auth_headers("POST", path)
        resp = await self._client.post(url, json=json_body, headers=headers)
        resp.raise_for_status()
        return resp.json()

    async def _delete(self, path: str) -> dict:
        url = f"{self.base_url}{path}"
        headers = self._auth_headers("DELETE", path)
        resp = await self._client.delete(url, headers=headers)
        resp.raise_for_status()
        if resp.status_code == 204:
            return {}
        return resp.json()

    # ── Market Discovery ──

    async def fetch_active_markets(self, limit: int = 200, max_pages: int = 50) -> list[dict]:
        all_markets = []
        cursor: str | None = None

        for _ in range(max_pages):
            params = {"limit": limit, "status": "open"}
            if cursor:
                params["cursor"] = cursor
            data = await self._get("/markets", params=params)
            markets = data.get("markets", [])
            all_markets.extend(markets)
            cursor = data.get("cursor")
            if not cursor or not markets:
                break
            # Respect rate limits (20 req/s basic tier)
            await asyncio.sleep(0.1)

        logger.info("Kalshi: fetched %d active markets", len(all_markets))
        return all_markets

    async def get_event(self, event_ticker: str) -> dict:
        return await self._get(f"/events/{event_ticker}")

    async def get_market(self, ticker: str) -> dict:
        data = await self._get(f"/markets/{ticker}")
        return data.get("market", data)

    # ── Orderbook ──

    async def get_orderbook(self, ticker: str, depth: int | None = None) -> dict:
        params = {}
        if depth is not None:
            params["depth"] = depth
        return await self._get(f"/markets/{ticker}/orderbook", params=params)

    async def get_book(self, native_market_id: str) -> CanonicalBook:
        ob = await self.get_orderbook(native_market_id, depth=self.config.orderbook_depth)
        now = datetime.now(timezone.utc)

        ob_fp = ob.get("orderbook_fp", ob.get("orderbook", {}))
        yes_levels = ob_fp.get("yes_dollars", ob_fp.get("yes", []))
        no_levels = ob_fp.get("no_dollars", ob_fp.get("no", []))

        # Kalshi orderbook shows bids only, sorted ascending. Best bid = last element.
        # buy_yes (ask for YES) = 1 - best_no_bid
        # buy_no  (ask for NO)  = 1 - best_yes_bid
        if yes_levels:
            best_yes_bid = to_decimal(yes_levels[-1][0])
            depth_yes_bid = to_decimal(yes_levels[-1][1])
        else:
            best_yes_bid = Decimal("0")
            depth_yes_bid = Decimal("0")

        if no_levels:
            best_no_bid = to_decimal(no_levels[-1][0])
            depth_no_bid = to_decimal(no_levels[-1][1])
        else:
            best_no_bid = Decimal("0")
            depth_no_bid = Decimal("0")

        buy_yes = Decimal("1") - best_no_bid if best_no_bid > 0 else Decimal("1")
        buy_no = Decimal("1") - best_yes_bid if best_yes_bid > 0 else Decimal("1")

        return CanonicalBook(
            venue=Venue.KALSHI,
            native_market_id=native_market_id,
            canonical_id="",  # filled in by registry
            buy_yes=buy_yes,
            buy_no=buy_no,
            depth_buy_yes=depth_no_bid,   # depth available at the ask = depth at opposing bid
            depth_buy_no=depth_yes_bid,
            fee_rate=Decimal("0.01"),  # conservative default ~1 cent per contract
            tick_size=Decimal("0.01"),
            min_order_size=Decimal("1"),
            ts_exchange=None,
            ts_received=now,
        )

    # ── Fee ──

    async def get_fee_rate(self, native_market_id: str) -> Decimal:
        # Kalshi doesn't have a simple fee-rate endpoint; fees are per-fill
        # Use a conservative estimate
        return Decimal("0.01")

    # ── Trading ──

    async def place_order(
        self,
        native_market_id: str,
        side: Side,
        price: Decimal,
        size: Decimal,
    ) -> str:
        body = {
            "ticker": native_market_id,
            "side": side.value,
            "action": "buy",
            "type": "limit",
            "yes_price_dollars": str(price) if side == Side.YES else None,
            "no_price_dollars": str(price) if side == Side.NO else None,
            "count_fp": str(size),
            "time_in_force": "fill_or_kill",
        }
        # Remove None values
        body = {k: v for k, v in body.items() if v is not None}
        data = await self._post("/portfolio/orders", body)
        order = data.get("order", data)
        order_id = order.get("order_id", "")
        logger.info("Kalshi order placed: %s side=%s price=%s size=%s → %s",
                     native_market_id, side.value, price, size, order_id)
        return order_id

    async def cancel_order(self, native_order_id: str) -> bool:
        try:
            await self._delete(f"/portfolio/orders/{native_order_id}")
            return True
        except httpx.HTTPStatusError:
            return False

    async def get_balance(self) -> dict:
        return await self._get("/portfolio/balance", auth=True)

    async def get_positions(self) -> list[dict]:
        data = await self._get("/portfolio/positions", auth=True)
        return data.get("market_positions", [])
