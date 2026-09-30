# Overt design notes

[SPEC.md](SPEC.md) is the language as an agent writing Overt code sees it; that file goes into the agent's context and has a token budget. This file holds the rest: why each decision was made, what the compiler and runtime must do to implement it, and what is still open. Nothing here is needed to *write* Overt, so it isn't budgeted.

## The hypothesis

A coding agent working in Overt, with only SPEC.md and the toolchain, should build network servers with fewer tokens and fewer compile-fix rounds than it needs in Rust or Go. The result should be equally correct and comparably fast. The milestones in [ROADMAP.md](ROADMAP.md) test this: each one's program is also written in Rust and Go by fresh agents. If Overt doesn't win, we still learn which of these intuitions were wrong.

## Principles

1. **Learnable from the spec alone.** No model has training data for Overt, so SPEC.md is everything it knows. The hard cap is **5,000 tokens**, measured with `o200k_base` as a proxy (see the end of this file). A feature has to be worth the tokens it adds to the spec.
2. **Read less, fail less, fail cheaply.** Most agent tokens go to reading code and output and to retrying after errors, not to writing code. Outlines, locality and precise errors matter more than terse syntax.
3. **Familiar surface.** C-family, ASCII, braces, keywords an agent would guess anyway. Where Overt differs from Rust or TypeScript habits, the spec lists the difference in its last table.
4. **Explicit and local.** Every name resolves to exactly one declaration, which one grep finds. A function's signature says everything a caller needs to know.
5. **Redundancy where it catches mistakes:** named arguments for same-typed parameters, `inout` at the call site, `?` on every failable call, and an error when a result is left unused.
6. **A wrong choice costs one compile round, never a silent bug.** For example, forgetting `sink` costs a cheap copy, and forgetting `inout` is a compile error.

## Decisions

| Decision | Why | Rejected |
|---|---|---|
| C-family brace syntax | Most training data; brace errors are loud | APL and Unicode (rare symbols take several tokens and carry more meaning each); stack or point-free (hidden state to track); S-expressions (counting closing parens); AST as source (several times the tokens); significant whitespace (a string-replace edit can silently change meaning); short names (save about a token, lose meaning) |
| Construction as `T(field: v)` | Order-proof, same shape as calls, and no Rust/Go ambiguity with `if x == T{` | `T { field: v }` |
| `${expr}` interpolation | Server code is full of JSON; with `{x}` every literal brace needs escaping | `{x}`, `\(x)` |
| `return` required | The habit of most languages, and exits are greppable | Rust's implicit tail value |
| Blocks yield values only in `if`/`match`/`else`/`catch`/`lock` | Keeps `let x = if ...` and `match` concise | Everything is an expression |
| No imports | Removes the "forgot the import" round | `use` lists. Cost: a local can't be named like a module |
| No shadowing | Catches `let n = 5` in an inner block where `n = 5` was meant | Rust-style shadowing |
| Unused result is an error | With value semantics, `s.trim()` as a statement does nothing, silently | Warnings |
| Named arguments when parameter types repeat | Swapped `(src, dst)` is a classic agent bug | Always named (too many tokens); never named |
| One fixed `ErrKind` set | Servers map errors to statuses; `http.serve` does it automatically | Per-function error types (conversion boilerplate); string codes (typos) |
| Effects: only `io` and `fail` | Enough to know "does this touch the world" and "can this fail" | Fine-grained effects (signature churn) |
| `dbg` ignores effects | Print debugging in pure code without editing signatures | |
| `T` converts to `?T` | Lossless, and avoids `Some(x)` noise | No implicit conversions at all |
| Maps keep insertion order | Deterministic test output and JSON | Unordered hash maps |
| No user-defined interfaces in v0 | Spec budget; enums and structs of functions cover most server needs | Traits. Revisit if the benchmark needs them |
| Traps stop the process in v0 | No unwinding in the first codegen | Per-task traps (open question) |
| Names starting with `_` are private to their module | `Map`'s storage stays hidden, so `m.keys` (a field) and `m.keys()` (a method) can't clash | `pub` on public names (more tokens on every declaration) |
| Standard library written in Overt, over a few intrinsics | `ovt outline` shows the real signatures and docs; `Map` and `Set` are plain generic code | Built-in collections in the compiler |
| `pat => x += 1` accepted in `match` arms | A Rust habit that costs nothing to allow; `ovt fmt` adds the braces | An error |
| Arguments that read a variable passed `inout` are copied | `xs.push(xs[0])` just works, and the spec needs one rule less | Rejecting any other use of the variable in the call |
| Name "Overt" | States the principle; no existing language with a similar name whose habits would leak in | Cairn (one letter from Cairo, a real language) |

