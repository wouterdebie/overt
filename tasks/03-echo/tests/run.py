#!/usr/bin/env python3
"""Black-box tests for the echo server.

Usage: python3 tests/run.py <command...>

Starts `<command> <port>` on a free port, runs each case against it with
plain sockets, and stops the server at the end. The argument errors run the
command on its own. Prints one line per case and ends with "passed X of Y".
"""
import os
import random
import socket
import subprocess
import sys
import threading
import time

TIMEOUT = 5


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def connect(port):
    s = socket.create_connection(("127.0.0.1", port), timeout=TIMEOUT)
    s.settimeout(TIMEOUT)
    return s


def recv_exactly(s, n):
    data = b""
    while len(data) < n:
        chunk = s.recv(n - len(data))
        if not chunk:
            raise AssertionError(f"connection closed after {len(data)} of {n} bytes")
        data += chunk
    return data


def expect_echo(s, data):
    s.sendall(data)
    got = recv_exactly(s, len(data))
    if got != data:
        raise AssertionError(f"sent {data[:60]!r}, got {got[:60]!r}")


def expect_closed(s):
    """The server closes the connection soon, without sending anything more."""
    try:
        data = s.recv(100)
    except ConnectionResetError:
        return
    if data:
        raise AssertionError(f"expected the connection to close, got {data!r}")


# Each case takes the port and raises AssertionError on failure.


def one_line(port):
    with connect(port) as s:
        expect_echo(s, b"hello\n")


def several_lines(port):
    with connect(port) as s:
        expect_echo(s, b"one\ntwo\nthree\n")
        expect_echo(s, b"four\n")


def split_line(port):
    with connect(port) as s:
        s.sendall(b"hel")
        time.sleep(0.1)
        s.sendall(b"lo wor")
        time.sleep(0.1)
        s.sendall(b"ld\n")
        got = recv_exactly(s, 12)
        if got != b"hello world\n":
            raise AssertionError(f"got {got!r}")


def empty_line(port):
    with connect(port) as s:
        expect_echo(s, b"\n\nx\n")


def any_bytes(port):
    with connect(port) as s:
        expect_echo(s, bytes(b for b in range(256) if b != 10) + b"\n")


def long_line(port):
    with connect(port) as s:
        line = random.Random(1).randbytes(64 * 1024 - 1).replace(b"\n", b"x") + b"\n"
        # Send and receive at the same time, so neither side's buffer fills up.
        got = []
        reader = threading.Thread(target=lambda: got.append(recv_exactly(s, len(line))))
        reader.start()
        s.sendall(line)
        reader.join(TIMEOUT * 2)
        if not got or got[0] != line:
            raise AssertionError("the 64 KiB line didn't come back unchanged")


def many_clients(port):
    errors = []

    def client(i):
        try:
            with connect(port) as s:
                for j in range(20):
                    expect_echo(s, f"client {i} line {j}\n".encode())
        except Exception as e:  # noqa: BLE001
            errors.append(f"client {i}: {e}")

    threads = [threading.Thread(target=client, args=(i,)) for i in range(50)]
    for t in threads:
        t.start()
    for t in threads:
        t.join(TIMEOUT * 3)
    if errors:
        raise AssertionError(errors[0])


def silent_client(port):
    # One client sends half a line and waits; another must still be served.
    with connect(port) as quiet:
        quiet.sendall(b"no newline yet")
        with connect(port) as s:
            expect_echo(s, b"still served\n")
        quiet.sendall(b"\n")
        got = recv_exactly(quiet, 15)
        if got != b"no newline yet\n":
            raise AssertionError(f"got {got!r}")


def close_with_partial_line(port):
    with connect(port) as s:
        expect_echo(s, b"done\n")
        s.sendall(b"unfinished")
        s.shutdown(socket.SHUT_WR)
        expect_closed(s)


def abrupt_close(port):
    s = connect(port)
    s.sendall(b"half")
    # SO_LINGER 0 makes close send a reset instead of a normal close.
    s.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, b"\x01\x00\x00\x00\x00\x00\x00\x00")
    s.close()
    time.sleep(0.1)
    with connect(port) as t:
        expect_echo(t, b"after reset\n")


CASES = [
    one_line,
    several_lines,
    split_line,
    empty_line,
    any_bytes,
    long_line,
    many_clients,
    silent_client,
    close_with_partial_line,
    abrupt_close,
]


def start(cmd, args):
    port = free_port()
    p = subprocess.Popen(cmd + [str(port)] + args, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    deadline = time.time() + 10
    while time.time() < deadline:
        if p.poll() is not None:
            raise RuntimeError(f"the server exited with status {p.returncode}: {p.stderr.read().decode()[:300]}")
        try:
            socket.create_connection(("127.0.0.1", port), timeout=1).close()
            return p, port
        except OSError:
            time.sleep(0.05)
    p.kill()
    raise RuntimeError("the server didn't start listening within 10s")


def arg_errors(cmd):
    """(name, arguments) that must fail with status 1 and a message on stderr."""
    problems = []
    for name, args in [("no-arguments", []), ("port-not-a-number", ["http"]), ("port-zero", ["0"]), ("port-too-big", ["70000"]), ("extra-argument", ["8080", "x"])]:
        try:
            p = subprocess.run(cmd + args, capture_output=True, timeout=10)
        except subprocess.TimeoutExpired:
            problems.append((name, "still running after 10s"))
            continue
        if p.returncode != 1 or not p.stderr.strip():
            problems.append((name, f"exit status {p.returncode}, stderr {p.stderr[:100]!r}"))
        else:
            problems.append((name, None))
    return problems


def main():
    cmd = sys.argv[1:]
    if not cmd:
        print(__doc__.strip())
        sys.exit(2)
    if os.sep in cmd[0] and os.path.exists(cmd[0]):
        cmd[0] = os.path.abspath(cmd[0])
    passed = 0
    total = len(CASES) + 5
    try:
        server, port = start(cmd, [])
    except RuntimeError as e:
        print(f"FAIL server: {e}")
        print(f"passed 0 of {total}")
        sys.exit(1)
    try:
        for case in CASES:
            name = case.__name__.replace("_", "-")
            try:
                case(port)
                passed += 1
                print(f"ok   {name}")
            except Exception as e:  # noqa: BLE001
                print(f"FAIL {name}: {e}")
            if server.poll() is not None:
                print(f"FAIL server exited with status {server.returncode} during {name}")
                break
    finally:
        server.kill()
        server.wait()
    for name, problem in arg_errors(cmd):
        if problem:
            print(f"FAIL {name}: {problem}")
        else:
            passed += 1
            print(f"ok   {name}")
    print(f"passed {passed} of {total}")
    sys.exit(0 if passed == total else 1)


if __name__ == "__main__":
    main()
