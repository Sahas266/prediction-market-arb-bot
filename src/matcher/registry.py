from __future__ import annotations

import json
import logging
from decimal import Decimal
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

from ..models import CanonicalContract, VenueMapping, Venue
from ..config import PROJECT_ROOT

logger = logging.getLogger(__name__)


class ContractRegistry:
    def __init__(self) -> None:
        self.contracts: dict[str, CanonicalContract] = {}
        self.mappings: dict[tuple[str, Venue], VenueMapping] = {}
        # Reverse lookup: (venue, native_market_id) → canonical_id
        self._reverse: dict[tuple[Venue, str], str] = {}

    def add_contract(self, contract: CanonicalContract) -> None:
        self.contracts[contract.canonical_id] = contract

    def add_mapping(self, mapping: VenueMapping) -> None:
        key = (mapping.canonical_id, mapping.venue)
        self.mappings[key] = mapping
        self._reverse[(mapping.venue, mapping.native_market_id)] = mapping.canonical_id

    def get_canonical_id(self, venue: Venue, native_market_id: str) -> Optional[str]:
        return self._reverse.get((venue, native_market_id))

    def get_mapping(self, canonical_id: str, venue: Venue) -> Optional[VenueMapping]:
        return self.mappings.get((canonical_id, venue))

    def get_paired_contracts(self) -> list[tuple[str, VenueMapping, VenueMapping]]:
        """Return all canonical IDs that have verified mappings on both venues."""
        pairs = []
        for cid in self.contracts:
            pm = self.mappings.get((cid, Venue.POLYMARKET))
            km = self.mappings.get((cid, Venue.KALSHI))
            if pm and km and pm.is_verified and km.is_verified:
                pairs.append((cid, pm, km))
        return pairs

    def load_manual_mappings(self, path: str | Path | None = None) -> int:
        if path is None:
            path = PROJECT_ROOT / "mappings" / "manual_mappings.json"
        path = Path(path)
        if not path.exists():
            logger.warning("Manual mappings file not found: %s", path)
            return 0

        with open(path) as f:
            data = json.load(f)

        count = 0
        for entry in data.get("mappings", []):
            cid = entry["canonical_id"]
            cutoff = datetime.fromisoformat(entry["cutoff_time_utc"].replace("Z", "+00:00"))

            contract = CanonicalContract(
                canonical_id=cid,
                title=entry.get("title", ""),
                subject_key=entry.get("subject_key", ""),
                resolution_source=entry.get("resolution_source", ""),
                cutoff_time_utc=cutoff,
                category=entry.get("category", ""),
            )
            self.add_contract(contract)

            venues = entry.get("venues", {})
            if "polymarket" in venues:
                pm = venues["polymarket"]
                self.add_mapping(VenueMapping(
                    canonical_id=cid,
                    venue=Venue.POLYMARKET,
                    native_market_id=pm["condition_id"],
                    yes_token_id=pm.get("yes_token_id"),
                    no_token_id=pm.get("no_token_id"),
                    neg_risk=pm.get("neg_risk", False),
                    confidence=Decimal("1.0"),
                    method="manual",
                    is_verified=True,
                ))
            if "kalshi" in venues:
                km = venues["kalshi"]
                self.add_mapping(VenueMapping(
                    canonical_id=cid,
                    venue=Venue.KALSHI,
                    native_market_id=km["ticker"],
                    confidence=Decimal("1.0"),
                    method="manual",
                    is_verified=True,
                ))

            count += 1

        logger.info("Loaded %d manual mappings", count)
        return count
