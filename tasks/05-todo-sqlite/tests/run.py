#!/usr/bin/env python3
"""Tests for the todo API on sqlite.

Usage: python3 tests/run.py <command...>

Runs task 04's tests (tests/api.py, unchanged) against `<command>` in a fresh
directory, then the restart cases below, each in a fresh directory of its own:
the server is killed with SIGKILL and started again, and what it had saved
must still be there. Prints one line per case and ends with "passed X of Y".
"""
import os
import signal
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import api  # noqa: E402

api_path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "api.py")


class Server:
    """The server started in `cwd`; `restart` kills it with SIGKILL and starts it again."""

    def __init__(self, cmd, cwd):
        self.cmd = cmd
        self.cwd = cwd
        self.start()

    def start(self):
        self.port = api.free_port()
        self.p = subprocess.Popen(self.cmd + [str(self.port)], cwd=self.cwd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        deadline = time.time() + 10
        while time.time() < deadline:
            if self.p.poll() is not None:
                raise AssertionError(f"the server exited with status {self.p.returncode}: {self.p.stderr.read().decode()[:300]}")
            try:
                api.socket.create_connection(("127.0.0.1", self.port), timeout=1).close()
                return
            except OSError:
                time.sleep(0.05)
        self.p.kill()
        raise AssertionError("the server didn't start listening within 10s")

    def kill(self):
        if self.p.poll() is None:
            self.p.send_signal(signal.SIGKILL)
        self.p.wait()

    def restart(self):
        self.kill()
        self.start()


def survives_restart(cmd, cwd):
    s = Server(cmd, cwd)
    try:
        with api.Api(s.port) as a:
            t1 = a.create("first")
            t2 = a.create("second", done=True)
            t3 = a.create("  third  ")
            a.status("PATCH", f"/todos/{t1['id']}", {"done": True, "title": "first, done"}, 200)
            a.status("DELETE", f"/todos/{t2['id']}", want=204)
            before = a.list()
        s.restart()
        with api.Api(s.port) as a:
            after = a.list()
            if after != before:
                raise AssertionError(f"after a restart GET /todos is {after!r}, before it was {before!r}")
            api.check_todo(a.get(t1["id"]), title="first, done", done=True)
            api.check_todo(a.get(t3["id"]), title="third", done=False)
            a.status("GET", f"/todos/{t2['id']}", want=404)
            if a.list("?done=true") != [t for t in before if t["done"]]:
                raise AssertionError("?done=true lists something else after a restart")
    finally:
        s.kill()


def ids_after_restart(cmd, cwd):
    s = Server(cmd, cwd)
    try:
        with api.Api(s.port) as a:
            ids = [a.create(f"todo {i}")["id"] for i in range(3)]
            # The newest one is deleted, so its id is free in the table.
            a.status("DELETE", f"/todos/{ids[-1]}", want=204)
        s.restart()
        with api.Api(s.port) as a:
            t = a.create("after the restart")
            if t["id"] != ids[-1] + 1:
                raise AssertionError(f"expected id {ids[-1] + 1} after ids {ids} (the last deleted) and a restart, got {t['id']}")
        s.restart()
        with api.Api(s.port) as a:
            u = a.create("after two restarts")
            if u["id"] != t["id"] + 1:
                raise AssertionError(f"expected id {t['id'] + 1}, got {u['id']}")
    finally:
        s.kill()


def saved_before_response(cmd, cwd):
    s = Server(cmd, cwd)
    try:
        made = []
        with api.Api(s.port) as a:
            for i in range(50):
                made.append(a.create(f"quick {i}"))
            last = a.status("PATCH", f"/todos/{made[-1]['id']}", {"done": True}, 200)
        # Killed at once after the last response: nothing is flushed on exit.
        s.restart()
        with api.Api(s.port) as a:
            todos = a.list()
            if [t["id"] for t in todos] != [t["id"] for t in made]:
                raise AssertionError(f"expected {len(made)} todos after being killed, got {len(todos)}")
            if todos[-1] != last:
                raise AssertionError(f"the last change was lost: {todos[-1]!r}, want {last!r}")
    finally:
        s.kill()


def many_clients_then_restart(cmd, cwd):
    s = Server(cmd, cwd)
    try:
        errors = []
        made = []

        def client(i):
            try:
                with api.Api(s.port) as a:
                    for j in range(10):
                        made.append(a.create(f"client {i} todo {j}", done=j % 2 == 0))
            except Exception as e:  # noqa: BLE001
                errors.append(f"client {i}: {e}")

        threads = [api.threading.Thread(target=client, args=(i,)) for i in range(20)]
        for t in threads:
            t.start()
        for t in threads:
            t.join(30)
        if errors:
            raise AssertionError(errors[0])
        s.restart()
        with api.Api(s.port) as a:
            todos = a.list()
            if sorted(todos, key=lambda t: t["id"]) != sorted(made, key=lambda t: t["id"]):
                raise AssertionError(f"expected the {len(made)} todos made by 20 clients after a restart, got {len(todos)}")
    finally:
        s.kill()


def uses_sqlite(cmd, cwd):
    s = Server(cmd, cwd)
    try:
        with api.Api(s.port) as a:
            a.create("in the database")
    finally:
        s.kill()
    path = os.path.join(cwd, "todos.db")
    if not os.path.exists(path):
        raise AssertionError("there's no todos.db in the current directory")
    with open(path, "rb") as f:
        if f.read(16) != b"SQLite format 3\x00":
            raise AssertionError("todos.db isn't a sqlite database")


CASES = [survives_restart, ids_after_restart, saved_before_response, many_clients_then_restart, uses_sqlite]


def main():
    cmd = sys.argv[1:]
    if not cmd:
        print(__doc__.strip())
        sys.exit(2)
    if os.sep in cmd[0] and os.path.exists(cmd[0]):
        cmd[0] = os.path.abspath(cmd[0])
    passed = 0
    total = 0
    with tempfile.TemporaryDirectory() as d:
        r = subprocess.run([sys.executable, api_path] + cmd, cwd=d, capture_output=True, text=True, timeout=600)
        for line in r.stdout.splitlines():
            if line.startswith("passed "):
                p, t = line.split()[1], line.split()[3]
                passed += int(p)
                total += int(t)
            else:
                print(line)
        if not r.stdout:
            print(f"FAIL api: {r.stderr[:300]}")
    for case in CASES:
        total += 1
        name = case.__name__.replace("_", "-")
        with tempfile.TemporaryDirectory() as d:
            try:
                case(cmd, d)
                passed += 1
                print(f"ok   {name}")
            except Exception as e:  # noqa: BLE001
                print(f"FAIL {name}: {e}")
    print(f"passed {passed} of {total}")
    sys.exit(0 if passed == total else 1)


if __name__ == "__main__":
    main()
