#!/bin/sh
# Black-box test: runs the program given as arguments and checks what it prints.
set -u
out=$(mktemp)
trap 'rm -f "$out"' EXIT
"$@" > "$out"
status=$?
if [ "$status" -ne 0 ]; then
  echo "FAIL: exit status $status, want 0"
  exit 1
fi
if ! printf 'hello, world\n' | cmp -s - "$out"; then
  echo "FAIL: output differs; want exactly 'hello, world' and a newline, got:"
  cat "$out"
  exit 1
fi
echo "PASS"
