#!/usr/bin/env python3
"""Black-box tests for the todo API.

Usage: python3 tests/run.py <command...>

Starts `<command> <port>` on a free port and runs each case against it in
order, speaking HTTP over plain sockets. The first cases expect a fresh
server; the rest only look at the todos they create. The argument errors run
the command on its own. Prints one line per case and ends with
"passed X of Y".
"""
import json
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


class Closed(Exception):
    pass


class Resp:
    def __init__(self, status, headers, body):
        self.status = status
        self.headers = headers
        self.body = body

    def json(self):
        ctype = self.headers.get("content-type", "")
        if not ctype.startswith("application/json"):
            raise AssertionError(f"status {self.status}: Content-Type is {ctype!r}, want application/json")
        try:
            return json.loads(self.body.decode("utf-8"))
        except (UnicodeDecodeError, ValueError) as e:
            raise AssertionError(f"status {self.status}: the body isn't JSON ({e}): {self.body[:100]!r}")

    def __repr__(self):
        return f"{self.status} {self.body[:100]!r}"


class Conn:
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

    def send(self, data):
        self.s.sendall(data)

    def fill(self):
        try:
            chunk = self.s.recv(65536)
        except ConnectionResetError:
            chunk = b""
        except socket.timeout:
            raise AssertionError(f"no response within {TIMEOUT}s")
        if not chunk:
            raise Closed()
        self.buf += chunk

    def line(self):
        while b"\r\n" not in self.buf:
            self.fill()
        line, self.buf = self.buf.split(b"\r\n", 1)
        return line

    def response(self):
        """The next response. Handles Content-Length, chunked and read-to-close bodies."""
        try:
            while b"\r\n\r\n" not in self.buf:
                if b"\n\n" in self.buf:
                    raise AssertionError("response header lines must end with \\r\\n")
                self.fill()
        except Closed:
            raise AssertionError(f"the server closed the connection instead of responding (got {self.buf[:100]!r})")
        head, self.buf = self.buf.split(b"\r\n\r\n", 1)
        lines = head.decode("latin-1").split("\r\n")
        parts = lines[0].split(" ", 2)
        if len(parts) < 2 or not parts[0].startswith("HTTP/1.") or not parts[1].isdigit():
            raise AssertionError(f"bad status line {lines[0]!r}")
        status = int(parts[1])
        headers = {}
        for l in lines[1:]:
            k, sep, v = l.partition(":")
            if not sep:
                raise AssertionError(f"bad header line {l!r}")
            headers[k.strip().lower()] = v.strip()
        if status == 204:
            if headers.get("content-length", "0") != "0" or "transfer-encoding" in headers:
                raise AssertionError("a 204 response must not have a body")
            return Resp(status, headers, b"")
        if "chunked" in headers.get("transfer-encoding", "").lower():
            body = b""
            while True:
                size = int(self.line().split(b";")[0], 16)
                if size == 0:
                    while self.line():
                        pass
                    break
                while len(self.buf) < size + 2:
                    self.fill()
                body, self.buf = body + self.buf[:size], self.buf[size + 2 :]
            return Resp(status, headers, body)
        if "content-length" in headers:
            n = int(headers["content-length"])
            while len(self.buf) < n:
                try:
                    self.fill()
                except Closed:
                    raise AssertionError(f"the connection closed after {len(self.buf)} of {n} body bytes")
            body, self.buf = self.buf[:n], self.buf[n:]
            return Resp(status, headers, body)
        # No length: the body runs until the server closes the connection.
        try:
            while True:
                self.fill()
        except Closed:
            pass
        body, self.buf = self.buf, b""
        return Resp(status, headers, body)

    def request(self, method, path, body=None, headers=(), version="HTTP/1.1"):
        self.send(encode(method, path, body, headers, version))
        return self.response()

    def closed(self):
        """True if the server closes the connection within TIMEOUT without sending more."""
        self.s.settimeout(TIMEOUT)
        try:
            data = self.s.recv(100)
        except ConnectionResetError:
            return True
        except socket.timeout:
            return False
        if data:
            raise AssertionError(f"expected the connection to close, got {data[:60]!r}")
        return True


