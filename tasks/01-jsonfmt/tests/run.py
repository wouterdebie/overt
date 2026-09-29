#!/usr/bin/env python3
"""Black-box tests for jsonfmt.

Usage: python3 tests/run.py <command...>

Feeds each case to the program on stdin. Valid input must print the expected
formatting and exit 0; invalid input must print nothing to stdout, exit 1,
and start stderr with `error: line L, column C:` at the right place.
Prints one line per case and ends with "passed X of Y".
"""
import os
import re
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.setrecursionlimit(20000)
from reference import format_json  # noqa: E402

# Valid documents, with the exact expected output for some of them. For the
# rest, and for invalid documents, the reference implementation decides.
VALID = [
    ("readme-example", b'{"a":[1, 2.50,{}], "b" : "x\\ty", "c": []}',
     b'{\n  "a": [\n    1,\n    2.50,\n    {}\n  ],\n  "b": "x\\ty",\n  "c": []\n}\n'),
    ("scalar-number", b"42", b"42\n"),
    ("scalar-string", b'  "hi"  \n', b'"hi"\n'),
    ("literals", b"[true,false,null]", b"[\n  true,\n  false,\n  null\n]\n"),
    ("empty-containers", b"[[],{}, [ ] ,{ }]", b"[\n  [],\n  {},\n  [],\n  {}\n]\n"),
    ("nested-objects", b'{"a":{"b":{"c":1}}}', b'{\n  "a": {\n    "b": {\n      "c": 1\n    }\n  }\n}\n'),
    ("numbers-verbatim", b"[-0, 0.5, 1E+10, 2.50e-3, -12, 1e5]", None),
    ("escapes-verbatim", b'["\\"\\\\\\/\\b\\f\\n\\r\\t\\u00e9\\uD83D\\uDE00"]', None),
    ("non-ascii", '{"naïve": "日本語"}'.encode("utf-8"), '{\n  "naïve": "日本語"\n}\n'.encode("utf-8")),
    ("duplicate-keys", b'{"a":1,"a":2}', b'{\n  "a": 1,\n  "a": 2\n}\n'),
    ("whitespace-everywhere", b' \t\r\n{ "a" \n:\t[ 1 ,\r\n2 ] } \n', None),
    ("deep-nesting", b"[" * 1000 + b"]" * 1000, None),
    ("deep-objects", b'{"a":' * 500 + b"1" + b"}" * 500, None),
    ("long-string", b'"' + b"x" * 100000 + b'"', None),
]

# Invalid documents, and the (line, column) where they fail.
INVALID = [
    ("trailing-comma-array", b"[1,]", (1, 4)),
    ("missing-colon", b'{"a" 1}', (1, 6)),
    ("unterminated-string", b'"abc', (1, 5)),
    ("leading-zero", b"01", (1, 2)),
    ("truncated-literal", b"tru", (1, 4)),
    ("literal-then-garbage", b"truex", (1, 5)),
    ("bad-escape", b'"\\x"', (1, 3)),
    ("bad-unicode-escape", b'"\\u12G4"', (1, 6)),
    ("raw-tab-in-string", b'"a\tb"', (1, 3)),
    ("empty-input", b"", (1, 1)),
    ("only-whitespace", b"   \n  ", (2, 3)),
    ("missing-comma", b"[1 2]", (1, 4)),
    ("trailing-comma-object", b'{"a":1,}', (1, 8)),
    ("fraction-without-digits", b"1.", (1, 3)),
    ("dot-then-exponent", b"1.e5", (1, 3)),
    ("lone-minus", b"-", (1, 2)),
    ("second-document", b"[1]\n]", (2, 1)),
    ("second-object", b'{"a":1}\n{', (2, 1)),
    ("broken-literal-multiline", b'{\n  "a": tru e\n}', (2, 11)),
    ("non-string-key", b"{1:2}", (1, 2)),
    ("negative-leading-zero", b"[-01]", (1, 4)),
    ("exponent-without-digits", b"[1e]", (1, 4)),
    ("exponent-sign-only", b"[1e+]", (1, 5)),
    ("truncated-null", b"nul", (1, 4)),
    ("missing-comma-object", b'{"a":1 "b":2}', (1, 8)),
    ("key-without-value", b'{"a",}', (1, 5)),
    ("unclosed-array", b"[1, 2", (1, 6)),
    ("unclosed-deep", b"[" * 1000, (1, 1001)),
    ("single-quotes", b"['a']", (1, 2)),
    ("plus-sign", b"+1", (1, 1)),
    ("control-char-newline", b'"a\nb"', (1, 3)),
]

ERROR_LINE = re.compile(rb"^error: line (\d+), column (\d+):")


def run(cmd, data):
    try:
        return subprocess.run(cmd, input=data, capture_output=True, timeout=30)
    except subprocess.TimeoutExpired:
        return None


def main():
    cmd = sys.argv[1:]
    if not cmd:
        print(__doc__.strip())
        sys.exit(2)
    passed = 0
    total = 0
    for name, data, want in VALID:
        total += 1
        ref, err = format_json(data)
        assert err is None, f"reference rejects valid case {name}"
        if want is not None:
            assert ref == want, f"reference disagrees with the expected output of {name}"
        p = run(cmd, data)
        if p is None:
            print(f"FAIL {name}: timed out after 30s")
        elif p.returncode != 0:
            print(f"FAIL {name}: exit status {p.returncode}, want 0; stderr: {p.stderr[:200]!r}")
        elif p.stdout != ref:
            print(f"FAIL {name}: output differs\n    got:  {p.stdout[:300]!r}\n    want: {ref[:300]!r}")
        else:
            passed += 1
            print(f"ok   {name}")
    for name, data, want in INVALID:
        total += 1
        ref, err = format_json(data)
        assert ref is None and err == want, f"reference puts {name} at {err}, the table says {want}"
        p = run(cmd, data)
        if p is None:
            print(f"FAIL {name}: timed out after 30s")
            continue
        m = ERROR_LINE.match(p.stderr)
        got = (int(m.group(1)), int(m.group(2))) if m else None
        if p.returncode != 1:
            print(f"FAIL {name}: exit status {p.returncode}, want 1")
        elif p.stdout:
            print(f"FAIL {name}: printed to stdout on invalid input: {p.stdout[:100]!r}")
        elif got is None:
            print(f"FAIL {name}: stderr doesn't start with `error: line L, column C:`: {p.stderr[:200]!r}")
        elif got != want:
            print(f"FAIL {name}: reported line {got[0]}, column {got[1]}; want line {want[0]}, column {want[1]}")
        else:
            passed += 1
            print(f"ok   {name}")
    print(f"passed {passed} of {total}")
    sys.exit(0 if passed == total else 1)


if __name__ == "__main__":
    main()
