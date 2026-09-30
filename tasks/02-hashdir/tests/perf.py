#!/usr/bin/env python3
"""Times hashdir on a generated tree of about 512 MiB.

Usage: python3 tests/perf.py <command...>

Generates tests/data/tree on first use (deterministic): 16 files of 16 MiB
and 4,000 files of 1–128 KiB in nested directories. Runs `<command> tree`
in tests/data once to warm the file cache and check the output, then three
times more, and prints a JSON line: {"correct": bool, "seconds": best wall
time, "cpu_seconds": user and system time of that run}. CPU time over wall
time shows how much the program ran in parallel.
"""
import hashlib
import json
import os
import random
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
DATA = os.path.join(HERE, "data")
TREE = os.path.join(DATA, "tree")
EXPECTED = os.path.join(DATA, "tree.expected")


def generate():
    rng = random.Random(20260930)
    files = [(f"big/b{i:02d}.bin", 16 * 1024 * 1024) for i in range(16)]
    for i in range(4000):
        d = f"small/s{i % 40:02d}/t{i % 7}"
        files.append((f"{d}/f{i:04d}.dat", rng.randrange(1024, 128 * 1024)))
    lines = []
    tmp = TREE + ".tmp"
    subprocess.run(["rm", "-rf", tmp])
    for rel, size in files:
        path = os.path.join(tmp, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        data = rng.randbytes(size)
        with open(path, "wb") as f:
            f.write(data)
        lines.append((f"tree/{rel}".encode(), hashlib.sha256(data).hexdigest()))
    lines.sort()
    with open(EXPECTED, "w") as f:
        f.write("".join(f"{h}  {p.decode()}\n" for p, h in lines))
    os.replace(tmp, TREE)


def run(cmd):
    """Runs the command in DATA; returns (stdout, wall seconds, cpu seconds)."""
    start = time.perf_counter()
    p = subprocess.Popen(cmd + ["tree"], cwd=DATA, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    out = p.stdout.read()
    _, status, usage = os.wait4(p.pid, 0)
    wall = time.perf_counter() - start
    p.returncode = os.waitstatus_to_exitcode(status)
    return (out if p.returncode == 0 else None), wall, usage.ru_utime + usage.ru_stime


def main():
    cmd = sys.argv[1:]
    if not cmd:
        print(__doc__.strip())
        sys.exit(2)
    if not os.path.exists(TREE):
        os.makedirs(DATA, exist_ok=True)
        print("generating tests/data/tree ...", file=sys.stderr)
        generate()
    with open(EXPECTED, "rb") as f:
        want = f.read()
    out, _, _ = run(cmd)
    correct = out == want
    best = None
    for _ in range(3):
        _, wall, cpu = run(cmd)
        if best is None or wall < best[0]:
            best = (wall, cpu)
    print(json.dumps({"correct": correct, "seconds": round(best[0], 3), "cpu_seconds": round(best[1], 3)}))


if __name__ == "__main__":
    main()
