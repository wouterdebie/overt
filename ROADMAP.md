# Overt roadmap

The goals are a ladder of programs. Each one needs a new layer of the compiler or runtime, so every rung shows that something new works. Milestones 1–6 double as the benchmark that tests the hypothesis in [DESIGN.md](DESIGN.md): the same tasks are written in Rust and Go by fresh agents, and the results are compared.

| # | Program | What it proves |
|---|---|---|
| 0 | Hello world | The whole pipeline, including the error format |
| 1 | `wordfreq` and `jsonfmt` | Core language, heap values, failure handling, tests |
| 2 | `hashdir` | Scheduler, parallel tasks, values shared across threads |
| 3 | TCP echo, then a chat server | Network IO, thousands of tasks, channels |
| 4 | HTTP JSON API | `http`, automatic JSON, error kinds as status codes |
| 5 | The same API on sqlite | C interop and blocking calls |
| 6 | Redis-compatible server | All of the above in one real server |
| 7 | `ovt` written in Overt | A large codebase maintained by agents |

## How each milestone runs

1. **Task first.** Write `tasks/<nn>-<name>/README.md`, a task description that doesn't depend on the language, plus black-box tests in `tasks/<nn>-<name>/tests/`. The tests talk only to the built program (arguments and output, TCP, HTTP), so the same suite runs against the Overt, Rust and Go versions. Both are written before any Overt code for the milestone.
2. **Build the layer.** Add the compiler, runtime and standard library work the milestone lists under *Needs*.
3. **A fresh agent writes the program.** It works in a directory that holds only SPEC.md, the task README, the tests and `ovt` on the PATH. It never sees the compiler's source, because the experiment is whether the language can be learned from the spec, not whether the agent that built the compiler can use it.
4. **Log everything.** Record the numbers under *What we measure*. Sort every stumble into one of three causes, and fix it in the matching place:
   - a gap in the spec: fix SPEC.md
   - a compiler bug: fix the compiler
   - a design flaw: fix DESIGN.md, then SPEC.md
5. **Benchmark (milestones 1–6).** Fresh agents get the same README and tests for Rust and Go, with their normal toolchains. Every task runs at least 3 times per language, because agent runs vary.

A milestone is done when its *Done when* list holds and the benchmark numbers are recorded. Overt doesn't have to win: a loss is a finding, and it goes into DESIGN.md.

## Repository layout

```
SPEC.md  DESIGN.md  ROADMAP.md
compiler/            the Rust compiler (stage 0)
runtime/             the C runtime (libovtrt.a)
std/                 the standard library, in Overt plus C where needed
tasks/<nn>-<name>/   README.md, tests/, and one directory per language (overt/, rust/, go/)
bench/               scripts that run agents and record the numbers
```

## Milestones

### 0. Hello world

A program that prints `hello, world`.

**Needs**
- A lexer and a parser for the *whole* grammar in SPEC.md, so `ovt fmt` and `ovt outline` work on any program. Checking and code generation only need to cover this milestone.
- Name resolution, and type checking for functions, string literals and `print`.
- Textual LLVM IR, compiled and linked by clang.
- A minimal runtime: `print`, and a trap handler that prints the message and location. `main` runs on the main thread; there's no scheduler yet.
- The one-line error format, with at least "unknown name, did you mean …" implemented with a fix.

**Done when**
- `ovt run` prints `hello, world`.
- A misspelled function name produces a one-line error with a suggestion.
- Every code block in SPEC.md parses, and `ovt fmt` leaves it unchanged.
- `ovt outline` works on the example program in SPEC.md.

This milestone isn't benchmarked.

### 1. `wordfreq` and `jsonfmt`

- `wordfreq <file> <n>` prints the `n` most frequent words with their counts.
- `jsonfmt` reads JSON on stdin and pretty-prints it. It reports syntax errors with line and column. It includes its own JSON parser, since writing one is part of the task.

**Needs**
- **Types:** integers, floats, `bool`, `str`, arrays, `Map`, `Set`, tuples, optionals, structs, enums (including recursive ones), generics, closures and methods.
- **Patterns:** `match` with every kind of pattern.
- **Checks:** effects and failure (`?`, `else`, `catch`), parameter modes and the exclusivity check, unused results, no shadowing.
- **Memory:** reference counting, copy-on-write, moves on last use.
- **Tests:** the `ex` and `test` runner behind `ovt test`.
- **Standard library:** methods on `str`, arrays and `Map`, plus `fs.read`, `os.args` and `print`.

**Done when**
- The black-box tests pass.
- macOS `leaks` reports no leaks for either program.
- Speed is measured on a large input (about 100 MB of text for `wordfreq`, a large JSON file for `jsonfmt`) against the Go and Rust versions.

### 2. `hashdir`

`hashdir <dir>` walks a directory tree and hashes every file with SHA-256, in parallel. The output is sorted and in the same format as `shasum -a 256`. The agent implements SHA-256 itself, which exercises `u32` arithmetic, wrapping operators and bit operations.

**Needs**
- **Runtime:** worker threads, context switching, task stacks and work stealing.
- **Concurrency:** `task.map`, `par` and `Atomic`, plus marking values shared when they cross threads.
- **Blocking pool:** file IO runs there, since kqueue doesn't cover regular files.
- **Standard library:** `fs.walk`.

