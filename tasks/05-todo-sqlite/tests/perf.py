#!/usr/bin/env python3
"""Load-tests the todo API on sqlite with oha (https://github.com/hatoo/oha).

Usage: python3 tests/perf.py <command...>

Starts `<command> <port>` in a fresh directory (so todos.db starts empty), creates 1,000 todos (every other one done), warms
up for 2s, and then runs three loads of 10s each over 50 keep-alive
connections:
- list: GET /todos?done=false, which returns 500 todos
- read: GET /todos/<id>, for random ids from 100 to 999
- update: PATCH /todos/<id> with {"done": true}, for the same ids

It samples the server's resident memory (`ps -o rss`) throughout and reads its
CPU time before and after. Prints a JSON line with each load's requests per
second and p99 latency, "peak_rss_mib", "cpu_seconds", "correct" (false if any
response had the wrong status or a request failed) and "seconds" (the read
load's p99 latency, in seconds).
"""
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time

TODOS = 1000
CONNECTIONS = 50
DURATION = "10s"


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def ps(pid, field):
    return subprocess.run(["ps", "-o", f"{field}=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()


def cpu_seconds(pid):
    # [[dd-]hh:]mm:ss.cc
    t = ps(pid, "time")
    days, _, t = t.rpartition("-")
    secs = 0.0
    for part in t.split(":"):
        secs = secs * 60 + float(part)
    return secs + (int(days) * 86400 if days else 0)


def start(cmd, cwd):
    port = free_port()
    p = subprocess.Popen(cmd + [str(port)], cwd=cwd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    deadline = time.time() + 10
    while time.time() < deadline:
        try:
            socket.create_connection(("127.0.0.1", port), timeout=1).close()
            return p, port
        except OSError:
            time.sleep(0.05)
    p.kill()
    raise RuntimeError("the server didn't start")


def seed(port):
    with socket.create_connection(("127.0.0.1", port), timeout=10) as s:
        f = s.makefile("rb")
        for i in range(TODOS):
            body = json.dumps({"title": f"todo number {i + 1}", "done": i % 2 == 1}).encode()
            s.sendall(b"POST /todos HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: %d\r\n\r\n%s" % (len(body), body))
            status = f.readline().split()[1]
            length = 0
            while (line := f.readline().strip()) != b"":
                k, _, v = line.partition(b":")
                if k.strip().lower() == b"content-length":
                    length = int(v)
            f.read(length)
            if status != b"201":
                raise RuntimeError(f"creating todo {i + 1} got status {status.decode()}")


def oha(url, status, duration, method="GET", body=None):
    args = ["oha", "-z", duration, "-c", str(CONNECTIONS), "-w", "--no-tui", "--output-format", "json", "-m", method]
    if body is not None:
        args += ["-d", body, "-T", "application/json"]
    if "[" in url:
        args.append("--rand-regex-url")
    r = subprocess.run(args + [url], capture_output=True, text=True, timeout=120)
    if r.returncode != 0:
        raise RuntimeError(f"oha failed: {r.stderr.strip()[:300]}")
    d = json.loads(r.stdout)
    codes = d.get("statusCodeDistribution") or {}
    ok = set(codes) == {str(status)} and not d.get("errorDistribution")
    if not ok:
        print(f"{method} {url}: statuses {codes}, errors {d.get('errorDistribution')}", file=sys.stderr)
    return ok, round(d["summary"]["requestsPerSec"]), d["latencyPercentiles"]["p99"]


def main():
    cmd = sys.argv[1:]
    if not cmd:
        print(__doc__.strip())
        sys.exit(2)
    if not shutil.which("oha"):
        print("perf.py needs oha: brew install oha, or cargo install oha", file=sys.stderr)
        sys.exit(2)
    if os.sep in cmd[0] and os.path.exists(cmd[0]):
        cmd[0] = os.path.abspath(cmd[0])
    workdir = tempfile.TemporaryDirectory()
    server, port = start(cmd, workdir.name)
    result = {"correct": False, "seconds": None}
    peak = [0]
    sampling = [True]

    def sample():
        while sampling[0]:
            rss = ps(server.pid, "rss")
            if rss:
                peak[0] = max(peak[0], int(rss))
            time.sleep(0.2)

    sampler = threading.Thread(target=sample)
    try:
        seed(port)
        base = f"http://127.0.0.1:{port}"
        ids = base + "/todos/[1-9][0-9][0-9]"
        sampler.start()
        oha(ids, 200, "2s")
        cpu_before = cpu_seconds(server.pid)
        correct = True
        for name, url, status, method, body in [
            ("list", base + "/todos?done=false", 200, "GET", None),
            ("read", ids, 200, "GET", None),
            ("update", ids, 200, "PATCH", '{"done": true}'),
        ]:
            ok, rps, p99 = oha(url, status, DURATION, method, body)
            correct = correct and ok
            result[f"{name}_rps"] = rps
            result[f"{name}_p99_ms"] = round(p99 * 1000, 3)
        result["cpu_seconds"] = round(cpu_seconds(server.pid) - cpu_before, 2)
        result["correct"] = correct
        result["seconds"] = round(result["read_p99_ms"] / 1000, 6)
    except Exception as e:  # noqa: BLE001
        print(f"error: {e}", file=sys.stderr)
    finally:
        sampling[0] = False
        if sampler.is_alive():
            sampler.join()
        result["peak_rss_mib"] = round(peak[0] / 1024, 1)
        server.kill()
        server.wait()
        workdir.cleanup()
    print(json.dumps(result))


if __name__ == "__main__":
    main()