## Memory model

### Representation

| Kind | Layout |
|---|---|
| plain | inline, C layout (fields in declared order, C alignment) |
| `str`, `[T]` | `{buf: ptr, off: i64, len: i64}`, a view of a buffer with a 24-byte header `{rc: i64, cap: i64, used: i64}` followed by the elements. `used` is how many elements the buffer holds, since a view may show fewer. As in Lean 4, `rc > 0` is a count owned by one task, `rc < 0` (planned) a shared count updated atomically, and `rc == 0` a static buffer that is never freed (string literals). The empty array is `{null, 0, 0}` |
| `Map`, `Set` | plain Overt structs in `std/collections.ovt`: parallel arrays of keys, values and live flags, plus an open-addressing slot table of indexes, like Python's compact dict. They get copy-on-write from their arrays |
| recursive field | a pointer to a box `{rc: i64, value}`, inserted by the compiler when a type contains itself without an array or other indirection in between |
| enum | `i32` when no variant has fields; otherwise `{i32 tag, [N x i64] payload}`, with each variant's fields laid out as a struct in the payload |
| `?T` | `{i1 some, T}`; niche optimization comes later |
| closure | `{fn: ptr, env: ptr}`. The environment is `{rc: i64, drop: ptr, captures...}` holding copies of the captured values; `drop` frees it, so dropping a function value doesn't need its closure's type. Named functions used as values get a thunk and a null environment |
| resource | planned (milestone 5): plain layout plus a drop function; move-only |
| `Shared[T]`, `Atomic`, `Chan` | planned (milestone 2): a pointer to a box with an always-atomic count. `Shared` holds `{rc, lock, owner_task, T}` |

### Copy, move, drop

- **Owned or borrowed.** Codegen evaluates every expression as either owned (the holder must drop it) or borrowed (valid while its owner lives). Copying a borrowed value into a place that keeps it (a variable, a field, a `sink` argument, a return) increments its counts (`dup`). Owned temporaries that are only read are dropped at the end of their statement.
- **Cleanup stack.** Locals and temporaries that need dropping are registered on a stack of scopes. Leaving a scope normally drops its entries; `return`, failure, `break` and `continue` drop every scope they leave.
- **Mutating a heap-backed value** requires its view to be the only one and to cover the whole buffer (`rc == 1`, `off == 0`, `len == used`); otherwise the viewed range is copied first, with its elements `dup`ed. `push` and `s += t` then work in place, growing the buffer by doubling.
- **Slices** (`xs[a..b]`, `s[a..b]`) are `{buf, off + a, b - a}` plus a `dup`. Known cost: a small slice keeps a large buffer alive.
- **Per-type helpers.** For each concrete type, codegen generates `dup`, `drop`, `eq`, `cmp`, `hash` and `show` functions that call the helpers of the type's components, so recursive types need no special handling.
- **Not yet:** a variable's last use doesn't become a move, so values are copied (a count update) where a move would do. Perceus-style reuse analysis comes later.
- **Resource drops** (milestone 5) run at scope end in reverse declaration order. Moved-out variables aren't dropped (static drop flags).

### Calling convention

| Mode | Passed as | Count traffic |
|---|---|---|
| `x: T` | the value (LLVM aggregates for structs), borrowed | none; the callee `dup`s only what it keeps |
| `x: inout T` | a pointer to the caller's place | none |
| `x: sink T` | the value, owned | the caller `dup`s; the callee drops it |
| return | the value, owned | |
| failable function | returns `{i1 failed, T, Err}` | the error's message is owned |

Closures and function values take their environment pointer first. Runtime functions take only scalars and pointers, so the C calling convention for structs never matters.

