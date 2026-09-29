#!/usr/bin/env python3
"""Black-box tests for wordfreq.

Usage: python3 tests/run.py <command...>

Runs `<command> <file> <n>` for each case and compares stdout and the exit
status. Error cases also require a message on stderr and nothing on stdout.
Prints one line per case and ends with "passed X of Y".
"""
import os
import subprocess
import sys
import tempfile

# (name, file contents or None for a missing file, arguments after the file, expected stdout, expected status)
CASES = [
    ("basic", "The the THE cat sat on the mat. Cat!\n", ["3"], "4 the\n2 cat\n1 mat\n", 0),
    ("fewer-words-than-n", "b a b\n", ["10"], "2 b\n1 a\n", 0),
    ("n-zero", "a b c\n", ["0"], "", 0),
    (
        "separators",
        "don't stop-believing 123abc a_b\ttab\n",
        ["10"],
        "1 a\n1 abc\n1 b\n1 believing\n1 don\n1 stop\n1 t\n1 tab\n",
        0,
    ),
    ("empty-file", "", ["5"], "", 0),
    ("only-separators", "123 ... !!\n\n", ["5"], "", 0),
    ("non-ascii", "café naïve CAFÉ\n", ["5"], "2 caf\n1 na\n1 ve\n", 0),
    ("ties-alphabetical", "zeta alpha beta alpha zeta beta gamma\n", ["2"], "2 alpha\n2 beta\n", 0),
    ("case-merges", "Go GO go gO\n", ["1"], "4 go\n", 0),
    ("no-trailing-newline", "one two two", ["2"], "2 two\n1 one\n", 0),
    ("many-lines", "a\nb\r\na\n\nc a", ["3"], "3 a\n1 b\n1 c\n", 0),
    ("large-count", "x " * 100000, ["1"], "100000 x\n", 0),
    ("long-word", "a" * 5000 + " b", ["5"], "1 " + "a" * 5000 + "\n1 b\n", 0),
    ("missing-n", "a\n", [], None, 1),
    ("extra-argument", "a\n", ["3", "extra"], None, 1),
    ("n-not-a-number", "a\n", ["three"], None, 1),
    ("negative-n", "a\n", ["-1"], None, 1),
    ("missing-file", None, ["3"], None, 1),
]


def run_case(cmd, tmp, name, contents, args, want_out, want_status):
    path = os.path.join(tmp, name + ".txt")
    if contents is not None:
        with open(path, "wb") as f:
            f.write(contents.encode("utf-8"))
    try:
        p = subprocess.run(cmd + [path] + args, capture_output=True, timeout=30)
    except subprocess.TimeoutExpired:
        return "timed out after 30s"
    out = p.stdout.decode("utf-8", "replace")
    if p.returncode != want_status:
        return f"exit status {p.returncode}, want {want_status}"
    if want_status == 0:
        if out != want_out:
            return f"stdout differs\n    got:  {out!r}\n    want: {want_out!r}"
    else:
        if out:
            return f"printed to stdout on an error: {out!r}"
        if not p.stderr.strip():
            return "no error message on stderr"
    return None


def main():
    cmd = sys.argv[1:]
    if not cmd:
        print(__doc__.strip())
        sys.exit(2)
    passed = 0
    # The "missing-n" case passes only the file, so it tests the argument count.
    with tempfile.TemporaryDirectory() as tmp:
        for name, contents, args, want_out, want_status in CASES:
            problem = run_case(cmd, tmp, name, contents, args, want_out, want_status)
            if problem:
                print(f"FAIL {name}: {problem}")
            else:
                passed += 1
                print(f"ok   {name}")
        # No arguments at all.
        p = subprocess.run(cmd, capture_output=True, timeout=30)
        if p.returncode == 1 and not p.stdout and p.stderr.strip():
            passed += 1
            print("ok   no-arguments")
        else:
            print(f"FAIL no-arguments: exit status {p.returncode}, stdout {p.stdout!r}")
    total = len(CASES) + 1
    print(f"passed {passed} of {total}")
    sys.exit(0 if passed == total else 1)


if __name__ == "__main__":
    main()