**Done when**
- Output matches `find <dir> -type f -exec shasum -a 256 {} + | sort -k 2` on a test tree.
- Run time drops as cores are added, until IO becomes the limit.
- The program is clean under ThreadSanitizer (clang's `-fsanitize=thread` on the generated IR), which is a direct test of the memory model.

### 3. TCP echo, then a chat server

- **Echo:** sends back every line it receives.
- **Chat:** a line-based protocol. `/nick <name>` sets a name, `/join <room>` switches rooms, and other lines go to everyone in the room. Idle connections time out.

**Needs**
- **Runtime:** a kqueue poller, with tasks suspending on network IO.
- **Standard library:** `net` (listen, accept, read, write), with sockets as resources that have `drop`.
- **Concurrency:** `Chan`, `Shared`, `task.group`, cancellation when a client disconnects, and timers.

**Done when**
- The black-box client tests pass.
- The server holds 10,000 concurrent idle connections, using less than 64 KiB of memory per idle connection.
- Message latency across a room is measured.

The chat server is benchmarked; the echo server isn't.

### 4. HTTP JSON API

An in-memory todo service:
- `POST /todos`, `GET /todos`, and `GET`, `PATCH` and `DELETE` on `/todos/{id}`
- invalid input returns 400, and a missing todo returns 404

**Needs**
- `http` in the standard library, written in Overt on top of `net`: an HTTP/1.1 parser with keep-alive.
- `json` encoding and decoding, generated by the compiler for every type.
- Failure kinds mapped to status codes.
- `log`.

**Done when**
- The black-box HTTP tests pass.
- A load test with `oha` or `wrk` records requests per second, p99 latency and memory, against Go (`net/http`) and Rust (`axum`).

### 5. The same API on sqlite

Milestone 4's service, with the todos stored in sqlite.

**Needs**
- `extern` blocks, and header import through libclang.
- `blocking` calls, and resources with `drop`.
- `ffi` in the standard library (C types and pointer operations), and `c_str()`.

**Done when**
- Milestone 4's tests pass unchanged.
- A restart test shows the data survives.
- `leaks` reports no leaks.

### 6. Redis-compatible server

**Scope**
- The RESP2 protocol, including pipelining.
- Commands: `PING`, `ECHO`, `GET`, `SET` (with `EX`, `PX`, `NX` and `XX`), `DEL`, `EXISTS`, `INCR`, `DECR`, `MGET`, `MSET`, `EXPIRE`, `TTL`, `PUBLISH`, `SUBSCRIBE` and `UNSUBSCRIBE`.
- An append-only file for persistence, synced to disk every second.

**Needs**
- Expiry: lazy on access, plus periodic cleanup driven by a timer.
- Pub/sub fan-out.
- Appending to a file and `fsync`, through the blocking pool.
- Whatever the earlier milestones turned out to be missing.

**Done when**
- `redis-cli` works interactively against the server.
- A test suite written with an existing Redis client library passes.
- `redis-benchmark -t get,set -P 16` results are recorded against real Redis and the Go and Rust versions.
- A restart restores the data from the append-only file.

**Watch for:** whether we need `select` over channels. A subscribed connection has to read commands and deliver published messages at the same time; v0's answer is one reader task and one writer task per connection.

### Language review before milestone 7

After milestone 6, review the language as a whole:
- Resolve or explicitly defer every open question in DESIGN.md.
- Make sure SPEC.md is still under its 5,000-token cap.
- Call the result spec v1.

Changing the language gets expensive after this point, because every change has to be made in two compilers until milestone 7 is done.

### 7. `ovt` written in Overt

This milestone is done in slices. The Rust compiler stays the reference until the last one.

1. **Front end:** lexer, parser, `fmt` and `outline`. Both compilers must produce the same output on every compiler test file and every task program.
2. **Resolution and checking:** both must give the same diagnostics on the compiler's error tests.
3. **Lowering and code generation:** programs compiled by both must produce the same output on every test.
4. **Bootstrap:** the Rust compiler builds stage 1, stage 1 builds stage 2, and stage 2 builds stage 3. Stages 2 and 3 must emit identical LLVM IR for the compiler itself.

**Needs**
- `os.exec` (to run clang), exit codes and `fs.write`.
- Compile speed that makes the Overt compiler practical to use.

**Done when**
- The bootstrap reaches that fixed point.
- All compiler tests pass.
- Compile time is recorded next to the Rust compiler's.

**Measured differently.** A compiler is too large to rebuild in Rust and Go for comparison. This milestone measures how agents cope with a large Overt codebase instead:
- tokens and compile rounds per change
- how often `ovt outline` was enough, and how often function bodies had to be read

The work spans many agent sessions. Each session starts with SPEC.md and an outline of the Overt compiler.

## What we measure

For every agent run:
- tokens in and out, and tool calls
- compile-fix rounds: failed builds before the tests pass
- tests passing at the first successful build, and at the end
- every stumble, its cause (spec gap, compiler bug or design flaw), and the fix

For every program, depending on the task:
- run time or throughput
- p99 latency
- peak memory
- binary size

The scripts in `bench/` that run agents and record these numbers are built alongside milestone 1. Headless Claude Code runs report token usage, which covers the agent numbers.