**Planned:** pass large read-only values by pointer with `noalias readonly nocapture nonnull`, and `inout` pointers with `noalias`. That is sound because:
- An `inout` argument overlaps no other argument (see below).
- There are no mutable globals.
- A `Shared` value can only be mutated inside `lock`, and a task can't lock the same `Shared` twice. The runtime records the owner task and traps on re-entry.

It's also the biggest single enabler for LLVM's auto-vectorizer.

`inout xs[i]` first makes `xs`'s buffer unique (copy-on-write), then passes a pointer to the element. A map entry can't be passed `inout` or changed in place: the entries live in `Map`'s arrays, behind functions. The compiler rewrites `m[k] = v` and `m[k] += v` into calls to `set`, and rejects `m[k].field = v` with a message saying to copy the entry out and back.

### Exclusivity check

The check is static and runs per call:
- The same variable can't be passed as `inout` twice.
- An argument that reads a variable another argument passes as `inout` is copied before the call, so `xs.push(xs[0])` works. The spec therefore needs only the first rule.
- A variable being changed by `for inout` can't be used in the loop body.

Disjoint fields (`f(inout p.x, inout p.y)`) are rejected for now, since any two `inout` arguments with the same root variable count as the same.

### Values that cross threads

A task owns its data. Non-atomic counting is safe even when the scheduler moves a task to another worker thread, because the handoff itself synchronizes.

Data that becomes reachable from more than one task is marked *shared* by negating its count, Lean 4 style. Count operations on marked buffers use atomics. Marking happens when a value:
- is put into a `Shared`, `Chan` or `Atomic`
- is captured by a `par` statement, `task.map` or `task.group` closure
- is passed to `g.spawn`

Marking walks the reachable buffers once and stops at any buffer that's already marked.

## Effects and failure

- Effects are checked by the type checker and erased in codegen.
- A failable function returns `{i1 failed, T, Err}`, where `Err` is the prelude struct `{kind: ErrKind, msg: str}`. `?` is a branch that returns the error, dropping everything in scope. `else` and `catch` are branches to the handler.
- A failing `main` prints `error: <msg>` and exits with status 1.
- Std failure messages say what failed and on what: `can't read <path>: <reason>`, with the whole path. Callers pass them on with `?` and don't add the path again.
- In an `ex` line, the outermost call on each side may fail without `?`; a failure fails the example. Calls inside it that can fail need `?` as usual. Only `io` is ruled out, so examples don't depend on the outside world. (Until the wordfreq benchmark, `ex` lines also ruled out failing inside calls, and agents made their functions trap to get around it.)
- Effect parameters (`!E`) are monomorphized per call site, like type parameters. Inside the generic body, calling an `E`-typed function passes failure on implicitly, since the body can't know whether `E` contains `fail`. After monomorphization with an `E` without `fail`, that branch disappears.
- Standard higher-order functions (`map`, `filter`, `task.map`, ...) are declared with effect parameters.

## Traps

- **v0:** flush stdout, write `file:line:col: trap: message` (later a backtrace) to stderr, and exit with status 101. Not `abort()`: on macOS that triggers a slow crash report for every trap.
- **Overflow** uses `llvm.*.with.overflow` intrinsics. **Bounds checks** are a compare and a branch to a cold trap block.
- **Stack overflow** hits the task stack's guard page. A SIGSEGV handler on an alternate signal stack turns it into a trap message.
- **Later:** a trap fails only its task, using LLVM landing pads for unwinding, so one bad request only kills its connection. This needs a policy for locks poisoned mid-update.

## Concurrency runtime

The runtime is written in C as a static library, `libovtrt.a`, linked into every program.

- **Scheduler.** Tasks are spread over one worker thread per core. Each worker has its own run queue (a Chase-Lev deque) and idle workers steal from busy ones. A global queue takes tasks from outside the workers.
- **IO.** kqueue on macOS first; later epoll and then io_uring on Linux. Sockets are non-blocking. A task waiting on IO registers interest and parks.
- **Task stacks.**
  - Each task reserves 256 KiB of virtual memory plus a guard page. The OS commits pages only when they're touched, and stacks are pooled and `madvise(MADV_FREE)`d on reuse.
  - Stacks never move, because borrowed parameters point into them, so there are no growable stacks.
  - An idle task costs its touched pages, roughly 16–32 KiB with arm64 macOS's 16 KiB pages.
  - This is why tasks keep their own stacks (stackful coroutines): a borrowed pointer stays valid across an IO wait, which avoids the whole class of Rust `Pin` and self-referential future problems.
