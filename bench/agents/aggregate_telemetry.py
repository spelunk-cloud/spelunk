#!/usr/bin/env python3
"""Aggregate raw SWE-bench result JSONs into a per-cell telemetry/cost table.

Pure Python, stdlib only. No API keys, no DB, no network.

A *cell* is the tuple (model, harness, condition, instance_filter). Result rows
are grouped by cell; per cell we report task count and mean/median input tokens,
output tokens, turns, and wall seconds. Given a committed price config, we also
extrapolate cost per cell and project cost for prospective (not-yet-run) cells.

Provenance contract: rows follow bench/agents/README.md "Reproducibility /
provenance contract" (harness-matrix fields land via the harness
adapters). Legacy rows with no `harness` field are treated as harness "none".

Reported as tokens-to-outcome, never a headline "tokens saved" (binding P8).

Usage:
    python bench/agents/aggregate_telemetry.py \
        [--results-dir bench/results] [--prices bench/agents/pricing.json] \
        [--json-out out.json] [--md-out out.md]
"""

from __future__ import annotations

import argparse
import json
import statistics
import sys
from pathlib import Path
from typing import Any, Iterable

# Metrics summarised per cell. Order is the column order in the markdown table.
METRICS = ["input_tokens", "output_tokens", "turns", "wall_seconds"]

# Missing `harness` => a pre-harness-matrix (legacy) row. See README.
LEGACY_HARNESS = "none"


def load_prices(path: str | Path) -> dict[str, Any]:
    """Load the committed price config (CLI arg or default file)."""
    data = json.loads(Path(path).read_text())
    if "prices" not in data:
        raise ValueError(f"price config {path} has no 'prices' section")
    return data


def load_results(results_dir: str | Path) -> list[dict[str, Any]]:
    """Read every *.json under results_dir (recursively) as a list of rows.

    Each file is a JSON array of result objects (the shape agent.py / the
    harness adapters emit). A file that is a single object is accepted as a
    one-row list. Non-list / non-object JSON is skipped with a warning.
    """
    rows: list[dict[str, Any]] = []
    for path in sorted(Path(results_dir).rglob("*.json")):
        try:
            doc = json.loads(path.read_text())
        except json.JSONDecodeError as e:
            print(f"warning: skipping unparseable {path}: {e}", file=sys.stderr)
            continue
        items = doc if isinstance(doc, list) else [doc]
        for item in items:
            if isinstance(item, dict):
                rows.append(item)
            else:
                print(f"warning: skipping non-object row in {path}", file=sys.stderr)
    return rows


def cell_key(row: dict[str, Any]) -> tuple[str, str, str, str | None]:
    """(model, harness, condition, instance_filter). Legacy rows -> harness none."""
    return (
        row.get("model", "unknown"),
        row.get("harness") or LEGACY_HARNESS,
        row.get("condition", "unknown"),
        row.get("instance_filter"),  # kept distinct; None renders as "-"
    )


def group_by_cell(
    rows: Iterable[dict[str, Any]],
) -> dict[tuple[str, str, str, str | None], list[dict[str, Any]]]:
    cells: dict[tuple[str, str, str, str | None], list[dict[str, Any]]] = {}
    for row in rows:
        cells.setdefault(cell_key(row), []).append(row)
    return cells


def _nums(rows: list[dict[str, Any]], field: str) -> list[float]:
    out = []
    for r in rows:
        v = r.get(field)
        if isinstance(v, (int, float)) and not isinstance(v, bool):
            out.append(float(v))
    return out


def summarize_cell(rows: list[dict[str, Any]]) -> dict[str, Any]:
    """Task count plus mean/median for each metric."""
    summary: dict[str, Any] = {"tasks": len(rows)}
    for field in METRICS:
        vals = _nums(rows, field)
        summary[field] = {
            "mean": statistics.fmean(vals) if vals else None,
            "median": statistics.median(vals) if vals else None,
        }
    return summary


def _price_for(prices: dict[str, Any], model: str) -> dict[str, Any] | None:
    """Return the model's price entry only if it carries usable rates."""
    entry = prices.get("prices", {}).get(model)
    if not entry:
        return None
    if entry.get("input_per_mtok") is None or entry.get("output_per_mtok") is None:
        return None  # placeholder — never bill against a null price
    return entry