def encode(method, path, body=None, headers=(), version="HTTP/1.1"):
    if isinstance(body, (dict, list)):
        body = json.dumps(body)
    if isinstance(body, str):
        body = body.encode()
    lines = [f"{method} {path} {version}", "Host: 127.0.0.1"]
    names = {k.lower() for k, _ in headers}
    lines += [f"{k}: {v}" for k, v in headers]
    if body is not None and "content-length" not in names:
        lines.append(f"Content-Length: {len(body)}")
        if "content-type" not in names:
            lines.append("Content-Type: application/json")
    return ("\r\n".join(lines) + "\r\n\r\n").encode() + (body or b"")


def check(resp, status):
    """Checks the status, and that the body is what that status needs; returns the parsed body."""
    if resp.status != status:
        raise AssertionError(f"expected status {status}, got {resp!r}")
    if status == 204:
        return None
    data = resp.json()
    if status >= 400:
        if not isinstance(data, dict) or not isinstance(data.get("error"), str):
            raise AssertionError(f'status {status}: expected {{"error": "..."}}, got {data!r}')
    return data


def check_todo(data, id=None, title=None, done=None):
    if not isinstance(data, dict) or set(data) != {"id", "title", "done"}:
        raise AssertionError(f"expected a todo with exactly id, title and done, got {data!r}")
    if not isinstance(data["id"], int) or isinstance(data["id"], bool) or data["id"] < 1:
        raise AssertionError(f"expected a positive integer id, got {data!r}")
    if not isinstance(data["title"], str) or not isinstance(data["done"], bool):
        raise AssertionError(f"expected a string title and a boolean done, got {data!r}")
    for k, want in [("id", id), ("title", title), ("done", done)]:
        if want is not None and data[k] != want:
            raise AssertionError(f"expected {k} {want!r}, got {data!r}")
    return data


class Api:
    """One keep-alive connection, with helpers that check each response."""

    def __init__(self, port):
        self.c = Conn(port)

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.c.close()

    def create(self, title, done=None):
        body = {"title": title} if done is None else {"title": title, "done": done}
        want = title.strip(" ")
        return check_todo(check(self.c.request("POST", "/todos", body), 201), title=want, done=bool(done))

    def get(self, id):
        return check_todo(check(self.c.request("GET", f"/todos/{id}"), 200), id=id)

    def list(self, query=""):
        data = check(self.c.request("GET", "/todos" + query), 200)
        if not isinstance(data, list):
            raise AssertionError(f"expected an array, got {data!r}")
        for t in data:
            check_todo(t)
        ids = [t["id"] for t in data]
        if ids != sorted(set(ids)):
            raise AssertionError(f"expected the todos in id order, got ids {ids[:20]}")
        return data

    def status(self, method, path, body=None, want=400, headers=()):
        return check(self.c.request(method, path, body, headers), want)


def contains(todos, todo):
    return any(t == todo for t in todos)


# Each case takes the port and raises AssertionError on failure.


def empty_list(port):
    with Api(port) as a:
        if a.list() != []:
            raise AssertionError("a fresh server must have no todos")


def create(port):
    with Api(port) as a:
        a.create("buy milk")
        t = a.get(1)
        check_todo(t, id=1, title="buy milk", done=False)


def create_done(port):
    with Api(port) as a:
        check_todo(a.create("walk the dog", done=True), id=2)


def ids_count_up(port):
    with Api(port) as a:
        ids = [a.create(f"count {i}")["id"] for i in range(5)]
        if ids != list(range(ids[0], ids[0] + 5)):
            raise AssertionError(f"expected consecutive ids, got {ids}")


def get_one(port):
    with Api(port) as a:
        t = a.create("read a book", done=True)
        if a.get(t["id"]) != t:
            raise AssertionError(f"GET returned something other than {t!r}")


def list_all(port):
    with Api(port) as a:
        made = [a.create(f"list {i}", done=i % 2 == 0) for i in range(4)]
        todos = a.list()
        for t in made:
            if not contains(todos, t):
                raise AssertionError(f"{t!r} is missing from GET /todos")


def list_filter(port):
    with Api(port) as a:
        yes = a.create("filter done", done=True)
        no = a.create("filter not done")
        done = a.list("?done=true")
        if not contains(done, yes) or contains(done, no) or not all(t["done"] for t in done):
            raise AssertionError("?done=true must list exactly the done todos")
        open_ = a.list("?done=false")
        if not contains(open_, no) or contains(open_, yes) or any(t["done"] for t in open_):
            raise AssertionError("?done=false must list exactly the todos that aren't done")


def list_bad_query(port):
    with Api(port) as a:
        for q in ["?done=maybe", "?done=1", "?done=", "?done=TRUE", "?finished=true"]:
            a.status("GET", "/todos" + q)


