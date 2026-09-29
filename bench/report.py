#!/usr/bin/env python3
"""Summarizes benchmark results.

Usage: bench/report.py [task...]

Reads bench/results/<task>.jsonl and prints, per task and language, the
median over runs of: total tokens (input, cache and output), output tokens,
cost, turns, builds, failed builds, tests passed and perf time.
"""
import json
import statistics
import sys
from pathlib import Path

RESULTS = Path(__file__).resolve().parent / "results"


def med(values):
    values = [v for v in values if v is not None]
    return statistics.median(values) if values else None


def fmt(v, digits=0):
    if v is None:
        return "-"
    return f"{v:,.{digits}f}"


def main():
    tasks = sys.argv[1:] or sorted(p.stem for p in RESULTS.glob("*.jsonl"))
    header = f"{'task':<14} {'lang':<6} {'runs':>4} {'tokens':>11} {'out tok':>9} {'cost $':>7} {'turns':>6} {'builds':>7} {'failed':>7} {'tests':>7} {'perf s':>7}"
    print(header)
    print("-" * len(header))
    for task in tasks:
        path = RESULTS / f"{task}.jsonl"
        if not path.exists():
            continue
        runs = [json.loads(l) for l in path.read_text().splitlines() if l.strip()]
        for lang in sorted({r["lang"] for r in runs}):
            rs = [r for r in runs if r["lang"] == lang]
            tokens = med([r["input_tokens"] + r["cache_read_tokens"] + r["cache_write_tokens"] + r["output_tokens"] for r in rs])
            passed = med([r["tests_passed"] / r["tests_total"] if r.get("tests_total") else 0 for r in rs])
            print(
                f"{task:<14} {lang:<6} {len(rs):>4} {fmt(tokens):>11} {fmt(med([r['output_tokens'] for r in rs])):>9} "
                f"{fmt(med([r['cost_usd'] for r in rs]), 2):>7} {fmt(med([r['turns'] for r in rs])):>6} "
                f"{fmt(med([r['builds'] for r in rs])):>7} {fmt(med([r['failed_builds'] for r in rs])):>7} "
                f"{fmt(passed * 100 if passed is not None else None):>6}% {fmt(med([r['perf_seconds'] for r in rs]), 2):>7}"
            )


if __name__ == "__main__":
    main()
