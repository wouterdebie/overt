#!/usr/bin/env python3
"""Black-box tests for hashdir.

Usage: python3 tests/run.py <command...>

Builds a directory tree for each case, runs `<command> <dir>` in the tree's
parent directory, and compares stdout and the exit status with what
Python's hashlib gives. Error cases also require a message on stderr and
nothing on stdout. Prints one line per case and ends with "passed X of Y".
"""
import hashlib
import os
import random
import stat
import subprocess
import sys
import tempfile


def write(root, rel, data):
    path = os.path.join(root, rel)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)


def rand_bytes(seed, n):
    return random.Random(seed).randbytes(n)


# Each builder fills an empty directory `root`.


def basic(root):
    write(root, "a.txt", b"hello\n")
    write(root, "b/c.txt", b"nested file\n")
    write(root, "b/d/e.txt", b"deeper\n")


def empty_dirs(root):
    for d in ["x", "x/y", "z"]:
        os.makedirs(os.path.join(root, d), exist_ok=True)


def empty_file(root):
    write(root, "empty", b"")


def padding_sizes(root):
    # SHA-256 pads each message to a multiple of 64 bytes, with at least 9 bytes of padding.
    for n in [1, 3, 55, 56, 57, 63, 64, 65, 111, 119, 120, 127, 128, 129, 1000]:
        write(root, f"size-{n:04d}", rand_bytes(n, n))


def all_byte_values(root):
    write(root, "bytes", bytes(range(256)) * 4)


def large_file(root):
    write(root, "large.bin", rand_bytes(7, 3 * 1024 * 1024 + 17))


def many_files(root):
    rng = random.Random(11)
    for i in range(2000):
        write(root, f"d{i % 20:02d}/f{i:04d}", rng.randbytes(rng.randrange(0, 4000)))


def deep(root):
    rel = "/".join(f"level{i}" for i in range(40))
    write(root, rel + "/bottom.txt", b"at the bottom\n")
    write(root, "level0/level1/side.txt", b"on the side\n")


def names(root):
    for name in ["with space.txt", "café.txt", "日本語.txt", "-dash", "B", "a", "_under", "Z", "émile"]:
        write(root, name, name.encode("utf-8"))


def hidden(root):
    write(root, ".hidden", b"secret\n")
    write(root, ".config/settings", b"x=1\n")
    write(root, "visible", b"shown\n")


def symlinks(root):
    write(root, "real/file.txt", b"real\n")
    os.symlink("real/file.txt", os.path.join(root, "link-to-file"))
    os.symlink("real", os.path.join(root, "link-to-dir"))
    os.symlink("nowhere", os.path.join(root, "broken-link"))


def byte_order(root):
    # Sorting whole paths puts "a-b" and "a.b" before "a/x", since "-" and "." come before "/".
    write(root, "a/x", b"1")
    write(root, "a.b", b"2")
    write(root, "a-b", b"3")
    write(root, "a0", b"4")


def unreadable_file(root):
    write(root, "ok.txt", b"fine\n")
    write(root, "locked.txt", b"no\n")
    os.chmod(os.path.join(root, "locked.txt"), 0)


def unreadable_dir(root):
    write(root, "ok.txt", b"fine\n")
    write(root, "locked/inside.txt", b"no\n")
    os.chmod(os.path.join(root, "locked"), 0)


def expected(parent, arg):
    """The output `hashdir arg` should print, run in `parent`."""
    top = os.path.join(parent, arg)
    lines = []
    for dirpath, dirnames, filenames in os.walk(top):
        for name in filenames:
            full = os.path.join(dirpath, name)
            if not stat.S_ISREG(os.lstat(full).st_mode):
                continue
            rel = os.path.relpath(full, top)
            path = arg + ("" if arg.endswith("/") else "/") + rel
            with open(full, "rb") as f:
                lines.append((path.encode("utf-8"), hashlib.sha256(f.read()).hexdigest()))
    lines.sort()
    return "".join(f"{h}  {p.decode('utf-8')}\n" for p, h in lines)