def patch_fields(port):
    with Api(port) as a:
        t = a.create("patch me")
        id = t["id"]
        check_todo(a.status("PATCH", f"/todos/{id}", {"title": "patched"}, 200), id=id, title="patched", done=False)
        check_todo(a.status("PATCH", f"/todos/{id}", {"done": True}, 200), id=id, title="patched", done=True)
        check_todo(a.status("PATCH", f"/todos/{id}", {"title": "  both  ", "done": False}, 200), id=id, title="both", done=False)
        check_todo(a.status("PATCH", f"/todos/{id}", {}, 200), id=id, title="both", done=False)
        check_todo(a.get(id), title="both", done=False)


def patch_invalid(port):
    with Api(port) as a:
        t = a.create("stays the same")
        for body in [
            {"title": ""},
            {"title": "   "},
            {"title": 5},
            {"title": "x" * 201},
            {"done": "yes"},
            {"done": 1},
            {"title": "changed", "done": "no"},
            {"title": 1, "done": True},
            [],
            "just a string",
            "not json",
            "",
        ]:
            a.status("PATCH", f"/todos/{t['id']}", body)
        if a.get(t["id"]) != t:
            raise AssertionError("a PATCH that failed changed the todo")


def null_fields(port):
    with Api(port) as a:
        t = a.create("nulls", done=True)
        check_todo(a.status("PATCH", f"/todos/{t['id']}", {"title": None, "done": None}, 200), title="nulls", done=True)
        check_todo(check(a.c.request("POST", "/todos", {"title": "done is null", "done": None}), 201), title="done is null", done=False)
        a.status("POST", "/todos", {"title": None, "done": True})


def patch_missing(port):
    with Api(port) as a:
        a.status("PATCH", "/todos/987654", {"done": True}, 404)


def delete(port):
    with Api(port) as a:
        t = a.create("delete me")
        path = f"/todos/{t['id']}"
        a.status("DELETE", path, want=204)
        a.status("GET", path, want=404)
        a.status("DELETE", path, want=404)
        a.status("PATCH", path, {"done": True}, 404)
        if contains(a.list(), t):
            raise AssertionError("a deleted todo is still in GET /todos")


def ids_not_reused(port):
    with Api(port) as a:
        t = a.create("short-lived")
        a.status("DELETE", f"/todos/{t['id']}", want=204)
        u = a.create("next")
        if u["id"] != t["id"] + 1:
            raise AssertionError(f"expected id {t['id'] + 1} after deleting {t['id']}, got {u['id']}")


def create_invalid(port):
    with Api(port) as a:
        before = a.create("before the bad ones")
        for body in [
            {},
            {"done": True},
            {"title": ""},
            {"title": "  "},
            {"title": 7},
            {"title": ["x"]},
            {"title": None},
            {"title": "x", "done": "true"},
            {"title": "x", "done": 0},
            [],
            [{"title": "x"}],
            '"title"',
            "null",
            "42",
            "not json",
            '{"title": "x"',
            '{"title": "x",}',
            "{'title': 'x'}",
            '{"title": "x"} extra',
            '{"title" "x"}',
            "",
            None,
        ]:
            a.status("POST", "/todos", body)
        after = a.create("after the bad ones")
        if after["id"] != before["id"] + 1:
            raise AssertionError(f"failed POSTs used up ids: {before['id']} then {after['id']}")


def title_limits(port):
    with Api(port) as a:
        for title in ["x" * 200, "日" * 200, "🙂" * 200, "  " + "y" * 200 + "  ", "a"]:
            a.create(title)
        for title in ["x" * 201, "日" * 201, "🙂" * 201]:
            a.status("POST", "/todos", {"title": title})


def titles_trimmed(port):
    with Api(port) as a:
        t = a.create("   buy bread  ")
        check_todo(a.get(t["id"]), title="buy bread")
        a.create("inner  spaces stay")


def extra_fields(port):
    with Api(port) as a:
        body = '{"title": "extra", "priority": 3.5e2, "tags": ["a", {"b": null}], "n": -0.5E-3, "t": true, "f": false, "o": {}, "done": true}'
        check_todo(check(a.c.request("POST", "/todos", body), 201), title="extra", done=True)


