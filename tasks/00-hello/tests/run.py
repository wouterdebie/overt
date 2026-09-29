#!/usr/bin/env python3
"""Black-box test for hello world.

Usage: python3 tests/run.py <command...>
"""
import subprocess
import sys

cmd = sys.argv[1:]
if not cmd:
    print(__doc__.strip())
    sys.exit(2)
p = subprocess.run(cmd, capture_output=True, timeout=30)
if p.returncode != 0:
    print(f"FAIL hello: exit status {p.returncode}, want 0")
    ok = False
elif p.stdout != b"hello, world\n":
    print(f"FAIL hello: printed {p.stdout!r}, want 'hello, world' and a newline")
    ok = False
else:
    print("ok   hello")
    ok = True
print(f"passed {int(ok)} of 1")
sys.exit(0 if ok else 1)
