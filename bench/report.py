#!/usr/bin/env python3
"""Summarizes benchmark results.

Usage: bench/report.py [task...] [--all]

Reads bench/results/<task>.jsonl and prints, per task and language, the
median over runs of: tool calls and tokens (input including cache, and
output), cost, builds, failed builds, tests passed and speed. The "to pass"
columns are the numbers at the first time every test passed.

The last two columns are what learning Overt costs:
- "docs tok": tokens of docs the agent had, from its prompt or looked up.
  Docs in the prompt are the first call's context minus that of runs
  without docs in the same protocol. Looked-up docs are estimated from the
  transcript (see `analyze` in run.py).
- "docs %": those tokens counted once per call that carried them, as a
  share of all input tokens.

Only runs of the current protocol are shown; `--all` shows every protocol.
Runs recorded before protocols existed count as "v1: speed matters".
"""
import json
import statistics
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from run import PROTOCOL, analyze  # noqa: E402

BENCH = Path(__file__).resolve().parent
RESULTS = BENCH / "results"


def med(values):
    values = [v for v in values if v is not None]
    return statistics.median(values) if values else None


def fmt(v, digits=0):
    if v is None:
        return "-"
    return f"{v:,.{digits}f}"


def with_contexts(r):
    """The record, with per-call context sizes and doc lookups from its
    transcript if it was recorded before those existed."""
    if "contexts" in r:
        return r
    t = BENCH / "runs" / r["run_id"] / "transcript.jsonl"
    if not t.exists():
        return r
    st = analyze(t.read_text(), r.get("tests_total"))
    return {**r, **{k: st[k] for k in ("contexts", "doc_lookup_tokens", "doc_lookup_carried")}}


def doc_costs(rs):
    """Per run: (tokens of docs the agent had, share of input tokens spent carrying them)."""
    plain = [r["contexts"][0] for r in rs if r.get("contexts") and not r.get("docs_in_prompt")]
    base = statistics.median(plain) if plain else None
    out = []
    for r in rs:
        if not r.get("contexts"):
            out.append((None, None))
            continue
        in_prompt = 0
        if r.get("docs_in_prompt"):
            if base is None:
                out.append((None, None))
                continue
            in_prompt = r["contexts"][0] - base
        total = r["input_tokens"] + r["cache_read_tokens"] + r["cache_write_tokens"]
        carried = in_prompt * len(r["contexts"]) + r.get("doc_lookup_carried", 0)
        out.append((in_prompt + r.get("doc_lookup_tokens", 0), carried / total if total else None))
    return out


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    show_all = "--all" in sys.argv
    tasks = args or sorted(p.stem for p in RESULTS.glob("*.jsonl"))
    header = (
        f"{'lang':<10} {'runs':>4} {'calls':>6} {'to pass':>8} {'in tok':>10} {'to pass':>9} {'out tok':>8} "
        f"{'cost $':>7} {'builds':>7} {'failed':>7} {'tests':>6} {'perf s':>7} {'docs tok':>9} {'docs %':>7}"
    )
    for task in tasks:
        path = RESULTS / f"{task}.jsonl"
        if not path.exists():
            continue
        runs = [with_contexts(json.loads(l)) for l in path.read_text().splitlines() if l.strip()]
        protocols = sorted({r.get("protocol", "v1: speed matters") for r in runs})
        for proto in protocols if show_all else [PROTOCOL]:
            rs_all = [r for r in runs if r.get("protocol", "v1: speed matters") == proto]
            if not rs_all:
                continue
            print(f"\n{task} ({proto})")
            print(header)
            print("-" * len(header))
            costs = dict(zip((r["run_id"] for r in rs_all), doc_costs(rs_all)))
            for lang in sorted({r["lang"] for r in rs_all}):
                rs = [r for r in rs_all if r["lang"] == lang]
                fp = [r.get("first_pass") or {} for r in rs]
                docs = med([costs[r["run_id"]][0] for r in rs])
                share = med([costs[r["run_id"]][1] for r in rs])
                tokens = med([r["input_tokens"] + r["cache_read_tokens"] + r["cache_write_tokens"] for r in rs])
                passed = med([r["tests_passed"] / r["tests_total"] if r.get("tests_total") else 0 for r in rs])
                print(
                    f"{lang:<10} {len(rs):>4} {fmt(med([r['tool_calls'] for r in rs])):>6} "
                    f"{fmt(med([f.get('tool_calls') for f in fp])):>8} {fmt(tokens):>10} {fmt(med([f.get('input_tokens') for f in fp])):>9} "
                    f"{fmt(med([r['output_tokens'] for r in rs])):>8} {fmt(med([r['cost_usd'] for r in rs]), 2):>7} "
                    f"{fmt(med([r['builds'] for r in rs])):>7} {fmt(med([r['failed_builds'] for r in rs])):>7} "
                    f"{fmt(passed * 100 if passed is not None else None):>5}% {fmt(med([r['perf_seconds'] for r in rs]), 2):>7} "
                    f"{fmt(docs):>9} {fmt(share * 100 if share is not None else None):>6}%"
                )


if __name__ == "__main__":
    main()