def json_strings(port):
    with Api(port) as a:
        raw = r'"quote \" backslash \\ slash \/ tab\tnewline\n é 🙂 \u0001\u001f caf' + "é 日本" + '"'
        want = json.loads(raw)
        body = '{"title": ' + raw + "}"
        t = check_todo(check(a.c.request("POST", "/todos", body), 201), title=want)
        check_todo(a.get(t["id"]), title=want)


def json_whitespace(port):
    with Api(port) as a:
        body = '\r\n {\n\t"title" :\t"spaced" ,  "done" : true }\n '
        check_todo(check(a.c.request("POST", "/todos", body), 201), title="spaced", done=True)


def method_not_allowed(port):
    with Api(port) as a:
        t = a.create("methods")
        for method, path in [("PUT", "/todos"), ("DELETE", "/todos"), ("PATCH", "/todos"), ("POST", f"/todos/{t['id']}"), ("PUT", f"/todos/{t['id']}")]:
            a.status(method, path, {"title": "x"}, 405)


def missing_ids(port):
    with Api(port) as a:
        for id in ["999999", "0", "-1", "abc", "1.5", "99999999999999999999999"]:
            a.status("GET", f"/todos/{id}", want=404)
            a.status("DELETE", f"/todos/{id}", want=404)


def unknown_paths(port):
    with Api(port) as a:
        for method, path in [("GET", "/"), ("GET", "/todo"), ("GET", "/todos/1/extra"), ("GET", "/nothing"), ("DELETE", "/nothing"), ("POST", "/items")]:
            a.status(method, path, {"title": "x"} if method == "POST" else None, 404)


def keep_alive(port):
    with Api(port) as a:
        for i in range(30):
            t = a.create(f"keep {i}")
            a.get(t["id"])
            a.status("PATCH", f"/todos/{t['id']}", {"done": True}, 200)
            a.status("DELETE", f"/todos/{t['id']}", want=204)
            a.status("GET", f"/todos/{t['id']}", want=404)
            a.status("POST", "/todos", "bad")


def pipelining(port):
    with Conn(port) as c:
        c.send(encode("POST", "/todos", {"title": "piped"}))
        t = check_todo(check(c.response(), 201), title="piped")
        path = f"/todos/{t['id']}"
        c.send(
            encode("GET", path)
            + encode("PATCH", path, {"done": True})
            + encode("DELETE", path)
            + encode("GET", path)
            + encode("POST", "/todos", "{")
        )
        check_todo(check(c.response(), 200), done=False)
        check_todo(check(c.response(), 200), done=True)
        check(c.response(), 204)
        check(c.response(), 404)
        check(c.response(), 400)


def split_request(port):
    with Conn(port) as c:
        req = encode("POST", "/todos", {"title": "arrives in pieces", "done": True})
        head_end = req.index(b"\r\n\r\n")
        for piece in [req[:10], req[10:head_end], req[head_end : head_end + 3], req[head_end + 3 : head_end + 12], req[head_end + 12 :]]:
            c.send(piece)
            time.sleep(0.1)
        check_todo(check(c.response(), 201), title="arrives in pieces", done=True)


def header_case(port):
    with Conn(port) as c:
        body = b'{"title": "odd headers"}'
        c.send(
            b"POST /todos HTTP/1.1\r\nhost: 127.0.0.1\r\nuser-agent: tests/run.py\r\nACCEPT: */*\r\n"
            + b"X-Long: " + b"v" * 4000 + b"\r\ncontent-type: application/json\r\ncontent-LENGTH: "
            + str(len(body)).encode() + b"\r\n\r\n" + body
        )
        check_todo(check(c.response(), 201), title="odd headers")


def large_body(port):
    with Api(port) as a:
        body = json.dumps({"filler": "z" * 256 * 1024, "title": "after the filler"})
        check_todo(check(a.c.request("POST", "/todos", body), 201), title="after the filler")


def connection_close(port):
    with Conn(port) as c:
        check(c.request("GET", "/todos", headers=[("Connection", "close")]), 200)
        if not c.closed():
            raise AssertionError("the connection stayed open after Connection: close")


def http10(port):
    with Conn(port) as c:
        check(c.request("GET", "/todos", version="HTTP/1.0"), 200)
        if not c.closed():
            raise AssertionError("an HTTP/1.0 connection without keep-alive stayed open")
    with Conn(port) as c:
        r = c.request("GET", "/todos", headers=[("Connection", "keep-alive")], version="HTTP/1.0")
        check(r, 200)
        if r.headers.get("connection", "").lower() != "keep-alive":
            raise AssertionError(f"an HTTP/1.0 keep-alive response needs Connection: keep-alive, got {r.headers.get('connection')!r}")
        check(c.request("GET", "/todos", headers=[("Connection", "keep-alive")], version="HTTP/1.0"), 200)


