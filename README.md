# Overt

An experiment: a compiled programming language designed for coding agents to write and read, instead of for humans. Anything that affects behavior is visible where it happens: effects are in signatures, mutation is marked at call sites, shared state shows in types. It compiles to native code through LLVM.

- [SPEC.md](SPEC.md): the language as an agent writing Overt sees it, kept under 5,000 tokens.
- [DESIGN.md](DESIGN.md): why each decision was made, and how the compiler and runtime implement it.
- [ROADMAP.md](ROADMAP.md): the milestones, which double as a benchmark against Rust and Go.

## Status

Milestone 0 is done:
- `ovt` parses the whole grammar in SPEC.md, and `ovt fmt` and `ovt outline` work on any program.
- It compiles a core subset to native code: `int`, `bool` and `str` values, functions, `if`/`while`/`for`, checked arithmetic, `pre` conditions and the `io` effect.
- Everything else is reported as not supported yet.

## Building

Needs Rust and clang.

```
cd compiler
cargo build
cargo test
./target/debug/ovt run ../tasks/00-hello/overt
```
