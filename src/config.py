from __future__ import annotations

import os
from dataclasses import dataclass
from decimal import Decimal
from pathlib import Path

import yaml
from dotenv import load_dotenv

PROJECT_ROOT = Path(__file__).resolve().parent.parent


def _parse_env_rsa_key(env_path: Path) -> tuple[str, str]:
    """Parse the .env file to extract KALSHI_RSA_PUBLIC_KEY and multiline KALSHI_RSA_PRIVATE_KEY."""
    api_key_id = ""
    private_key_lines: list[str] = []
    in_private_key = False

    if not env_path.exists():
        return "", ""

    for line in env_path.read_text().splitlines():
        stripped = line.strip()
        if stripped.startswith("KALSHI_RSA_PUBLIC_KEY"):
            api_key_id = stripped.split("=", 1)[1].strip()
        elif stripped.startswith("KALSHI_RSA_PRIVATE_KEY"):
            in_private_key = True
            after_eq = stripped.split("=", 1)[1].strip()
            if after_eq:
                private_key_lines.append(after_eq)
        elif in_private_key:
            if "=" in stripped and not stripped[0].isalpha():
                # Likely still part of base64 (ends with =)
                private_key_lines.append(stripped)
            elif stripped and not any(stripped.startswith(p) for p in ("KALSHI_", "POLYMARKET_", "#")):
                private_key_lines.append(stripped)
            else:
                in_private_key = False

    private_key_b64 = "\n".join(private_key_lines)
    return api_key_id, private_key_b64


def load_config(config_path: str | None = None) -> AppConfig:
    load_dotenv(PROJECT_ROOT / ".env")

    path = Path(config_path) if config_path else PROJECT_ROOT / "config.yaml"
    with open(path) as f:
        raw = yaml.safe_load(f)

    api_key_id, rsa_key = _parse_env_rsa_key(PROJECT_ROOT / ".env")
    # Fallback to env vars if direct parsing didn't work
    if not api_key_id:
        api_key_id = os.environ.get("KALSHI_API_KEY_ID", os.environ.get("KALSHI_RSA_PUBLIC_KEY", ""))
    if not rsa_key:
        rsa_key = os.environ.get("KALSHI_RSA_PRIVATE_KEY", "")

    return AppConfig(
        polymarket=PolymarketConfig(
            gamma_url=raw["venues"]["polymarket"]["gamma_url"],
            clob_url=raw["venues"]["polymarket"]["clob_url"],
            ws_url=raw["venues"]["polymarket"]["ws_url"],
            ws_heartbeat_interval_s=raw["venues"]["polymarket"]["ws_heartbeat_interval_s"],
        ),
        kalshi=KalshiConfig(
            rest_url=raw["venues"]["kalshi"]["rest_url"],
            ws_url=raw["venues"]["kalshi"]["ws_url"],
            poll_interval_s=raw["venues"]["kalshi"]["poll_interval_s"],
            orderbook_depth=raw["venues"]["kalshi"]["orderbook_depth"],
            api_key_id=api_key_id,
            rsa_private_key_b64=rsa_key,
        ),
        detector=DetectorConfig(
            min_net_edge=Decimal(raw["detector"]["min_net_edge"]),
            slippage_buffer=Decimal(raw["detector"]["slippage_buffer"]),
            max_stale_ms=raw["detector"]["max_stale_ms"],
            min_trade_size=Decimal(raw["detector"]["min_trade_size"]),
            max_trade_size=Decimal(raw["detector"]["max_trade_size"]),
            min_depth=Decimal(raw["detector"]["min_depth"]),
            persistence_snapshots=raw["detector"]["persistence_snapshots"],
            settlement_blackout_min=raw["detector"]["settlement_blackout_min"],
        ),
        risk=RiskConfig(
            max_notional_per_contract=Decimal(raw["risk"]["max_notional_per_contract"]),
            max_notional_total=Decimal(raw["risk"]["max_notional_total"]),
            max_residual_per_contract=Decimal(raw["risk"]["max_residual_per_contract"]),
            max_trades_per_minute=raw["risk"]["max_trades_per_minute"],
            max_consecutive_failures=raw["risk"]["max_consecutive_failures"],
            max_api_errors_per_minute=raw["risk"]["max_api_errors_per_minute"],
        ),
    )


@dataclass(frozen=True)
class PolymarketConfig:
    gamma_url: str
    clob_url: str
    ws_url: str
    ws_heartbeat_interval_s: int


@dataclass(frozen=True)
class KalshiConfig:
    rest_url: str
    ws_url: str
    poll_interval_s: int
    orderbook_depth: int
    api_key_id: str = ""
    rsa_private_key_b64: str = ""


@dataclass(frozen=True)
class DetectorConfig:
    min_net_edge: Decimal
    slippage_buffer: Decimal
    max_stale_ms: int
    min_trade_size: Decimal
    max_trade_size: Decimal
    min_depth: Decimal
    persistence_snapshots: int
    settlement_blackout_min: int


@dataclass(frozen=True)
class RiskConfig:
    max_notional_per_contract: Decimal
    max_notional_total: Decimal
    max_residual_per_contract: Decimal
    max_trades_per_minute: int
    max_consecutive_failures: int
    max_api_errors_per_minute: int


@dataclass(frozen=True)
class AppConfig:
    polymarket: PolymarketConfig
    kalshi: KalshiConfig
    detector: DetectorConfig
    risk: RiskConfig
