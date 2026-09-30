#!/usr/bin/env python3
"""Black-box tests for the chat server.

Usage: python3 tests/run.py <command...>

Starts `<command> <port> 60` on a free port and runs each case against it
with plain sockets. Each case uses its own nicknames and rooms. The idle
timeout runs against a second server started with `<command> <port> 1`, and
the argument errors run the command on its own. Prints one line per case and
ends with "passed X of Y".
"""
import os
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


class Client:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=TIMEOUT)
        self.buf = b""

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()

    def close(self):
        try:
            self.s.close()
        except OSError:
            pass

    def send(self, line):
        self.s.sendall(line.encode() + b"\n")

    def line(self, timeout=TIMEOUT):
        """The next line, without "\\n", or None if the server closed the connection."""
        self.s.settimeout(timeout)
        while b"\n" not in self.buf:
            try:
                chunk = self.s.recv(65536)
            except ConnectionResetError:
                chunk = b""
            if not chunk:
                return None
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        return line.decode("utf-8", "replace")

    def expect(self, *want):
        for w in want:
            try:
                got = self.line()
            except socket.timeout:
                raise AssertionError(f"expected {w!r}, got nothing within {TIMEOUT}s")
            if got != w:
                raise AssertionError(f"expected {w!r}, got {got!r}")

    def expect_error(self):
        try:
            got = self.line()
        except socket.timeout:
            raise AssertionError(f"expected an error, got nothing within {TIMEOUT}s")
        if got is None or not got.startswith("error: "):
            raise AssertionError(f"expected a line starting with 'error: ', got {got!r}")

    def quiet(self, secs=0.3):
        try:
            got = self.line(timeout=secs)
        except socket.timeout:
            return
        raise AssertionError(f"expected nothing, got {got!r}")

    def drain(self):
        """Reads whatever has arrived, until nothing comes for a moment."""
        while True:
            try:
                if self.line(timeout=0.3) is None:
                    return
            except socket.timeout:
                return

    def closed(self, timeout=TIMEOUT):
        """The server closes the connection within `timeout`, maybe after sending other lines."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                if self.line(timeout=max(0.05, deadline - time.time())) is None:
                    return
            except socket.timeout:
                break
        raise AssertionError(f"the server didn't close the connection within {timeout}s")


def nick(c, name):
    c.send(f"/nick {name}")
    c.expect("ok")


def join(c, room):
    c.send(f"/join {room}")
    c.expect("ok")


def room_of(port, room, *names):
    """Clients with these nicknames in `room`, with everyone's pending lines read."""
    cs = []
    for n in names:
        c = Client(port)
        nick(c, n)
        join(c, room)
        cs.append(c)
    for c in cs:
        c.drain()
    return cs


# Each case takes the port and raises AssertionError on failure.


def nick_ok(port):
    with Client(port) as a:
        nick(a, "alice")


def nick_invalid(port):
    with Client(port) as a:
        for bad in ["/nick", "/nick a-b", "/nick " + "x" * 17, "/nick two words", "/nick café"]:
            a.send(bad)
            a.expect_error()
        nick(a, "Ok_16_" + "x" * 10)


def nick_taken(port):
    with Client(port) as a, Client(port) as b:
        nick(a, "carol")
        b.send("/nick carol")
        b.expect_error()
        a.close()
        # The name is free once the server has seen the disconnect.
        for _ in range(20):
            time.sleep(0.1)
            b.send("/nick carol")
            if b.line() == "ok":
                return
        raise AssertionError("the nickname wasn't freed after its owner disconnected")


def message_needs_nick(port):
    with Client(port) as a:
        a.send("hello")
        a.expect_error()


def lobby_message(port):
    with Client(port) as a, Client(port) as b:
        nick(a, "ann")
        nick(b, "ben")
        a.send("hi there")
        b.expect("ann: hi there")
        a.quiet()


def join_rooms(port):
    with Client(port) as a, Client(port) as b, Client(port) as c:
        nick(a, "j_a")
        nick(b, "j_b")
        nick(c, "j_c")
        join(c, "j_dev")
        a.expect("* j_c left")
        b.expect("* j_c left")
        join(a, "j_dev")
        b.expect("* j_a left")
        c.expect("* j_a joined")
        a.send("in dev")
        c.expect("j_a: in dev")
        b.quiet()
        b.send("in lobby")
        a.quiet()
        c.quiet()


def join_same_room(port):
    with Client(port) as a:
        nick(a, "same")
        join(a, "lobby")
        a.quiet()


def join_needs_nick(port):
    with Client(port) as a:
        a.send("/join somewhere")
        a.expect_error()


def join_invalid_room(port):
    with Client(port) as a:
        nick(a, "r_bad")
        a.send("/join bad-room")
        a.expect_error()


def nick_change(port):
    a, b = room_of(port, "nc_room", "nc_old", "nc_b")
    try:
        nick(a, "nc_new")
        b.expect("* nc_old is now nc_new")
        a.send("hello")
        b.expect("nc_new: hello")
    finally:
        a.close()
        b.close()


def disconnect_announced(port):
    a, b = room_of(port, "dc_room", "dc_a", "dc_b")
    a.close()
    b.expect("* dc_a left")
    b.close()


def quit_command(port):
    a, b = room_of(port, "q_room", "q_a", "q_b")
    a.send("/quit")
    a.closed()
    b.expect("* q_a left")
    a.close()
    b.close()


def unknown_command(port):
    with Client(port) as a:
        a.send("/dance")
        a.expect_error()


