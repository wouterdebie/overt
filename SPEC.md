# Overt v0

Overt is a compiled language that coding agents write and read. Anything that affects behavior is visible where it happens: effects are in signatures, mutation is marked at call sites, shared state shows in types. It compiles to native code through LLVM and calls C directly.

When unsure about a standard library API, run `ovt outline <module>` instead of guessing.

## A complete program

```
// src/main.ovt: counts hits per key over HTTP.
type Store { hits: Map[str, int] }
type HitCount { key: str, hits: int }

fn main() ! io, fail {
  let store = Shared.new(Store(hits: {}))
  http.serve(":8080", |req| route(req, store))?
}

// POST /hit/<key> adds a hit; GET /hit/<key> reads the count.
fn route(req: http.Req, store: Shared[Store]) -> http.Resp ! fail {
  return match (req.method, req.path_parts()) {
    ("POST", ["hit", key]) => http.json(200, HitCount(key, hits: hit(store, key)))
    ("GET", ["hit", key]) => http.json(200, HitCount(key, hits: count(store, key)?))
    _ => http.text(404, "not found")
  }
}

fn hit(store: Shared[Store], key: str) -> int {
  return lock store as s {
    s.hits[key] = (s.hits.get(key) else 0) + 1
    s.hits[key]
  }
}

fn count(store: Shared[Store], key: str) -> int ! fail {
  let n = lock store as s { s.hits.get(key) }
  return n else fail(.NotFound, "no hits for ${key}")
}

test "hits count per key" ! fail {
  let s = Shared.new(Store(hits: {}))
  assert(hit(s, "a") == 1)
  assert(hit(s, "a") == 2)
  assert(count(s, "a")? == 2)
}
```

## Files and names

- A package is a directory with `src/`. Each `.ovt` file is a module named by its path: `src/api/users.ovt` is `api.users`. The program starts at `fn main() ! io, fail` in `src/main.ovt`.
- There are no imports. Use any module by name (`users.find(id)`, `fs.read(path)`); inside a module, use bare names. Top-level declarations may appear in any order.
- The top level holds only `const`, `type`, `enum`, `fn`, `extern`, `drop` and `test`. There are no global variables.
- Naming is enforced: `snake_case` for variables, functions and modules, `PascalCase` for types and variants, `UPPER_CASE` for constants. Names inside `extern` blocks keep their C spelling.
- No shadowing: a name can't be declared while another declaration of it is in scope (sibling blocks and `match` arms may reuse names), and a local can't reuse a module name.
- Newlines end statements; there are no semicolons. A statement continues onto the next line when the line ends with an operator, `,` or an open bracket, or when the next line starts with `.`, `else` or `catch`.
- `//` starts a comment. Comment lines directly above a declaration are its documentation.
- Keywords: `fn type enum const let var if else match for in while break continue return inout sink self par lock as extern header blocking unsafe drop pre ex fails test catch none true false`. `simd` is reserved.

## Types

| Type | Meaning |
|---|---|
| `int` | 64-bit signed integer, the default |
| `i32 i16 i8 u64 u32 u16 u8` | sized integers |
| `f64 f32`, `bool`, `Dur` | floats, booleans, durations |
| `str` | UTF-8 text. `s[i]` is a byte (`u8`); `s[a..b]` is a substring by byte offsets |
| `[T]`, `[T; N]` | growable array; fixed-size inline array |
| `Map[K, V]`, `Set[T]` | hash map and set; both keep insertion order |
| `(A, B)` | tuple: `t.0`, `let (a, b) = t` |
| `?T` | optional: `none` or a `T` |
| `fn(A, B) -> R ! io` | function type |

