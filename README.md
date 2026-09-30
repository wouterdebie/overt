# Overt

An experiment: a compiled programming language designed for coding agents to write and read, instead of for humans. Anything that affects behavior is visible where it happens: effects are in signatures, mutation is marked at call sites, shared state shows in types. It compiles to native code through LLVM.

- [SPEC.md](SPEC.md): the language as an agent writing Overt sees it, kept under 6,000 tokens.
- [DESIGN.md](DESIGN.md): why each decision was made, and how the compiler and runtime implement it.
- [ROADMAP.md](ROADMAP.md): the milestones, which double as a benchmark against Rust and Go.

## Status

**Milestone 3's compiler and runtime are done: the TCP echo and chat servers, in Overt, pass their tests; the benchmark hasn't run yet.** The benchmark results for milestones 1 and 2 (`wordfreq`, `jsonfmt` and `hashdir` against Rust and Go) are in [DESIGN.md](DESIGN.md#benchmark-findings).
- **Language:** SPEC.md compiles to native code, with value semantics (reference counting and copy-on-write, checked leak-free and clean under AddressSanitizer).
  - Structs, enums (including recursive ones), generics, closures and methods.
  - `match` with exhaustiveness checking, optionals, failure handling, effects and parameter modes.
- **Concurrency:** `main` and all code run in tasks on one worker thread per core. `task.map`, `par`, `task.group`, `task.timeout`, `Chan`, `Shared` and `Atomic` share values safely by marking them shared, and ThreadSanitizer checks it.
- **Networking:** `net` (TCP listen, accept, connect, lines and bytes, timeouts) and `time`, on a kqueue poller. The chat server holds 10,000 idle connections at 34 KiB each.
- **Standard library:** strings, arrays, `Map`, `Set`, `os`, `fs` (including `walk`), `net`, `time`, `task`, `math` and `log`, written mostly in Overt.
- **Tools:** `ovt test` runs `ex` lines and `test` blocks, and `ovt outline` documents any module, including the standard library.
- **Not yet:** HTTP and JSON (milestone 4), and C interop (5).

## Building

Needs Rust and clang.

```
cd compiler
cargo build
cargo test
./target/debug/ovt run ../tasks/00-hello/overt
./target/debug/ovt outline str
```

`bench/run.py <task> <lang>` runs a fresh agent on a task and records the result; `bench/report.py` summarizes the results.