- **Context switch.** Hand-written assembly per architecture, arm64 first. It saves x19–x30, sp and d8–d15, about 20 instructions.
- **Blocking C calls.** `blocking` extern calls run on a separate pool of OS threads while the task parks. Non-blocking extern calls run on the task stack, so heavy C work should be marked `blocking`.
- **Timers.** A timer heap per worker, used by `time.sleep` and `task.timeout`.
- **Cancellation.** Each task has a flag, and cancellation propagates down the task tree. The flag is checked at every suspension point and on entry to every `io` call, which then fails with `.Cancelled`.
- **Structured scopes.** `par`, `task.map` and `task.group` create child scopes, and the parent waits for them. In fail-fast scopes (`par`, `task.map`), a failure cancels the siblings.
- **`Shared[T]`.** Lock blocks can't do `io`, so critical sections are short. v0 uses `os_unfair_lock` (a pthread mutex on Linux), which blocks the worker thread for the duration. A task relocking a `Shared` it already holds traps.
- **`Chan[T]`.** A bounded ring buffer with queues of parked senders and receivers.
- **`Atomic[int]`.** A box holding an atomic `i64`.

## C interop

- `extern "lib" { ... }` declares the symbols and adds `-llib` to the link.
- `header "x.h"`:
  - At build time, libclang parses the header and generates extern declarations for functions, structs (C layout), enums (integer constants) and integer `#define`s.
  - Macros with arguments and varargs functions are skipped and reported.
  - Generated declarations are cached in the build directory and visible to `ovt outline`.
- Only C-compatible types may appear in extern signatures: numbers, `bool`, `*T`, and structs of those. `str`, arrays, enums and optionals may not.
- `s.c_str()` allocates a NUL-terminated copy. Keeping it alive while C uses the pointer is the `unsafe` code's job.
- **Not in v0:** callbacks from C into Overt. They'd need exported C-ABI functions and a rule for code running on a thread that has no task.

## Codegen and performance

- Generics and effect parameters are monomorphized.
- Parameter attributes follow the calling-convention table above.
- `for x in xs` needs no bounds checks. Indexed loops keep them, and LLVM hoists many.
- Standard float reductions (`sum`, `dot`) use several accumulators so they vectorize without fast-math.
- `simd` is reserved. Later, `simd[T, N]` will map directly onto LLVM vector types.
- Build: emit textual `.ll`, then `clang -O2`, linking the runtime and extern libraries.

## Compiler architecture

The compiler (`compiler/`) is written in Rust with no dependencies. Its stages:

1. **`lexer`**: tokens, with newlines that end statements; literals keep their source text.
2. **`parser`**: a syntax tree with spans, which `fmt` and `outline` also use.
3. **`check`**: all modules of the program and the standard library together.
   - `mod.rs`: declarations, scopes, signatures, recursive types.
   - `body.rs` and `call.rs`: bodies, with bidirectional checking and unification (`infer.rs`) inside each function. Closures are checked in their own frame and capture what they use from outside.
   - `pat.rs`: patterns, and exhaustiveness with the usefulness algorithm, which also gives an example of a missing case.
   - `zonk.rs`: resolves inferred types, defaults literals, checks `Eq`/`Ord`/`Hash` constraints.
   - The result is the typed program (`tir.rs`), whose types may still contain generic parameters.
4. **`codegen`**: textual LLVM IR. Each function is emitted once per set of type arguments, from a work queue, along with per-type helpers (`helpers.rs`), `match` (`pat.rs`) and intrinsics (`intrin.rs`).
5. **clang**: compiles the IR together with the runtime (`runtime/rt.c`, embedded in `ovt`).

Diagnostics are one line each, with the fix in the message when one is known. Planned: `--json` output, and `ovt put` and `ovt q` on the same front end.