- Literals: `1_000`, `0xff`, `1.5`, `'a'` (a `u8`), `true`, `none`, `"a ${expr} b"`, `"""raw "quotes" and ${expr}"""` (may span lines), `[1, 2]`, `{"k": 1}` (map), `{1, 2}` (set), `{}` (empty map or set), `250ms` `5s` `2m` `1h` (`Dur`).
- A number literal takes the type its context expects (`let b: u8 = 7`; `1` is fine for an `f64`), otherwise `int` or `f64`.
- In strings only `${` is special (`\$` escapes it), so JSON needs no brace escaping: `"""{"id": ${id}}"""`.
- There are no implicit conversions, except that a `T` is accepted where a `?T` is expected. Both sides of an operator have the same type. Convert with `int(x)`, `u8(x)`, `f64(x)` (these trap if the value doesn't fit); parse with `int.parse(s)`.
- Integer `+ - *` trap on overflow; `+% -% *%` wrap. `/` and `%` trap on zero. Indexing out of range traps. A trap stops the program with a message and its source location.
- Operators: `+ - * / %`, `== != < <= > >=`, `&& || !`, `& | ^ << >>`. `+` also joins strings. There is no operator or function overloading.

## Declarations

```
const MAX_BODY = 1_000_000

type Point { x: f64, y: f64 }
type Config { host: str = "0.0.0.0", port: int = 8080 }   // defaulted fields may be omitted
type UserId = int                                          // alias

enum Shape {
  Circle(r: f64)
  Rect(w: f64, h: f64)
  Empty
}

type Page[T] { items: [T], next: ?str }
```

- Construction names every field: `Point(x: 1.0, y: 2.0)`, `Config(port: 9000)`. A bare variable whose name matches a field counts as named: `Point(x, y)`.
- Write a variant as `Shape.Circle(r: 1.0)`, or as `.Circle(r: 1.0)` where the type is known (patterns, arguments, return values, comparisons).
- Recursive types need no annotation.
- Every type automatically gets `==`, hashing, printing in `"${v}"`, and JSON (`json.encode(v)`, `json.decode[T](s)`) when its fields support them. `<` works on numbers, `str`, tuples and arrays.

## Functions

```
// Parses "key = value".
fn parse(line: str) -> Entry ! fail
  pre line.len() < 4096
  ex parse("a = b") == Entry(key: "a", val: "b")
  ex parse("ab") fails .Invalid
{
  let i = line.find("=") else fail(.Invalid, "no '=' in ${line}")
  return Entry(key: line[..i].trim(), val: line[i + 1..].trim())
}
```

- The signature is the whole contract: parameters, return type, and effects after `!`. Without `->` a function returns nothing. Types are inferred only inside bodies.
- Return values with `return`. `if`, `match`, `else`, `catch` and `lock` blocks used as values yield their last line.
- `pre cond` traps on entry if false. `ex` lines are examples that `ovt test` runs: `ex <bool expr>`, `ex <call> fails`, `ex <call> fails .Kind`. They may only call pure functions.
- Parameters may have defaults: `fn get(url: str, retries: int = 3)`.
- Named arguments: inside the parentheses, when two or more arguments go to parameters of the same type, name all but the first of them: `copy(a, dst: b)`, `s.replace("a", new: "b")`. Any argument may be named. Named arguments come after positional ones, in any order.
- A call's result must be used: `s.trim()` alone on a line is an error. Discard with `_ = f()`.
- Closures: `|x| x * 2`, or a block body `|x| { ... }` that uses `return`. They capture copies of the variables they use and can't assign to outer variables.
- Methods are functions named `Type.name`, declared in the type's module. With `self` first they're called as `v.name(...)`; without it they're static (`Stack.new()`).

```
type Stack[T] { items: [T] }
fn Stack[T].new() -> Stack[T] { return Stack(items: []) }
fn Stack[T].push(inout self, v: T) { self.items.push(v) }
fn Stack[T].top(self) -> ?T { return self.items.last() }
```

- Generics: `fn max[T: Ord](a: T, b: T) -> T`. The constraints are `Eq`, `Ord` and `Hash`. Explicit type arguments: `json.decode[User](body)`. There are no user-defined interfaces; use enums or structs of functions.

## Values and mutation

Everything is a value: assigning or passing one gives the receiver its own copy. Copies are cheap because heap data is shared until one side writes to it. There are no references, no null and no lifetimes: after `var b = a` and `b.push(1)`, `a` is unchanged.

| Kind | Examples | Copying |
|---|---|---|
| plain | numbers, `bool`, structs of plain fields | copies the bytes |
| heap-backed | `str`, `[T]`, `Map`, `Set` | shares the data until a write |
| resource | `net.Conn`, `fs.File`, C handles | not allowed; values move |
| shared handle | `Shared[T]`, `Atomic[T]`, `Chan[T]` | both copies refer to the same object |

- `let` bindings can't change; `var` bindings can. Parameters are read-only unless marked.

| Declared | The callee may | Call site |
|---|---|---|
| `x: T` | read it | `f(v)` |
| `x: inout T` | change the caller's variable | `f(inout v)`, where `v` is a `var` or a field or element of one: `inout p.x`, `inout xs[i]` |
| `x: sink T` | keep it (store it, send it) | `f(v)`; moved if this is `v`'s last use, otherwise copied |

- Methods take `self`, `inout self` or `sink self`. Calling an `inout self` method needs a `var` receiver and no marker: `stack.push(1)`.
- A variable passed as `inout` can't appear anywhere else in the same call.
- Change values in place: `p.x = 1`, `xs[i] += 1`, `m[k] = v`, `for inout x in xs { x += 1 }`.
- `drop T { ... }` makes `T` a resource: it can only be moved, and the block runs when the value is destroyed (`self` names it). A type containing a resource is a resource.

## Control flow

```
if a { ... } else if b { ... } else { ... }
let size = if big { 10 } else { 1 }
while cond { ... }
for x in xs { ... }       for i, x in xs { ... }       for k, v in m { ... }
for i in 0..n { ... }     for i in 0..=n { ... }       break    continue
if let v = opt { ... } else { ... }
while let line = next_line() { ... }
```

`match` must be exhaustive, with one `pattern => value` arm per line (see `route` in the program at the top).

Patterns: literals, `_`, names (these bind), ranges (`'a'..='z'`), tuples, arrays (`[a, b]`, `[first, ..rest]`), variants (`.Rect(w, h)` binds fields by name, `.Rect(w: width)` renames, unlisted fields are ignored), alternatives (`p | q`), and guards (`pat if cond => ...`).

## Optionals and failure

- `x else alt` gives `alt` when the optional `x` is `none`. `alt` can be a value, a block, or something that leaves: `return`, `break`, `continue`, `fail(...)`, `trap(...)`. `else` binds loosest: `(m.get(k) else 0) + 1`.
- Failure is an effect, not a return type. A function that can fail declares `! fail` and fails with `fail(kind, msg)`. The error value is `Err { kind: ErrKind, msg: str }`, and `ErrKind` is one of `.Invalid .NotFound .Denied .Conflict .Timeout .Cancelled .Unavailable .Io .Internal`.
- Every call to a function that can fail must handle the failure:

```
let n = int.parse(s)?               // pass it on; the caller declares `! fail`
let n = int.parse(s) else 0         // replace it
let n = int.parse(s) catch e {      // inspect it; e is an Err
  log.warn("bad count: ${e.msg}")
  0
}
```

- When a call can fail and also returns `?T`, `else` handles the failure; `f()? else x` handles the `none`.
- `trap(msg)`, `assert(cond)`, `assert(cond, msg)` and `todo()` stop the program. They're for bugs, not expected errors.
- `dbg(x)` prints `x` to stderr and returns it. It's allowed in pure code, and its result may be ignored.

## Effects

- `io` means the function touches the outside world: files, network, clock, randomness, environment, printing, sleeping. `fail` means it can fail.
- Declare effects after the return type: `-> str ! io, fail`. No `!` means pure.
- A function can only call functions whose effects it also declares.
- Function types carry effects: `fn(Req) -> Resp ! io, fail`. A closure with fewer effects is accepted.
- Standard higher-order functions take on the effects of the closure they're given: if `parse` can fail, write `lines.map(parse)?`. Your own functions do this with an effect parameter: `fn twice[!E](f: fn() ! E) ! E`.

## Concurrency

All code runs in lightweight tasks. An `io` call that waits suspends only its own task. There is no async/await.

- `par { ... }` runs each top-level statement of the block at the same time and waits for all of them. If one fails, the others are cancelled and the failure passes on. Variables declared inside are usable after the block. The statements can't use each other's variables or change variables from outside.

```
par {
  let user = db.user(id)?
  let posts = db.posts(id)?
}
return render(user, posts)
```

- `task.map(xs, f)` calls `f` on every element concurrently and keeps results in order. It stops at the first failure.
- `task.group(|g| { ... g.spawn(|| handle(conn)) ... })` runs any number of tasks and returns once all have finished. Spawned closures can't fail; they handle their own errors.
- `task.timeout(5s, || fetch(url))` fails with `.Timeout` if time runs out. A cancelled task's next `io` call fails with `.Cancelled`.
- Tasks share nothing: a value handed to a task is a copy. Shared mutable state uses shared handles:
  - `Shared[T]`: create with `Shared.new(v)`. `lock s as v { ... }` gives `v: inout T` for the block. It works on any `Shared` value, including a read-only parameter. No `io` is allowed inside a `lock` block.
  - `Atomic[int]`: `.load()`, `.store(n)`, `.add(n)`.
  - `Chan[T]`: bounded queue, created with `Chan[Msg].new(64)`. `.send(v)` waits while it's full (`! io, fail`). `.recv()` returns `none` once it's closed and empty (`! io`). Also `.close()` and `for m in ch { ... }`.

## C interop

```
extern "sqlite3" {                                     // links libsqlite3
  type sqlite3                                         // opaque C types
  type sqlite3_stmt
  fn sqlite3_open(path: *u8, db: **sqlite3) -> c.int
  fn sqlite3_close(db: *sqlite3) -> c.int
  blocking fn sqlite3_step(stmt: *sqlite3_stmt) -> c.int
}
extern "z" header "zlib.h"                             // declarations generated from the header
```

- Extern functions and raw pointers (`*T`) can only be used inside `unsafe { ... }` or an `unsafe fn`. Pointer operations are in `ptr`. `s.c_str()` gives a NUL-terminated copy.
- Mark C calls that can block (disk, DNS, heavy work) `blocking`; they run on a separate thread pool. `blocking extern "z" header "zlib.h"` marks a whole header.
- C types: `c.int c.uint c.long c.ulong c.size c.char`. Overt structs use C layout.
- Wrap each C handle in a resource so the rest of the program stays safe:

```
type Db { raw: *sqlite3 }
drop Db { unsafe { _ = sqlite3_close(self.raw) } }
```

## Standard library

- Always in scope: `print(x) ! io`, `dbg`, `assert`, `trap`, `todo`, `fail`, `Err`, `ErrKind`, `Map`, `Set`, `Shared`, `Atomic`, `Chan`.
- Modules: `math fs os time net http json log task ptr c`. For methods of built-in types, run `ovt outline str`, `ovt outline array` or `ovt outline Map`.
- `http.serve(addr, handler)` turns a handler's failure into a status: `.Invalid` 400, `.Denied` 403, `.NotFound` 404, `.Conflict` 409, `.Unavailable` 503, `.Timeout` 504, anything else 500.

## Tests

`ovt test` runs every `ex` line and every `test` block (see the program at the top) and prints only failures, showing both sides of a failed `==`. A `test` block declares the effects it needs: `test "name" ! io, fail { ... }`.

## Toolchain

| Command | Does |
|---|---|
| `ovt build`, `ovt run [args]` | compile; compile and run |
| `ovt test [filter]` | run `ex` lines and `test` blocks |
| `ovt outline <module or type>` | signatures, docs, `pre` and `ex` lines; no bodies |
| `ovt put <module.name>` | replace one declaration with the text on stdin |
| `ovt q callers <name>`, `ovt q type <file:line:col>` | look things up |
| `ovt fmt` | canonical formatting |

Errors are one line each and include a fix when one is known:

```
src/kv.ovt:12:9: error: no method str.index; did you mean str.find(self, pat: str) -> ?int
```

In an expression, `_` is a hole: the compiler reports the type it needs there and the values in scope that fit.

## Habits that are wrong in Overt

| Habit from elsewhere | In Overt |
|---|---|
| last expression is the return value | `return v` |
| `;` at the end of a line | nothing |
| `use`, `import` | none; write `module.name` |
| `a::b` | `a.b` |
| `Vec<T>`, `HashMap<K, V>`, `Option<T>`, `<T>` | `[T]`, `Map[K, V]`, `?T`, `[T]` |
| `Point { x: 1 }`, `Point(1, 2)` | `Point(x: 1, y: 2)` |
| `Some(x)`, `Ok(x)`, `Err(e)`, `throw` | `x`; `fail(.Kind, msg)` |
| `unwrap()`, `x!`, `x!!` | `?`, `else`, `catch` or `trap` |
| `&x`, `&mut x`, `mut` | read-only by default, `inout x`, `var` |
| `.clone()` | not needed |
| `impl T { fn m(&self) }` | `fn T.m(self)` |
| `format!`, f-strings, `"{x}"` | `"${x}"` |
| `usize` for lengths and indexes | `int` |
| `async`, `await`, `tokio::spawn`, `go f()` | not needed; `par`, `task.*` |
| redeclaring `let x` | pick a new name |
| a global mutable variable | a `Shared[T]` passed as a parameter |