def bad_requests(port):
    for raw in [
        b"GARBAGE\r\n\r\n",
        b"GET /todos\r\n\r\n",
        b"GET /todos HTTP/1.1\r\nno colon here\r\n\r\n",
        b"POST /todos HTTP/1.1\r\nContent-Length: lots\r\n\r\n{}",
    ]:
        with Conn(port) as c:
            c.send(raw)
            try:
                r = c.response()
            except AssertionError:
                continue  # closing the connection is fine
            if r.status != 400:
                raise AssertionError(f"{raw[:30]!r}: expected 400 or a closed connection, got {r!r}")
    with Api(port) as a:
        a.create("still working")


def concurrent_clients(port):
    errors = []
    made = []

    def client(i):
        try:
            with Api(port) as a:
                for j in range(20):
                    t = a.create(f"client {i} todo {j}")
                    a.get(t["id"])
                    made.append(t)
        except Exception as e:  # noqa: BLE001
            errors.append(f"client {i}: {e}")

    threads = [threading.Thread(target=client, args=(i,)) for i in range(25)]
    for t in threads:
        t.start()
    for t in threads:
        t.join(TIMEOUT * 4)
    if errors:
        raise AssertionError(errors[0])
    ids = [t["id"] for t in made]
    if len(set(ids)) != len(ids):
        raise AssertionError("two clients' todos got the same id")
    with Api(port) as a:
        todos = a.list()
        missing = [t for t in made if not contains(todos, t)]
        if missing:
            raise AssertionError(f"{len(missing)} todos are missing from GET /todos, like {missing[0]!r}")


def many_connections(port):
    conns = []
    try:
        for i in range(100):
            conns.append(Api(port))
        for i, a in enumerate(conns):
            a.list("?done=true")
    finally:
        for a in conns:
            a.c.close()


def slow_client(port):
    with Conn(port) as stalled:
        req = encode("POST", "/todos", {"title": "slow"})
        stalled.send(req[:-5])
        with Api(port) as a:
            a.create("not held up")
        stalled.send(req[-5:])
        check_todo(check(stalled.response(), 201), title="slow")


CASES = [
    empty_list,
    create,
    create_done,
    ids_count_up,
    get_one,
    list_all,
    list_filter,
    list_bad_query,
    patch_fields,
    patch_invalid,
    null_fields,
    patch_missing,
    delete,
    ids_not_reused,
    create_invalid,
    title_limits,
    titles_trimmed,
    extra_fields,
    json_strings,
    json_whitespace,
    method_not_allowed,
    missing_ids,
    unknown_paths,
    keep_alive,
    pipelining,
    split_request,
    header_case,
    large_body,
    connection_close,
    http10,
    bad_requests,
    concurrent_clients,
    many_connections,
    slow_client,
]


def start(cmd):
    port = free_port()
    p = subprocess.Popen(cmd + [str(port)], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
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
    """(name, problem or None) for arguments that must fail with status 1 and a message on stderr."""
    busy = socket.socket()
    busy.bind(("127.0.0.1", 0))
    busy.listen()
    out = []
    try:
        for name, args in [
            ("no-arguments", []),
            ("port-not-a-number", ["http"]),
            ("port-zero", ["0"]),
            ("port-too-big", ["70000"]),
            ("extra-argument", ["8080", "x"]),
            ("port-in-use", [str(busy.getsockname()[1])]),
        ]:
            try:
                p = subprocess.run(cmd + args, capture_output=True, timeout=10)
            except subprocess.TimeoutExpired:
                out.append((name, "still running after 10s"))
                continue
            ok = p.returncode == 1 and p.stderr.strip()
            out.append((name, None if ok else f"exit status {p.returncode}, stderr {p.stderr[:100]!r}"))
    finally:
        busy.close()
    return out


def main():
    cmd = sys.argv[1:]
    if not cmd:
        print(__doc__.strip())
        sys.exit(2)
    if os.sep in cmd[0] and os.path.exists(cmd[0]):
        cmd[0] = os.path.abspath(cmd[0])
    passed = 0
    total = len(CASES) + 6
    try:
        server, port = start(cmd)
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