The compiler's tests (`cargo test`) are golden files:
- programs with their expected output, traps and exit status (`tests/run/`)
- programs with their expected errors (`tests/errors/`)
- outlines (`tests/outline/`)
- the `ovt` code blocks in SPEC.md, which must parse and be canonical
- the standard library's `ex` lines (`ovt test --std`)
- reference versions of the task programs (`tests/programs/`), which must pass the tasks' tests

Set `OVT_CFLAGS="-fsanitize=address -g"` to build any program with AddressSanitizer.

## Standard library

The standard library (`std/`) is written in Overt, and embedded in `ovt`. Functions declared without a body are intrinsics, implemented in `codegen/intrin.rs` over the runtime. Files like `str.ovt` and `collections.ovt` declare methods and types visible everywhere; `os.ovt`, `fs.ovt`, `math.ovt` and `log.ovt` are modules. `ovt outline <name>` shows a file's declarations and docs, and hides private (`_`) names. The same outlines are what agents read, so the docs in std/ are part of the language's interface.

## Build order

The milestones in [ROADMAP.md](ROADMAP.md) set the build order. Each milestone lists the compiler, runtime and standard library work it needs.

## Benchmark findings

### Pilot: `wordfreq` in Overt, one run (milestone 1)

- **Result:** 19 of 19 tests, 0.21 s on 100 MB. 31 builds (8 failed), 68 turns, 18 minutes, 86k output tokens, $4.07.
- **Where the effort went:** the agent's first version, using `Map` the obvious way, took about 1.3 s. Because the prompt says speed matters, it spent most of the run replacing `Map` with its own hash table over a byte arena, and trying `task`, `par` and `[T; N]`, which weren't implemented.
- **Lesson:** the speed of the obvious code decides how many tokens agents spend. Overt's obvious code has to be fast, or agents will hand-optimize. Fixed since:
  - an inline fast path for `push` and array writes
  - a larger first allocation for small elements
  - an inline fast path for dropping shared buffers
  - `to_lower`/`to_upper` share the string when nothing changes

  That brought the first version from 1.33 s to 1.0 s. Still open: the counting idiom `m[k] = (m.get(k) else 0) + 1` looks the key up twice.
- **Stumbles and fixes:**
  - `c` was reserved as a module name (for C types), clashing with a common variable name. C types and pointer operations moved to `ffi`.
  - `int.parse(s) else none` in a function returning `?int` was an error. Now the result is optional when an optional is expected, for `else` and `catch`.
  - A real error was followed by "can't tell the type of this value". That message is now left out when the function already has an error.
  - `ovt build --help` said "no such file or directory". `--help` now prints the usage.
  - `ovt outline task` said there's no such module. It now says the module is planned for milestone 2.
  - The named-argument rule cost one round (`ranks_before(a, b)`, both `WordCount`). It's working as intended: that is exactly the swap it exists to catch.

### `wordfreq` in Overt and Rust, three runs each (protocol v2, milestone 1)

- **Result:** all six programs passed all 19 tests on their first build. Neither language had a failed build. Medians:

  | | Overt | Rust |
  |---|---|---|
  | tool calls (to first pass) | 8 (6) | 3 (3) |
  | input tokens (to first pass) | 270k (135k) | 117k (86k) |
  | output tokens | 3.3k | 1.9k |
  | cost | $0.30 | $0.14 |
  | 100 MB | 0.90 s | 0.55 s (fastest run 0.34 s) |

- **Where Overt's extra cost goes:**
  - **Learning the language:** reading SPEC.md and outlining four to seven std modules takes two or three extra calls, and adds about 12k tokens that every later call reads again.
  - **Polishing after the tests pass:** the Overt agents went on to run `ovt test` and `ovt fmt` and to try error cases by hand, which took two or three more calls. The Rust agents replied DONE as soon as the tests passed.

  The learning cost is fixed, so it dominates a task this small. The tasks from milestone 3 on are the ones that test the hypothesis.
