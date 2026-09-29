#!/usr/bin/env python3
"""Times jsonfmt on a generated document of about 50 MB.

Usage: python3 tests/perf.py <command...>

Generates tests/data/big.json on first use (deterministic), runs the program
on it three times with the file on stdin, checks the output against the
reference, and prints a JSON line: {"correct": bool, "seconds": best wall time}.
"""
import json
import os
import random
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from reference import format_json  # noqa: E402

DATA = os.path.join(HERE, "data")
DOC = os.path.join(DATA, "big.json")
EXPECTED = os.path.join(DATA, "big.expected")
TARGET_BYTES = 50 * 1024 * 1024


def generate():
    os.makedirs(DATA, exist_ok=True)
    rng = random.Random(4242)
    words = ["alpha", "beta", "gamma", "delta", "tab\\there", "quote\\\"d", "caf\\u00e9", "日本", "x" * 40]
    parts = ["["]
    size = 1
    first = True
    while size < TARGET_BYTES:
        tags = ",".join(f'"{rng.choice(words)}"' for _ in range(rng.randint(0, 4)))
        nums = ", ".join(rng.choice([str(rng.randint(-10**6, 10**6)), f"{rng.random():.6f}", f"{rng.randint(1, 9)}.{rng.randint(0, 99)}e{rng.randint(-20, 20)}"]) for _ in range(rng.randint(0, 6)))
        item = (
            f'{"" if first else ","}\n {{"id": {rng.randint(0, 10**9)}, "name": "{rng.choice(words)} {rng.randint(0, 999)}",'
            f' "active": {rng.choice(["true", "false"])}, "parent": {rng.choice(["null", str(rng.randint(0, 999))])},'
            f' "tags": [{tags}], "scores": [{nums}], "meta": {{"depth": {{"level": {rng.randint(0, 9)}, "items": []}}, "empty": {{}}}}}}'
        )
        first = False
        parts.append(item)
        size += len(item.encode("utf-8"))
    parts.append("\n]\n")
    data = "".join(parts).encode("utf-8")
    out, err = format_json(data)
    assert err is None, err
    with open(DOC + ".tmp", "wb") as f:
        f.write(data)
    with open(EXPECTED, "wb") as f:
        f.write(out)
    os.replace(DOC + ".tmp", DOC)


def main():
    cmd = sys.argv[1:]
    if not cmd:
        print(__doc__.strip())
        sys.exit(2)
    if not (os.path.exists(DOC) and os.path.exists(EXPECTED)):
        print("generating tests/data/big.json ...", file=sys.stderr)
        generate()
    want = open(EXPECTED, "rb").read()
    best = None
    correct = True
    for _ in range(3):
        with open(DOC, "rb") as f:
            start = time.perf_counter()
            p = subprocess.run(cmd, stdin=f, capture_output=True, timeout=600)
            elapsed = time.perf_counter() - start
        if p.returncode != 0 or p.stdout != want:
            correct = False
        best = elapsed if best is None else min(best, elapsed)
    print(json.dumps({"correct": correct, "seconds": round(best, 3)}))
    sys.exit(0 if correct else 1)


if __name__ == "__main__":
    main()