def cell_cost(
    rows: list[dict[str, Any]],
    prices: dict[str, Any],
    model: str,
) -> dict[str, Any]:
    """Cost extrapolation over the rows already present in a cell.

    raw_usd sums input_tokens x P_in + output_tokens x P_out across rows (all
    input billed at full rate). effective_usd re-bills the cache_read_input_tokens
    portion of each row's input at the cache-read rate (~0.1x input); rows without
    that field bill identically to raw. Both rows already span every seed present,
    so no n_seeds multiplier is applied here — that is for projections only.
    """
    entry = _price_for(prices, model)
    if entry is None:
        return {
            "priced": False,
            "reason": f"no verified price for model '{model}'",
            "raw_usd": None,
            "effective_usd": None,
        }

    p_in = entry["input_per_mtok"] / 1_000_000.0
    p_out = entry["output_per_mtok"] / 1_000_000.0
    cache_rate = entry.get("cache_read_per_mtok")
    if cache_rate is not None:
        p_cache = cache_rate / 1_000_000.0
    else:
        p_cache = p_in * float(prices.get("cache_read_multiplier", 0.1))

    raw = 0.0
    effective = 0.0
    for r in rows:
        in_tok = r.get("input_tokens") or 0
        out_tok = r.get("output_tokens") or 0
        cache_tok = min(r.get("cache_read_input_tokens") or 0, in_tok)
        out_cost = out_tok * p_out
        raw += in_tok * p_in + out_cost
        effective += (in_tok - cache_tok) * p_in + cache_tok * p_cache + out_cost

    return {
        "priced": True,
        "verified_on": entry.get("verified_on"),
        "raw_usd": raw,
        "effective_usd": effective,
    }


def project_cost(spec: dict[str, Any], prices: dict[str, Any]) -> dict[str, Any]:
    """Project cost for a prospective cell from per-task token estimates.

    cost ~= tasks x conditions x seeds x (in_tokens x P_in + out_tokens x P_out)
    """
    model = spec["model"]
    entry = _price_for(prices, model)
    runs = spec["tasks"] * spec.get("conditions", 1) * spec.get("seeds", 1)
    result: dict[str, Any] = {
        "name": spec.get("name", model),
        "model": model,
        "tasks": spec["tasks"],
        "conditions": spec.get("conditions", 1),
        "seeds": spec.get("seeds", 1),
        "total_runs": runs,
        "input_tokens_per_task": spec["input_tokens_per_task"],
        "output_tokens_per_task": spec["output_tokens_per_task"],
    }
    if entry is None:
        result.update(
            {"priced": False, "reason": f"no verified price for model '{model}'", "usd": None}
        )
        return result
    p_in = entry["input_per_mtok"] / 1_000_000.0
    p_out = entry["output_per_mtok"] / 1_000_000.0
    per_run = spec["input_tokens_per_task"] * p_in + spec["output_tokens_per_task"] * p_out
    result.update({"priced": True, "verified_on": entry.get("verified_on"), "usd": runs * per_run})
    return result


def build_report(
    rows: list[dict[str, Any]],
    prices: dict[str, Any],
) -> dict[str, Any]:
    """Machine-readable aggregation: one entry per cell, plus projections."""
    cells_out = []
    for key, cell_rows in sorted(group_by_cell(rows).items(), key=lambda kv: tuple(str(x) for x in kv[0])):
        model, harness, condition, instance_filter = key
        cells_out.append(
            {
                "model": model,
                "harness": harness,
                "condition": condition,
                "instance_filter": instance_filter,
                "summary": summarize_cell(cell_rows),
                "cost": cell_cost(cell_rows, prices, model),
            }
        )
    projections = [project_cost(s, prices) for s in prices.get("projections", [])]
    return {"cells": cells_out, "projections": projections}


def _fmt_num(v: float | None, digits: int = 0) -> str:
    if v is None:
        return "-"
    return f"{v:,.{digits}f}"


