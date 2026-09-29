"""Reference jsonfmt, used by the tests to compute expected output.

format_json(data: bytes) returns (output, None) for valid input, or
(None, (line, column)) with the location of the first byte that can't
continue a valid document.
"""

WS = b" \t\n\r"
HEX = b"0123456789abcdefABCDEF"
DIGITS = b"0123456789"


class Invalid(Exception):
    def __init__(self, pos):
        self.pos = pos


class Parser:
    def __init__(self, data):
        self.d = data
        self.i = 0

    def peek(self):
        return self.d[self.i] if self.i < len(self.d) else None

    def ws(self):
        while self.i < len(self.d) and self.d[self.i] in WS:
            self.i += 1

    def fail(self):
        raise Invalid(self.i)

    def expect(self, byte):
        if self.peek() != byte:
            self.fail()
        self.i += 1

    def value(self, out, depth):
        self.ws()
        c = self.peek()
        if c == ord("{"):
            self.obj(out, depth)
        elif c == ord("["):
            self.arr(out, depth)
        elif c == ord('"'):
            out.append(self.string())
        elif c is not None and (c == ord("-") or c in DIGITS):
            out.append(self.number())
        elif c is not None and c in b"tfn":
            for word in (b"true", b"false", b"null"):
                if word[0] == c:
                    for b in word:
                        self.expect(b)
                    out.append(word)
                    break
        else:
            self.fail()

    def string(self):
        start = self.i
        self.expect(ord('"'))
        while True:
            c = self.peek()
            if c is None or c < 0x20:
                self.fail()
            self.i += 1
            if c == ord('"'):
                return self.d[start:self.i]
            if c == ord("\\"):
                e = self.peek()
                if e is None or e not in b'"\\/bfnrtu':
                    self.fail()
                self.i += 1
                if e == ord("u"):
                    for _ in range(4):
                        h = self.peek()
                        if h is None or h not in HEX:
                            self.fail()
                        self.i += 1

    def digits(self):
        c = self.peek()
        if c is None or c not in DIGITS:
            self.fail()
        while self.peek() is not None and self.peek() in DIGITS:
            self.i += 1

    def number(self):
        start = self.i
        if self.peek() == ord("-"):
            self.i += 1
        c = self.peek()
        if c == ord("0"):
            self.i += 1
        else:
            self.digits()
        if self.peek() == ord("."):
            self.i += 1
            self.digits()
        if self.peek() is not None and self.peek() in b"eE":
            self.i += 1
            if self.peek() is not None and self.peek() in b"+-":
                self.i += 1
            self.digits()
        return self.d[start:self.i]

    def arr(self, out, depth):
        self.expect(ord("["))
        self.ws()
        if self.peek() == ord("]"):
            self.i += 1
            out.append(b"[]")
            return
        pad = b"  " * (depth + 1)
        out.append(b"[\n")
        while True:
            out.append(pad)
            self.value(out, depth + 1)
            self.ws()
            if self.peek() == ord(","):
                self.i += 1
                out.append(b",\n")
                continue
            self.expect(ord("]"))
            out.append(b"\n" + b"  " * depth + b"]")
            return

    def obj(self, out, depth):
        self.expect(ord("{"))
        self.ws()
        if self.peek() == ord("}"):
            self.i += 1
            out.append(b"{}")
            return
        pad = b"  " * (depth + 1)
        out.append(b"{\n")
        while True:
            self.ws()
            if self.peek() != ord('"'):
                self.fail()
            out.append(pad)
            out.append(self.string())
            self.ws()
            self.expect(ord(":"))
            out.append(b": ")
            self.value(out, depth + 1)
            self.ws()
            if self.peek() == ord(","):
                self.i += 1
                out.append(b",\n")
                continue
            self.expect(ord("}"))
            out.append(b"\n" + b"  " * depth + b"}")
            return


def location(data, pos):
    line = data.count(b"\n", 0, pos) + 1
    col = pos - (data.rfind(b"\n", 0, pos) + 1) + 1
    return line, col


def format_json(data):
    p = Parser(data)
    out = []
    try:
        p.value(out, 0)
        p.ws()
        if p.i != len(data):
            p.fail()
    except Invalid as e:
        return None, location(data, e.pos)
    out.append(b"\n")
    return b"".join(out), None
