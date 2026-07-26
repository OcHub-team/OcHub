#!/usr/bin/env python3
"""Build the deterministic LiteLLM pricing snapshot shipped with OcHub."""

from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.request
from datetime import datetime, timezone
from decimal import Decimal
from pathlib import Path
from typing import Any


REPOSITORY = "BerriAI/litellm"
BRANCH = "main"
UPSTREAM_PATH = "model_prices_and_context_window.json"
SOURCE_URL = (
    f"https://raw.githubusercontent.com/{REPOSITORY}/{BRANCH}/{UPSTREAM_PATH}"
)
LICENSE_URL = f"https://github.com/{REPOSITORY}/blob/{BRANCH}/LICENSE"
COMMIT_API = f"https://api.github.com/repos/{REPOSITORY}/commits/{BRANCH}"
RAW_AT_REVISION = (
    f"https://raw.githubusercontent.com/{REPOSITORY}/{{revision}}/{UPSTREAM_PATH}"
)
PER_MILLION = Decimal(1_000_000)
SUPPORTED_MODES = {"chat", "completion"}
SPECIAL_PRICE_MARKERS = (
    "_above_",
    "_priority",
    "_flex",
    "_batches",
    "_batch",
    "_fast",
)
BASE_PRICE_FIELDS = (
    "input_cost_per_token",
    "output_cost_per_token",
    "cache_read_input_token_cost",
    "cache_creation_input_token_cost",
)


def request_json(url: str) -> Any:
    headers = {
        "Accept": "application/vnd.github+json",
        "User-Agent": "OcHub-pricing-snapshot",
    }
    token = os.environ.get("GITHUB_TOKEN", "").strip()
    if token:
        headers["Authorization"] = f"Bearer {token}"
    request = urllib.request.Request(url, headers=headers)
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.loads(response.read(), parse_float=Decimal)


def resolve_revision() -> str:
    payload = request_json(COMMIT_API)
    revision = payload.get("sha") if isinstance(payload, dict) else None
    if not isinstance(revision, str) or len(revision) != 40:
        raise ValueError("GitHub commit response did not contain a full SHA")
    return revision


def decimal_string(value: Any) -> str | None:
    if not isinstance(value, (int, Decimal)) or isinstance(value, bool):
        return None
    decimal = Decimal(value) * PER_MILLION
    if decimal < 0:
        return None
    text = format(decimal, "f")
    if "." in text:
        text = text.rstrip("0").rstrip(".")
    return text or "0"


def special_pricing_fields(model: dict[str, Any]) -> list[str]:
    fields = []
    for field in model:
        if not field.startswith(BASE_PRICE_FIELDS):
            continue
        if any(marker in field for marker in SPECIAL_PRICE_MARKERS):
            fields.append(field)
    return sorted(fields)


def build_snapshot(
    upstream: dict[str, Any],
    revision: str,
    generated_at: str,
) -> dict[str, Any]:
    entries: list[dict[str, Any]] = []
    for model_key, model in upstream.items():
        if model_key == "sample_spec" or not isinstance(model, dict):
            continue
        mode = model.get("mode")
        provider = model.get("litellm_provider")
        input_cost = decimal_string(model.get("input_cost_per_token"))
        output_cost = decimal_string(model.get("output_cost_per_token"))
        if (
            mode not in SUPPORTED_MODES
            or not isinstance(provider, str)
            or not provider.strip()
            or input_cost is None
            or output_cost is None
        ):
            continue

        entry: dict[str, Any] = {
            "model_key": model_key,
            "provider": provider,
            "mode": mode,
            "input_cost_per_million": input_cost,
            "output_cost_per_million": output_cost,
            "cache_read_cost_per_million": decimal_string(
                model.get("cache_read_input_token_cost")
            ),
            "cache_creation_cost_per_million": decimal_string(
                model.get("cache_creation_input_token_cost")
            ),
            "special_pricing_fields": special_pricing_fields(model),
        }
        source = model.get("source")
        if isinstance(source, str) and source.strip():
            entry["source_url"] = source
        entries.append(entry)

    entries.sort(key=lambda entry: entry["model_key"].casefold())
    if len(entries) < 1000:
        raise ValueError(
            f"refusing to write suspiciously small catalog ({len(entries)} entries)"
        )

    return {
        "schema_version": 1,
        "source": {
            "name": "LiteLLM",
            "url": SOURCE_URL,
            "revision": revision,
            "generated_at": generated_at,
            "license": "MIT",
            "license_url": LICENSE_URL,
        },
        "stats": {
            "upstream_entries": len(upstream) - int("sample_spec" in upstream),
            "eligible_entries": len(entries),
        },
        "entries": entries,
    }


def validate_snapshot(snapshot: Any) -> None:
    if not isinstance(snapshot, dict) or snapshot.get("schema_version") != 1:
        raise ValueError("unsupported pricing snapshot schema")
    source = snapshot.get("source")
    entries = snapshot.get("entries")
    if not isinstance(source, dict) or not isinstance(entries, list):
        raise ValueError("pricing snapshot is missing source or entries")
    revision = source.get("revision")
    if not isinstance(revision, str) or not revision:
        raise ValueError("pricing snapshot has no source revision")
    if len(entries) < 1000:
        raise ValueError("pricing snapshot has too few entries")
    seen: set[str] = set()
    for entry in entries:
        if not isinstance(entry, dict):
            raise ValueError("pricing snapshot entry is not an object")
        key = entry.get("model_key")
        if not isinstance(key, str) or not key.strip():
            raise ValueError("pricing snapshot entry has no model key")
        folded = key.casefold()
        if folded in seen:
            raise ValueError(f"duplicate model key: {key}")
        seen.add(folded)
        for field in ("input_cost_per_million", "output_cost_per_million"):
            value = entry.get(field)
            if not isinstance(value, str) or Decimal(value) < 0:
                raise ValueError(f"invalid {field} for {key}")


def load_json(path: Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8"), parse_float=Decimal)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--output",
        type=Path,
        default=Path("crates/app/assets/data/litellm-model-prices.json"),
    )
    parser.add_argument("--source-file", type=Path)
    parser.add_argument("--revision")
    parser.add_argument("--generated-at")
    parser.add_argument("--check", type=Path)
    parser.add_argument("--force", action="store_true")
    args = parser.parse_args()

    if args.check:
        validate_snapshot(load_json(args.check))
        print(f"valid LiteLLM pricing snapshot: {args.check}")
        return 0

    revision = args.revision or resolve_revision()
    if args.output.exists() and not args.force:
        current = load_json(args.output)
        validate_snapshot(current)
        if current["source"]["revision"] == revision:
            print(f"LiteLLM pricing snapshot already at {revision}")
            return 0

    if args.source_file:
        upstream = load_json(args.source_file)
    else:
        upstream = request_json(RAW_AT_REVISION.format(revision=revision))
    if not isinstance(upstream, dict):
        raise ValueError("LiteLLM pricing source is not a JSON object")

    generated_at = args.generated_at or datetime.now(timezone.utc).replace(
        microsecond=0
    ).isoformat().replace("+00:00", "Z")
    snapshot = build_snapshot(upstream, revision, generated_at)
    validate_snapshot(snapshot)

    args.output.parent.mkdir(parents=True, exist_ok=True)
    serialized = json.dumps(snapshot, ensure_ascii=False, indent=2) + "\n"
    args.output.write_text(serialized, encoding="utf-8")
    print(
        f"wrote {len(snapshot['entries'])} LiteLLM prices at "
        f"{revision} to {args.output}"
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"pricing snapshot update failed: {error}", file=sys.stderr)
        raise SystemExit(1)