# (name, builder, argument, expected status). The tree is built in `<tmp>/<name>/tree`,
# and the command runs in `<tmp>/<name>`, or in the tree itself when the argument is ".".
CASES = [
    ("basic", basic, "tree", 0),
    ("trailing-slash", basic, "tree/", 0),
    ("dot", basic, ".", 0),
    ("nested-argument", basic, "tree/b", 0),
    ("empty-dirs", empty_dirs, "tree", 0),
    ("empty-file", empty_file, "tree", 0),
    ("padding-sizes", padding_sizes, "tree", 0),
    ("all-byte-values", all_byte_values, "tree", 0),
    ("large-file", large_file, "tree", 0),
    ("many-files", many_files, "tree", 0),
    ("deep", deep, "tree", 0),
    ("names", names, "tree", 0),
    ("hidden", hidden, "tree", 0),
    ("symlinks", symlinks, "tree", 0),
    ("byte-order", byte_order, "tree", 0),
    ("missing-dir", basic, "no-such-dir", 1),
    ("not-a-directory", basic, "tree/a.txt", 1),
    ("unreadable-file", unreadable_file, "tree", 1),
    ("unreadable-dir", unreadable_dir, "tree", 1),
]


def run_case(cmd, tmp, name, build, arg, want_status):
    case_dir = os.path.join(tmp, name)
    tree = os.path.join(case_dir, "tree")
    os.makedirs(tree)
    build(tree)
    cwd = tree if arg == "." else case_dir
    want_out = expected(cwd, arg) if want_status == 0 else None
    try:
        p = subprocess.run(cmd + [arg], cwd=cwd, capture_output=True, timeout=60)
    except subprocess.TimeoutExpired:
        return "timed out after 60s"
    out = p.stdout.decode("utf-8", "replace")
    if p.returncode != want_status:
        return f"exit status {p.returncode}, want {want_status}; stderr: {p.stderr.decode('utf-8', 'replace')[:300]!r}"
    if want_status == 0:
        if out != want_out:
            got, want = out.splitlines(), want_out.splitlines()
            for i, (g, w) in enumerate(zip(got, want)):
                if g != w:
                    return f"line {i + 1} differs\n    got:  {g!r}\n    want: {w!r}"
            return f"got {len(got)} lines, want {len(want)}"
    else:
        if out:
            return f"printed to stdout on an error: {out[:200]!r}"
        if not p.stderr.strip():
            return "no error message on stderr"
    return None


def unlock(tmp):
    """Makes everything readable again, so the temporary directory can be removed."""
    for dirpath, dirnames, filenames in os.walk(tmp):
        for n in dirnames + filenames:
            full = os.path.join(dirpath, n)
            if not os.path.islink(full):
                os.chmod(full, 0o755)


def main():
    cmd = sys.argv[1:]
    if not cmd:
        print(__doc__.strip())
        sys.exit(2)
    # Each case runs in its own directory, so a relative path like bin/hashdir
    # has to be made absolute first.
    if os.sep in cmd[0] and os.path.exists(cmd[0]):
        cmd[0] = os.path.abspath(cmd[0])
    if os.geteuid() == 0:
        sys.exit("run the tests as a normal user: root can read unreadable files")
    passed = 0
    tmp = tempfile.mkdtemp()
    try:
        for name, build, arg, want_status in CASES:
            problem = run_case(cmd, tmp, name, build, arg, want_status)
            if problem:
                print(f"FAIL {name}: {problem}")
            else:
                passed += 1
                print(f"ok   {name}")
        for name, args in [("no-arguments", []), ("two-arguments", ["a", "b"])]:
            p = subprocess.run(cmd + args, cwd=tmp, capture_output=True, timeout=60)
            if p.returncode == 1 and not p.stdout and p.stderr.strip():
                passed += 1
                print(f"ok   {name}")
            else:
                print(f"FAIL {name}: exit status {p.returncode}, stdout {p.stdout[:200]!r}")
    finally:
        unlock(tmp)
        subprocess.run(["rm", "-rf", tmp])
    total = len(CASES) + 2
    print(f"passed {passed} of {total}")
    sys.exit(0 if passed == total else 1)


if __name__ == "__main__":
    main()