def _fmt_usd(v: float | None) -> str:
    return "-" if v is None else f"${v:,.2f}"


def render_markdown(report: dict[str, Any]) -> str:
    lines: list[str] = []
    lines.append("## Per-cell telemetry and cost")
    lines.append("")
    lines.append(
        "Cell = (model, harness, condition, instance_filter). "
        "Token/turn/wall figures are per-task mean (median). "
        "Cost is extrapolated from committed list prices; effective cost applies "
        "the cache-read discount where telemetry carries cache_read_input_tokens."
    )
    lines.append("")
    header = (
        "| Model | Harness | Condition | Filter | Tasks | "
        "Input tok mean(med) | Output tok mean(med) | Turns mean(med) | "
        "Wall s mean(med) | Raw $ | Effective $ | Priced |"
    )
    sep = "|" + "---|" * 12
    lines.append(header)
    lines.append(sep)
    for c in report["cells"]:
        s = c["summary"]
        cost = c["cost"]

        def mm(field: str, digits: int = 0) -> str:
            return f"{_fmt_num(s[field]['mean'], digits)} ({_fmt_num(s[field]['median'], digits)})"

        priced = "yes" if cost["priced"] else f"no ({cost.get('reason', '')})"
        lines.append(
            "| {model} | {harness} | {condition} | {flt} | {tasks} | "
            "{inp} | {out} | {turns} | {wall} | {raw} | {eff} | {priced} |".format(
                model=c["model"],
                harness=c["harness"],
                condition=c["condition"],
                flt=c["instance_filter"] if c["instance_filter"] is not None else "-",
                tasks=s["tasks"],
                inp=mm("input_tokens"),
                out=mm("output_tokens"),
                turns=mm("turns"),
                wall=mm("wall_seconds", 1),
                raw=_fmt_usd(cost["raw_usd"]),
                eff=_fmt_usd(cost["effective_usd"]),
                priced=priced,
            )
        )

    if report["projections"]:
        lines.append("")
        lines.append("### Projected cost (prospective cells)")
        lines.append("")
        lines.append(
            "Projection: tasks x conditions x seeds x "
            "(input_tokens x P_in + output_tokens x P_out). "
            "Per-task token counts are estimates from the committed config."
        )
        lines.append("")
        lines.append(
            "| Cell | Model | Tasks | Cond | Seeds | Runs | "
            "In tok/task | Out tok/task | Projected $ | Priced |"
        )
        lines.append("|" + "---|" * 10)
        for p in report["projections"]:
            priced = "yes" if p["priced"] else f"no ({p.get('reason', '')})"
            lines.append(
                "| {name} | {model} | {tasks} | {cond} | {seeds} | {runs} | "
                "{inp} | {out} | {usd} | {priced} |".format(
                    name=p["name"],
                    model=p["model"],
                    tasks=p["tasks"],
                    cond=p["conditions"],
                    seeds=p["seeds"],
                    runs=p["total_runs"],
                    inp=_fmt_num(p["input_tokens_per_task"]),
                    out=_fmt_num(p["output_tokens_per_task"]),
                    usd=_fmt_usd(p["usd"]),
                    priced=priced,
                )
            )
    return "\n".join(lines) + "\n"


def main(argv: list[str] | None = None) -> int:
    here = Path(__file__).resolve().parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--results-dir",
        default=str(here.parent / "results"),
        help="Directory of result JSONs (default: bench/results).",
    )
    parser.add_argument(
        "--prices",
        default=str(here / "pricing.json"),
        help="Price config JSON (default: bench/agents/pricing.json).",
    )
    parser.add_argument("--json-out", help="Write machine-readable report JSON here.")
    parser.add_argument("--md-out", help="Write the markdown table here.")
    args = parser.parse_args(argv)

    prices = load_prices(args.prices)
    rows = load_results(args.results_dir)
    report = build_report(rows, prices)

    md = render_markdown(report)
    if args.md_out:
        Path(args.md_out).write_text(md)
    if args.json_out:
        Path(args.json_out).write_text(json.dumps(report, indent=2) + "\n")
    if not args.md_out and not args.json_out:
        print(md)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
