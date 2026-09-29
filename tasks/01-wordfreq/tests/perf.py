#!/usr/bin/env python3
"""Times wordfreq on a generated file of about 100 MB.

Usage: python3 tests/perf.py <command...>

Generates tests/data/words.txt on first use (deterministic), runs
`<command> tests/data/words.txt 20` three times, checks the output, and prints
a JSON line: {"correct": bool, "seconds": best wall time}.
"""
import json
import os
import random
import string
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
DATA = os.path.join(HERE, "data")
TEXT = os.path.join(DATA, "words.txt")
EXPECTED = os.path.join(DATA, "words.expected")
TARGET_BYTES = 100 * 1024 * 1024
TOP = 20


def generate():
    os.makedirs(DATA, exist_ok=True)
    rng = random.Random(20260929)
    vocab = set()
    while len(vocab) < 50000:
        n = rng.choice([1, 2, 3, 4, 5, 5, 6, 6, 7, 7, 8, 9, 10, 12])
        vocab.add("".join(rng.choice(string.ascii_lowercase) for _ in range(n)))
    vocab = sorted(vocab)
    rng.shuffle(vocab)
    # Zipf-like weights: the k-th word is about 1/k as common as the first.
    weights = [1.0 / (k + 1) for k in range(len(vocab))]
    seps = [" "] * 20 + ["\n", ", ", ". ", "; ", " - ", "\t", "  ", "! ", "? ", " (1999) ", "'s "]
    counts = {}
    written = 0
    with open(TEXT + ".tmp", "w", encoding="ascii") as f:
        while written < TARGET_BYTES:
            words = rng.choices(vocab, weights=weights, k=10000)
            parts = []
            for w in words:
                style = rng.random()
                shown = w.upper() if style < 0.05 else w.capitalize() if style < 0.2 else w
                sep = rng.choice(seps)
                parts.append(shown)
                parts.append(sep)
                counts[w] = counts.get(w, 0) + 1
                # "'s " adds the word "s".
                if sep == "'s ":
                    counts["s"] = counts.get("s", 0) + 1
            chunk = "".join(parts)
            f.write(chunk)
            written += len(chunk)
    top = sorted(counts.items(), key=lambda kv: (-kv[1], kv[0]))[:TOP]
    with open(EXPECTED, "w") as f:
        f.write("".join(f"{c} {w}\n" for w, c in top))
    os.replace(TEXT + ".tmp", TEXT)


def main():
    cmd = sys.argv[1:]
    if not cmd:
        print(__doc__.strip())
        sys.exit(2)
    if not (os.path.exists(TEXT) and os.path.exists(EXPECTED)):
        print("generating tests/data/words.txt ...", file=sys.stderr)
        generate()
    want = open(EXPECTED).read()
    best = None
    correct = True
    for _ in range(3):
        start = time.perf_counter()
        p = subprocess.run(cmd + [TEXT, str(TOP)], capture_output=True, timeout=600)
        elapsed = time.perf_counter() - start
        if p.returncode != 0 or p.stdout.decode("utf-8", "replace") != want:
            correct = False
        best = elapsed if best is None else min(best, elapsed)
    print(json.dumps({"correct": correct, "seconds": round(best, 3)}))
    sys.exit(0 if correct else 1)


if __name__ == "__main__":
    main()