- **Stumble: the path appears twice in the error message.** `fs.read_bytes` fails with `can't read <path>: <reason>`, and every agent wrapped it the way it would in Rust or Go, as `can't read ${path}: ${e.msg}`. Two agents noticed the doubled path when they tried a missing file and fixed it. The third shipped it, because the tests don't check what stderr says.
  - **Fixed since:** SPEC.md says that std failures already name what failed, and that callers should pass them on with `?`.
  - **Found while fixing it:** a file error with a path over about 1,170 bytes read past a 1,200-byte stack buffer. `snprintf` returns the untruncated length, and the runtime copied that many bytes. These messages are now built by appending their parts, with no length limit, and the `fs_errors` golden test covers them.
- **Speed:** all three programs count words with the idiom the pilot used. For each word, that idiom:
  - makes a `str` from a reused `[u8]` buffer, which means a UTF-8 check plus a new allocation when the buffer is next written
  - then runs `m[k] = (m.get(k) else 0) + 1`, which looks the key up twice

  The fastest Rust version looks the word up by slice, and allocates only for new words.

### `wordfreq` with the docs in the prompt (protocol v3, milestone 1)

- **Setup:** the model already knows Rust and Go, so Overt's docs go in the prompt, the way a CLAUDE.md would load them. There are two variants:
  - `overt` gets SPEC.md.
  - `overt-std` gets SPEC.md plus the outline of every std module.

  Each ran three times, and Rust ran again. All nine programs passed all 19 tests. Medians:

  | | `overt` | `overt-std` | Rust |
  |---|---|---|---|
  | tool calls (to first pass) | 5 (4) | 4 (2) | 3 (3) |
  | input tokens (to first pass) | 242k (155k) | 211k (81k) | 118k (86k) |
  | output tokens | 2.0k | 1.9k | 1.9k |
  | cost | $0.27 | $0.26 | $0.14 |
  | docs: tokens, share of input | 12.3k, 26% | 11.8k, 28% | 0 |
  | 100 MB | 0.84 s | 0.73 s | 0.52 s |

- **The std outline is what saves steps:**
  - With the outline in the prompt, the agents wrote the program before looking anything up. They reached passing tests in fewer steps and tokens than Rust.
  - With only the spec, they still spent two steps on `ovt outline`. The spec alone saves nothing, because in v2 the agents read it in the same step as other files.
- **The cost gap is the one-time cache write:** a fresh agent writes its docs, about 12k tokens, into the prompt cache once.
  - Overt runs write 21–24k tokens to the cache, against Rust's 9.3k.
  - At the prices these runs imply, the extra 12k is about $0.10 of the $0.12 difference.
  - Cache reads grow with every step but cost little.

  So learning Overt costs something per session, not per step, and a long session pays it once.
- **Since then:** from protocol v4 on, `overt` in the benchmark means SPEC.md plus the std outline in the prompt; `overt-spec` is the spec alone.
- **Checking after the pass:** the agents still took one or two more steps after the tests passed (`ovt test`, `ovt fmt`, an error case). The Rust agents didn't.
- **The error-message line in SPEC.md worked:** all six programs pass the file error on with `?`, and none of them prints the path twice.
- **Stumbles:**
  - `catch _ { ... }` to ignore the error: one run, one failed build. The message asks for a name, but doesn't say that `else` is the way to handle a failure without looking at it. **Fixed since:** the message now points to `else`.
  - An `ex` line with a failable call inside it, `ex count_words(b)?.get("b") == 2`: one run, and the same happened in v2. SPEC.md says `ex` lines may only call pure functions, yet its own example `ex parse("ab") fails .Invalid` calls one that can fail. The checker allows failure only in the outermost call. Both agents made the function trap instead, which is worse code written to satisfy the rule. **Fixed since:** an `ex` line may call functions that fail, anywhere in it, and only `io` is ruled out.

### `jsonfmt` in Overt, Rust and Go, three runs each (protocol v4, milestone 1)