def empty_line_ignored(port):
    a, b = room_of(port, "e_room", "e_a", "e_b")
    a.send("")
    b.quiet()
    a.quiet()
    a.send("x")
    b.expect("e_a: x")
    a.close()
    b.close()


def crlf(port):
    a, b = room_of(port, "cr_room", "cr_a", "cr_b")
    a.s.sendall(b"hello\r\n")
    b.expect("cr_a: hello")
    a.close()
    b.close()


def many_clients(port):
    cs = room_of(port, "big", *[f"big{i}" for i in range(30)])
    cs[0].send("to everyone")
    for c in cs[1:]:
        c.expect("big0: to everyone")
    cs[0].quiet()
    for c in cs:
        c.close()


def ordering(port):
    a, b = room_of(port, "o_room", "o_a", "o_b")
    a.s.sendall(b"".join(f"m{i}\n".encode() for i in range(300)))
    for i in range(300):
        b.expect(f"o_a: m{i}")
    a.close()
    b.close()


def unicode_text(port):
    a, b = room_of(port, "u_room", "u_a", "u_b")
    a.send("héllo wörld 日本語 🙂")
    b.expect("u_a: héllo wörld 日本語 🙂")
    a.close()
    b.close()


def split_line(port):
    a, b = room_of(port, "s_room", "s_a", "s_b")
    a.s.sendall(b"hel")
    time.sleep(0.2)
    a.s.sendall(b"lo\n")
    b.expect("s_a: hello")
    a.close()
    b.close()


def slow_reader(port):
    # `stuck` never reads. The others must keep getting messages.
    stuck, a, b = room_of(port, "slow_room", "slow_stuck", "slow_a", "slow_b")
    n = 5000
    got = []

    def read_all():
        try:
            for i in range(n):
                line = b.line(timeout=15)
                if line != f"slow_a: {'x' * 100} {i}":
                    got.append(f"message {i}: got {line!r}")
                    return
            got.append("ok")
        except Exception as e:  # noqa: BLE001
            got.append(f"after {i} messages: {e}")

    reader = threading.Thread(target=read_all)
    reader.start()
    a.s.sendall(b"".join(f"{'x' * 100} {i}\n".encode() for i in range(n)))
    reader.join(30)
    for c in (stuck, a, b):
        c.close()
    if got != ["ok"]:
        raise AssertionError(f"a client that doesn't read held up the others: {got[:1]}")


def idle_timeout(port):
    a, b = room_of(port, "idle_room", "idle_a", "idle_b")
    # `b` keeps talking; `a` says nothing, so the server disconnects it.
    deadline = time.time() + 4
    while time.time() < deadline:
        b.send("still here")
        try:
            line = b.line(timeout=0.3)
        except socket.timeout:
            continue
        if line == "* idle_a left":
            a.closed(timeout=2)
            a.close()
            b.close()
            return
        raise AssertionError(f"expected '* idle_a left', got {line!r}")
    a.close()
    b.close()
    raise AssertionError("a silent client wasn't disconnected after its idle time")


CASES = [
    nick_ok,
    nick_invalid,
    nick_taken,
    message_needs_nick,
    lobby_message,
    join_rooms,
    join_same_room,
    join_needs_nick,
    join_invalid_room,
    nick_change,
    disconnect_announced,
    quit_command,
    unknown_command,
    empty_line_ignored,
    crlf,
    many_clients,
    ordering,
    unicode_text,
    split_line,
    slow_reader,
]


def start(cmd, idle):
    port = free_port()
    p = subprocess.Popen(cmd + [str(port), str(idle)], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
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


def run_cases(cmd, idle, cases):
    """Runs `cases` against one server; returns (passed, lines)."""
    lines = []
    try:
        server, port = start(cmd, idle)
    except RuntimeError as e:
        return 0, [f"FAIL server: {e}"]
    passed = 0
    try:
        for case in cases:
            name = case.__name__.replace("_", "-")
            try:
                case(port)
                passed += 1
                lines.append(f"ok   {name}")
            except Exception as e:  # noqa: BLE001
                lines.append(f"FAIL {name}: {e}")
            if server.poll() is not None:
                lines.append(f"FAIL server exited with status {server.returncode} during {name}")
                break
            # Let the server finish with this case's disconnects.
            time.sleep(0.2)
    finally:
        server.kill()
        server.wait()
    return passed, lines


def arg_errors(cmd):
    out = []
    for name, args in [
        ("no-arguments", []),
        ("one-argument", ["8080"]),
        ("port-not-a-number", ["http", "10"]),
        ("port-too-big", ["70000", "10"]),
        ("idle-zero", ["8080", "0"]),
        ("idle-not-a-number", ["8080", "soon"]),
    ]:
        try:
            p = subprocess.run(cmd + args, capture_output=True, timeout=10)
        except subprocess.TimeoutExpired:
            out.append((name, "still running after 10s"))
            continue
        ok = p.returncode == 1 and p.stderr.strip()
        out.append((name, None if ok else f"exit status {p.returncode}, stderr {p.stderr[:100]!r}"))
    return out


def main():
    cmd = sys.argv[1:]
    if not cmd:
        print(__doc__.strip())
        sys.exit(2)
    if os.sep in cmd[0] and os.path.exists(cmd[0]):
        cmd[0] = os.path.abspath(cmd[0])
    total = len(CASES) + 1 + 6
    passed, lines = run_cases(cmd, 60, CASES)
    p2, lines2 = run_cases(cmd, 1, [idle_timeout])
    passed += p2
    for l in lines + lines2:
        print(l)
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
