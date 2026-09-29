# Working on Overt

- The compiler is `compiler/` (Rust, no dependencies) and the runtime is `runtime/rt.c`, embedded into `ovt` at build time. Build and test with `cd compiler && cargo build && cargo test`.
- SPEC.md is everything a code-writing agent knows about Overt. It must stay under 5,000 tokens (the command is at the end of DESIGN.md), and every `ovt` code block in it must come back unchanged from `ovt fmt`; `cargo test` checks this.
- When the language or its implementation changes, update SPEC.md and DESIGN.md in the same change. ROADMAP.md says what each milestone needs.
- Golden tests live in `compiler/tests/`:
  - `run/`: expected stdout in `.out`, and the trap message in `.stderr`
  - `errors/`: expected diagnostics in `.err`
  - `outline/`: expected `ovt outline` output in `.outline`

  Read the new output before regenerating a golden file.
- A feature the compiler doesn't implement yet must fail with a "not supported by this compiler yet" error, never a crash or wrong code.
- Task programs in `tasks/*/overt` are written by fresh agents that see only SPEC.md, the task and `ovt` (see ROADMAP.md). Don't write them yourself; `00-hello` is the only exception.
