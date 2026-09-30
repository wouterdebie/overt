#!/usr/bin/env python3
"""Measures the chat server: memory per idle connection, and message latency.

Usage: python3 tests/perf.py <command...>

Starts `<command> <port> 600`, then:
- opens 10,000 connections that each set a nickname and then stay silent,
  and measures the server's resident memory (`ps -o rss`) before and after
- puts 100 clients in one room and has one of them send 200 messages, one
  at a time, timing each until every other client has it

Prints a JSON line: {"correct": bool, "idle_kib_per_conn": float,
"latency_p50_ms": float, "latency_p99_ms": float, "seconds": p99 latency in
seconds}. "correct" is false if any connection or message went wrong.
"""
import json
import os
import resource
import selectors
import socket
import subprocess
import sys
import time

IDLE_CONNS = 10_000
ROOM = 100
MESSAGES = 200


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def rss_kib(pid):
    out = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    return int(out) if out else 0


def start(cmd):
    port = free_port()
    p = subprocess.Popen(cmd + [str(port), "600"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    deadline = time.time() + 10
    while time.time() < deadline:
        try:
            socket.create_connection(("127.0.0.1", port), timeout=1).close()
            return p, port
        except OSError:
            time.sleep(0.05)
    p.kill()
    raise RuntimeError("the server didn't start")


def read_line(s, buf):
    while b"\n" not in buf[0]:
        chunk = s.recv(65536)
        if not chunk:
            raise RuntimeError("connection closed")
        buf[0] += chunk
    line, buf[0] = buf[0].split(b"\n", 1)
    return line


def idle_memory(port, pid):
    before = rss_kib(pid)
    conns = []
    # In batches of 100, waiting for each batch's "ok"s, so the kernel's
    # accept queue (128 on macOS) never overflows.
    for first in range(0, IDLE_CONNS, 100):
        batch = []
        for i in range(first, min(first + 100, IDLE_CONNS)):
            s = socket.create_connection(("127.0.0.1", port), timeout=10)
            s.sendall(f"/nick idle{i}\n".encode())
            batch.append(s)
        for s in batch:
            s.settimeout(10)
            if read_line(s, [b""]) != b"ok":
                raise RuntimeError("an idle connection didn't get ok")
        conns.extend(batch)
    time.sleep(1)
    after = rss_kib(pid)
    return conns, (after - before) / IDLE_CONNS


def latency(port):
    cs = []
    # One at a time, so each client sees exactly the ones that join after it.
    for i in range(ROOM):
        s = socket.create_connection(("127.0.0.1", port), timeout=10)
        s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        s.sendall(f"/nick lat{i}\n/join latroom\n".encode())
        buf = [b""]
        read_line(s, buf)
        read_line(s, buf)
        cs.append([s, buf])
    # Earlier joiners also see later ones join; read those.
    for idx, (s, buf) in enumerate(cs):
        for _ in range(ROOM - 1 - idx):
            read_line(s, buf)
    sender = cs[0][0]
    times = []
    sel = selectors.DefaultSelector()
    for s, buf in cs[1:]:
        s.setblocking(False)
        sel.register(s, selectors.EVENT_READ, buf)
    for m in range(MESSAGES):
        want = f"lat0: msg {m}".encode()
        start = time.perf_counter()
        sender.sendall(want[len(b"lat0: "):] + b"\n")
        pending = {s for s, _ in cs[1:]}
        while pending:
            for key, _ in sel.select(timeout=5):
                s, buf = key.fileobj, key.data
                if s not in pending:
                    s.recv(65536)
                    continue
                buf[0] += s.recv(65536)
                if b"\n" in buf[0]:
                    line, buf[0] = buf[0].split(b"\n", 1)
                    if line != want:
                        raise RuntimeError(f"got {line!r}, want {want!r}")
                    pending.discard(s)
            if time.perf_counter() - start > 5:
                raise RuntimeError("a message took more than 5s to reach everyone")
        times.append(time.perf_counter() - start)
    for s, _ in cs:
        s.close()
    times.sort()
    return times[len(times) // 2], times[int(len(times) * 0.99) - 1]


def main():
    cmd = sys.argv[1:]
    if not cmd:
        print(__doc__.strip())
        sys.exit(2)
    if os.sep in cmd[0] and os.path.exists(cmd[0]):
        cmd[0] = os.path.abspath(cmd[0])
    soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    want = IDLE_CONNS + ROOM + 100
    if soft < want:
        resource.setrlimit(resource.RLIMIT_NOFILE, (min(want, hard) if hard != resource.RLIM_INFINITY else want, hard))
    server, port = start(cmd)
    result = {"correct": False, "idle_kib_per_conn": None, "latency_p50_ms": None, "latency_p99_ms": None, "seconds": None}
    try:
        conns, per_conn = idle_memory(port, server.pid)
        result["idle_kib_per_conn"] = round(per_conn, 1)
        p50, p99 = latency(port)
        result.update(correct=True, latency_p50_ms=round(p50 * 1000, 3), latency_p99_ms=round(p99 * 1000, 3), seconds=round(p99, 6))
        for s in conns:
            s.close()
    except Exception as e:  # noqa: BLE001
        print(f"error: {e}", file=sys.stderr)
    finally:
        server.kill()
        server.wait()
    print(json.dumps(result))


if __name__ == "__main__":
    main()
