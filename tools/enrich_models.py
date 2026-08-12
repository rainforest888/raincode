#!/usr/bin/env python3
"""Enrich raincode's model capability-score library from real web benchmark data.

Fetches OpenRouter's model catalogue (real Artificial-Analysis indices + Design-Arena
Elo + live pricing + context window), rebuilds every row of `~/.raincode/state.db`
`model_profiles`, and applies curated overrides for models that lack public arena
data (frontend/backend), e.g. qwen3.8-max (family proxy from qwen3.7-max).

Fixes two known data-quality issues in the library:
  1. Pricing units: OpenRouter reports $/token; we store $/1M tokens (x1e6), so
     cost_efficiency in dispatch() is meaningful instead of everything == 0.0001.
  2. Missing design-arena (frontend/backend) rows get a curated family-proxy score.

Usage:  python tools/enrich_models.py
        python tools/enrich_models.py --db <path-to-state.db>
"""
import argparse
import json
import os
import sqlite3
import sys
import urllib.request

OPENROUTER_URL = "https://openrouter.ai/api/v1/models"
DEFAULT_DB = os.path.join(os.path.expanduser("~"), ".raincode", "state.db")

# Curated overrides for models with no public design-arena data.
# Values are 0-100 capability scores; `source` replaces the provenance marker.
# qwen3.8-max: qwen3.7-max has website=1295(code=73.75) / codecategories=1303(75.75)
# on the arena; qwen3.8-max is the newer flagship (intelligence 58.1 vs 46.7, coding
# 71.8 vs 66.0), so frontend/backend take a small strength bump above the family.
CURATED_OVERRIDES = {
    "qwen/qwen3.8-max": {
        "frontend": 76.0,
        "backend": 78.0,
        "source": "curated-family-proxy",
    },
}


def normalize_arena_elo(elo: float) -> float:
    """LMArena Elo (~1000-1400) -> 0-100 (1000 ~ 0 pts, 1400 ~ 100 pts)."""
    return max(0.0, min(100.0, (elo - 1000.0) / 400.0 * 100.0))


def fetch_models():
    req = urllib.request.Request(OPENROUTER_URL, headers={"User-Agent": "raincode-enrich/0.1"})
    with urllib.request.urlopen(req, timeout=60) as resp:
        return json.load(resp)["data"]


def build_row(model: dict):
    """OpenRouter model dict -> model_profiles row, with corrected $/1M pricing."""
    pricing = model.get("pricing", {})
    try:
        in_price = float(pricing.get("prompt", 0)) * 1_000_000.0
        out_price = float(pricing.get("completion", 0)) * 1_000_000.0
    except (TypeError, ValueError):
        in_price = out_price = 0.0
    in_price = max(in_price, 0.0001)
    out_price = max(out_price, 0.0001)

    ctx = int(model.get("context_length") or 128_000)
    aa = model.get("benchmarks", {}).get("artificial_analysis") or {}
    da = model.get("benchmarks", {}).get("design_arena") or []

    reasoning = float(aa.get("intelligence_index", 0.0) or 0.0)
    coding = float(aa.get("coding_index", 0.0) or 0.0)
    math = reasoning  # no independent math benchmark; intelligence is the proxy
    elo = lambda cat: next(
        (normalize_arena_elo(e["elo"]) for e in da if e.get("category") == cat), 0.0
    )
    frontend = elo("website")
    backend = elo("codecategories")
    long_context = max(0.0, min(100.0, ctx / 128_000.0 * 100.0))
    has_benchmarks = bool(aa) or bool(da)
    source = "openrouter-arena" if has_benchmarks else "openrouter"

    curated = CURATED_OVERRIDES.get(model["id"])
    if curated:
        frontend = curated.get("frontend", frontend)
        backend = curated.get("backend", backend)
        source = curated.get("source", source)

    return (
        model["id"], reasoning, coding, frontend, backend, math, long_context,
        in_price, out_price, ctx, source, "now", 0,
    )


UPSERT_SQL = """
INSERT INTO model_profiles
    (model, reasoning, coding, frontend, backend, math, long_context,
     input_cost_per_m, output_cost_per_m, context_window, source, updated_at, multimodal)
VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)
ON CONFLICT(model) DO UPDATE SET
    reasoning=excluded.reasoning, coding=excluded.coding, frontend=excluded.frontend,
    backend=excluded.backend, math=excluded.math, long_context=excluded.long_context,
    input_cost_per_m=excluded.input_cost_per_m, output_cost_per_m=excluded.output_cost_per_m,
    context_window=excluded.context_window, source=excluded.source,
    updated_at=excluded.updated_at, multimodal=excluded.multimodal
"""


def main():
    ap = argparse.ArgumentParser(description="Enrich raincode model score library from OpenRouter benchmarks.")
    ap.add_argument("--db", default=DEFAULT_DB, help="path to state.db (default ~/.raincode/state.db)")
    ap.add_argument("--list-pool", nargs="?", const="deepseek-v4-flash,qwen3.8-max,mimo-v2.5",
                    help="comma list of model-name substrings to print a summary for after enrich")
    args = ap.parse_args()

    print(f"fetching real benchmark data from {OPENROUTER_URL} ...")
    models = fetch_models()
    print(f"  {len(models)} models on OpenRouter")

    con = sqlite3.connect(args.db)
    cur = con.cursor()
    n = 0
    for m in models:
        cur.execute(UPSERT_SQL, build_row(m))
        n += 1
    con.commit()
    print(f"  upserted {n} model_profiles into {args.db}")

    if args.list_pool:
        pats = [p.strip() for p in args.list_pool.split(",") if p.strip()]
        print("\npool / candidate summary (model | reasoning | coding | frontend | backend | $in/M | ctx | src):")
        cur.execute(
            "SELECT model, reasoning, coding, frontend, backend, input_cost_per_m, "
            "context_window, source FROM model_profiles ORDER BY input_cost_per_m"
        )
        for model, reasoning, coding, frontend, backend, inp, ctx, src in cur.fetchall():
            if any(p.lower() in model.lower() for p in pats):
                print(f"  {model:<40} {reasoning:>5.1f} {coding:>5.1f} {frontend:>6.1f} "
                      f"{backend:>6.1f} {inp:>8.4f} {ctx:>8} {src}")
    con.close()


if __name__ == "__main__":
    sys.exit(main())