- **Result:** all nine programs passed all 45 tests. One Overt run had two failed builds; no other run had any. Medians:

  | | Overt | Rust | Go |
  |---|---|---|---|
  | tool calls (to first pass) | 8 (6) | 6 (3) | 3 (3) |
  | input tokens (to first pass) | 461k (289k) | 252k (92k) | 129k (92k) |
  | output tokens | 9.9k | 5.8k | 3.6k |
  | of which thinking, roughly | 7k | 2.8k | 1.1k |
  | cost | $0.55 | $0.32 | $0.21 |
  | program, code only (o200k tokens) | 1.7k | 1.8k | 1.7k |
  | 50 MB | 0.43 s | 0.12 s | 0.21 s |

  "Thinking" is output tokens minus the code and text the agent wrote. The transcripts hide the thinking itself, and the estimate counts the visible part with `o200k_base`, so it's rough.
- **Where Overt's extra cost goes:**
  - **The docs:** writing them to the cache costs about $0.11 per run, as in wordfreq.
  - **Thinking:** the Overt agents thought two to three times as much as the Rust agents, and six times as much as the Go agents. Output tokens are the most expensive kind, so at the rates these runs imply that's about $0.08–0.12 more than Rust. There was no such gap on wordfreq. The extra deliberation grows with the size of the program, so it isn't a fixed cost.
  - **Steps:** the tests first passed after 4–9 tool calls, against 3 for every Rust and Go run.
- **Programs aren't shorter:** without comments and `ex` lines, the Overt programs are about the size of Go's and a little under Rust's. In wordfreq they were about a third smaller than Rust's. A byte-level parser gives the language little to save. The Overt programs also carry twice the comments, plus examples.
- **What this says about the hypothesis:**
  - Correctness can't separate the languages on these tasks: Rust and Go agents pass on their first build too.
  - What the tasks do measure is Overt's learning cost, which is now two parts: the docs, fixed per session, and extra thinking, which grows with the program.
  - Fewer compile-fix rounds can only show up where Rust and Go agents actually need rounds: concurrency and shared state, from milestone 2 on.
- **Stumble:** a helper `fn Parser.bad(self, what: str) -> never ! fail` that always fails, called without `?` (one run, two failed builds). The rule is consistent, since every call that can fail is handled, but the agent then replaced every call with an inline `fail(...)`, and its regex broke the parentheses.
- **Speed:** Overt is 3.5 times slower than Rust here, and about twice as slow as Go. The reference Overt version takes 0.37 s.

## Open questions

- Why do agents think two to three times as much when writing Overt, and does the spec's shape (more examples, fewer rules) change that?

- Should a trap stop only its task? That needs unwinding and a policy for poisoned locks.
- `select` over channels. v0 avoids it with separate reader and writer tasks per connection.
- Do we need user-defined interfaces? The benchmark decides.
- Visibility (`pub`) once there are libraries.
- Deadlock between two `Shared` values locked in opposite orders: lock levels?
- Adding context to a propagated failure without a full `catch`, for example `f()? "loading ${path}"`.
- `m[k]` traps on a missing key. Is that the right default for servers handling untrusted input?
- Default stack reservation, and memory per idle connection with 16 KiB pages.
- Small slices keeping large buffers alive.
- Effect parameters on closures stored in structs.
- Changing a map entry in place (`m[k].count += 1`, `m[k].push(x)`): today it takes a copy out and a store back.
- Moves on last use, so values aren't copied where they're used for the last time.
- One lookup for `m[k] = (m.get(k) else 0) + 1`, the most common map idiom, which today hashes and probes twice.
- Unchecked arithmetic for hot loops, if overflow checks block vectorization in practice.
- Callbacks from C.
- Linux: epoll or io_uring, and an x86_64 context switch.

## Measuring the spec budget

```
uvx --with tiktoken python -c "import tiktoken; e = tiktoken.get_encoding('o200k_base'); print(len(e.encode(open('SPEC.md').read())))"
```

`o200k_base` is a proxy, since Claude's tokenizer isn't available offline. Treat the number as relative: SPEC.md must stay under 5,000 by this measure. After milestone 1 it's about 4,950.

Agents also get the outline of every std module in their prompt (see the benchmark findings), so its size counts too. It has no cap yet. After milestone 1 it's about 3,200 by the same measure:

```
for m in prelude array collections fs log math numbers os str; do ovt outline $m; echo; done | uvx --with tiktoken python -c "import sys, tiktoken; print(len(tiktoken.get_encoding('o200k_base').encode(sys.stdin.read())))"
```
