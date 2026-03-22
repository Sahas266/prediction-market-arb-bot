from __future__ import annotations

import re
import unicodedata
from datetime import datetime


def normalize_text(text: str) -> str:
    text = unicodedata.normalize("NFKD", text)
    text = text.lower().strip()
    text = re.sub(r"[^\w\s]", " ", text)
    text = re.sub(r"\s+", " ", text).strip()
    return text


def extract_subject_key(text: str) -> str:
    normalized = normalize_text(text)
    return normalized.replace(" ", "_")


TEAM_ALIASES: dict[str, str] = {
    "duke blue devils": "duke",
    "duke": "duke",
    "tcu horned frogs": "tcu",
    "tcu": "tcu",
    "north carolina tar heels": "unc",
    "unc": "unc",
    "kentucky wildcats": "kentucky",
    "kentucky": "kentucky",
}


def normalize_team(name: str) -> str:
    key = normalize_text(name)
    return TEAM_ALIASES.get(key, key)


def parse_date_from_text(text: str) -> datetime | None:
    patterns = [
        r"(\d{4}-\d{2}-\d{2})",
        r"(\d{1,2}/\d{1,2}/\d{4})",
        r"(\w+ \d{1,2},?\s*\d{4})",
    ]
    for pat in patterns:
        m = re.search(pat, text)
        if m:
            raw = m.group(1)
            for fmt in ("%Y-%m-%d", "%m/%d/%Y", "%B %d, %Y", "%B %d %Y"):
                try:
                    return datetime.strptime(raw, fmt)
                except ValueError:
                    continue
    return None
