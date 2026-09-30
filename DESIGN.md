# Overt design notes

[SPEC.md](SPEC.md) is the language as an agent writing Overt code sees it; that file goes into the agent's context and has a token budget. This file holds the rest: why each decision was made, what the compiler and runtime must do to implement it, and what is still open. Nothing here is needed to *write* Overt, so it isn't budgeted.

## The hypothesis

A coding agent working in Overt, with only SPEC.md and the toolchain, should build network servers with fewer tokens and fewer compile-fix rounds than it needs in Rust or Go. The result should be equally correct and comparably fast. The milestones in [ROADMAP.md](ROADMAP.md) test this: each one's program is also written in Rust and Go by fresh agents. If Overt doesn't win, we still learn which of these intuitions were wrong.

## Principles

1. **Learnable from the spec alone.** No model has training data for Overt, so SPEC.md and the std outline are everything it knows. The budgets are **6,000 tokens** for SPEC.md and **12,000** for SPEC.md plus the std outline, measured with `o200k_base` as a proxy (see the end of this file). A feature has to be worth the tokens it adds.
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
| Named arguments when parameter types repeat, except literals | Swapped `(src, dst)` is a classic agent bug. A literal can't be a swapped variable, and `rotr(x, 7)` cost a failed build in five of six Overt `hashdir` runs before literals were exempt | Always named (too many tokens); never named; naming literals too |
| Every arithmetic and bit operator has an assignment form (`+%=`, `^=`, `<<=`, ...) | C-family agents expect it; `h[0] +%= a` cost a `hashdir` run a round | Only `+= -= *= /= %=` |
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
| `!` on an integer flips its bits | Hashing needs bitwise NOT (SHA-256's choose function), and agents coming from Rust write `!x` | `~x` (one more operator); no NOT at all (`x ^ 0xffffffff`) |
| `Atomic` operations are `! io` | Another task can change the value, so a function that reads one isn't pure | Pure atomics |
| `fs.walk` and `fs.list` return entries sorted by path | Deterministic output, and a listing like `hashdir`'s needs no sort | Directory order |
| `net.Conn` is a shared handle | A chat server has one task reading a connection and another writing it; a move-only connection would need Rust-style split halves. Resources with `drop` arrive with C interop (milestone 5) | A move-only resource |
| `Dur` scales by `int` (`n * 1s`, `d / 2`) | A timeout often comes from a number, like a command-line argument | Functions like `time.seconds(n)` |

## Memory model

### Representation

| Kind | Layout |
|---|---|
| plain | inline, C layout (fields in declared order, C alignment) |
| `str`, `[T]` | `{buf: ptr, off: i64, len: i64}`, a view of a buffer with a 24-byte header `{rc: i64, cap: i64, used: i64}` followed by the elements. `used` is how many elements the buffer holds, since a view may show fewer. As in Lean 4, `rc > 0` is a count owned by one task, `rc < 0` a shared count updated atomically (see below), and `rc == 0` a static buffer that is never freed (string literals, and array literals of numbers). The empty array is `{null, 0, 0}` |
| `Map`, `Set` | plain Overt structs in `std/collections.ovt`: parallel arrays of keys, values and live flags, plus an open-addressing slot table of indexes, like Python's compact dict. They get copy-on-write from their arrays |
| recursive field | a pointer to a box `{rc: i64, value}`, inserted by the compiler when a type contains itself without an array or other indirection in between |
| enum | `i32` when no variant has fields; otherwise `{i32 tag, [N x i64] payload}`, with each variant's fields laid out as a struct in the payload |
| `?T` | `{i1 some, T}`; niche optimization comes later |
| closure | `{fn: ptr, env: ptr}`. The environment is `{rc: i64, drop: ptr, mark: ptr, captures...}` holding copies of the captured values; `drop` frees it and `mark` marks the captures shared, so neither needs the closure's type. Named functions used as values get a thunk and a null environment |
| resource | planned (milestone 5): plain layout plus a drop function; move-only |
| shared handles: `Atomic`, `Shared[T]`, `Chan[T]`, `net.Conn`, `net.Listener`, `task.Group` | a std struct holding one closure-like value, `{fn: null, env}`, whose environment is a runtime object that starts with the environment header `{rc, drop, mark}`. Copies share the object, which gets counting and marking the way closures do, and is freed with the last copy |

### Copy, move, drop

- **Owned or borrowed.** Codegen evaluates every expression as either owned (the holder must drop it) or borrowed (valid while its owner lives). Copying a borrowed value into a place that keeps it (a variable, a field, a `sink` argument, a return) increments its counts (`dup`). Owned temporaries that are only read are dropped at the end of their statement.
- **Cleanup stack.** Locals and temporaries that need dropping are registered on a stack of scopes. Leaving a scope normally drops its entries; `return`, failure, `break` and `continue` drop every scope they leave.
- **Mutating a heap-backed value** requires its view to be the only one and to cover the whole buffer (`rc == 1`, `off == 0`, `len == used`); otherwise the viewed range is copied first, with its elements `dup`ed. `push` and `s += t` then work in place, growing the buffer by doubling.
- **Slices** (`xs[a..b]`, `s[a..b]`) are `{buf, off + a, b - a}` plus a `dup`. Known cost: a small slice keeps a large buffer alive.
- **Literals.** An array literal whose elements are all number literals is static data (count 0), like a string literal, so a constant table like SHA-256's costs nothing to use; changing a copy copies it first. Before this, every use of `const K: [u32] = [...]` built the array again, and SHA-256 ran at 4 MB/s.
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

Data that becomes reachable from more than one task is marked *shared* by negating its count, Lean 4 style:
- **When.** Before `task.map` starts, its array and its closure are marked. Before `par` starts, each statement's closure is, and its environment holds copies of what the statement uses from outside. Later: values put into a `Shared` or `Chan`, and closures given to `task.group` or `g.spawn`.
- **How.** A generated `mark` helper per type walks the reachable buffers, boxes and environments once. It negates positive counts, and stops at any count that's already shared or static.
- **Counting.** Generated code loads a count with an atomic (relaxed) load, which is a plain load on arm64. A positive count changes inline with plain stores; any other count goes to the runtime, which changes shared counts atomically. Copy-on-write treats a shared buffer as not unique, except at count -1, the last reference, which makes it owned again.
- **Checked.** With marking turned off, ThreadSanitizer reports races on the counts of strings that tasks copy, and the program crashes. With it on, `hashdir` and the concurrency tests are clean.

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
- **Stack overflow** hits the task stack's guard page. A SIGSEGV handler on each worker's alternate signal stack turns it into `trap: stack overflow (recursion too deep?)`. It isn't installed under the sanitizers, which have their own.
- **Later:** a trap fails only its task, using LLVM landing pads for unwinding, so one bad request only kills its connection. This needs a policy for locks poisoned mid-update.

## Concurrency runtime

The runtime is C, in `runtime/rt.c`, compiled with every program. Milestone 2 built tasks, workers, `par`, `task.map`, `Atomic` and the blocking pool; milestone 3 added the poller, sockets, timers, cancellation, `Shared`, `Chan`, `task.group` and `task.timeout`.

- **Tasks.** `main` is the first task, and every Overt function runs in one. Tasks are stackful coroutines, so a borrowed pointer stays valid across a wait, which avoids the whole class of Rust `Pin` and self-referential future problems.
- **Workers.** One OS thread per core; `OVT_WORKERS` overrides the count.
  - Worker 0 is the process's main thread.
  - The others start the first time a task is spawned, so a sequential program stays single-threaded, and `ovt test` can still fork a child per test.
- **Run queues.**
  - Each worker has a FIFO run queue behind a mutex. A global queue takes tasks woken from threads that aren't workers.
  - An idle worker takes from its own queue, then the global one, then steals from the others. When all are empty it sleeps on a condition variable.
  - v0 uses mutexes rather than Chase-Lev deques, since tasks are coarse and ThreadSanitizer checks mutexes exactly. Revisit this if profiles show contention.
- **Task stacks.**
  - A task's stack is 256 KiB of virtual memory, whose lowest page (16 KiB on arm64 macOS) is a guard page. `main` gets 8 MiB, like a main thread. The OS commits pages only when they're touched.
  - Finished tasks' stacks are pooled (up to 64) and `madvise(MADV_FREE)`d.
  - Stacks never move, because borrowed parameters point into them, so there are no growable stacks.
  - An idle task costs its touched pages, roughly 16–32 KiB with 16 KiB pages.
- **Context switch.** Hand-written arm64 assembly that saves x19–x30, sp and d8–d15. Other architectures don't build yet.
- **Waiting.** A task waits on a *waiter* in its own stack frame, and wakes exactly once:
  - The task switches to its worker's scheduler, which marks the waiter parked with a compare-and-swap.
  - The waker's last access is an exchange that marks the waiter done. If the waiter was parked, the waker puts the task back on a run queue.
  - The frame stays valid until the task has seen "done", so a waker never touches freed memory. A first design kept a park flag on the task instead, and a helper could reach a finished task or a parent's returned frame.
- **Structured scopes.** `ovt_parallel(n, body, ctx)` runs `body(ctx, i)` for every index, and `task.map` and `par` compile to it.
  - The work runs on up to one task per worker, including the calling task. Indexes are handed out one at a time.
  - After a failure, no new index starts, and the error of the lowest failing index is passed on. Indexes are handed out in order and a started body finishes, so that choice is deterministic.
  - The other results are dropped. Bodies already running are cancelled: their next `io` call fails.
- **Blocking pool.** File IO runs on up to 64 extra threads while the task waits, so a worker never sits in the kernel. With only one task alive there's nothing else to run, so the call runs inline instead.
- **`Atomic[int]`.** `load`, `store` and `add` are sequentially consistent atomic operations on the number.
- **Sanitizers.** Context switches are annotated for ThreadSanitizer (fibers) and AddressSanitizer (stack switching), and a stack is unpoisoned before reuse. `OVT_CFLAGS="-fsanitize=thread -g"` builds a program with ThreadSanitizer.
- **The poller.** One thread runs kqueue for every task waiting on a socket or a timer.
  - A waiting task hands it a request and parks on a waiter in its own frame. The poller wakes it once, with ready, timed out or cancelled.
  - Requests belong to the poller from then on, and only its thread touches them, so an fd event, a deadline and a cancellation can't both wake a task.
  - Timers are a heap in the poller, which uses the nearest deadline as kqueue's timeout. `time.sleep`, read and write timeouts and `task.timeout` all use it.
  - Epoll and io_uring on Linux come later.
- **Cancellation.** Every task is in a cancel scope, and cancelling a scope cancels the scopes below it.
  - A cancelled task's next failing `io` call fails with `.Cancelled`. A task parked on the poller is woken to fail, since the poller wakes every waiting request in a cancelled scope.
  - `task.timeout` runs its function in a child task in a new scope, with a deadline that cancels it. It fails with `.Timeout` once the deadline has passed, whatever the child did afterwards.
  - `task.group` cancels its tasks if its body fails. `par` and `task.map` cancel the others after the first failure.
  - Not yet: a task waiting in `Chan.recv` or for a task lock isn't woken by cancellation, and a task that never calls `io` can't be cancelled at all.
- **Sockets.** Non-blocking, with `SO_NOSIGPIPE`, and `SIGPIPE` ignored.
  - Each connection has a read buffer for `read_line`, allocated when a read needs one and freed while the connection waits with nothing buffered. That's what keeps idle connections small.
  - Reads and writes each hold a task lock, so lines written by several tasks don't interleave.
  - `close` shuts the socket down at once, which wakes waiting tasks. The fd is closed with the last copy of the handle, since closing it while the poller watches it would leave a task waiting forever.
  - The runtime raises the open-file limit to the maximum at startup.
- **Task locks.** When a task waits for the lock, it parks instead of blocking its worker thread, and unlocking hands the lock to the next waiter in order. The holder may move between threads while it waits inside the lock, which rules out `os_unfair_lock` and pthread mutexes: they must be unlocked by the thread that locked them.
- **`Shared[T]`.**
  - `lock` takes the value's task lock, and traps if the task already holds it.
  - The checker rejects `io`, failures passing out, `return`, and `break` or `continue` out of the block, so the block always ends by unlocking.
  - Unlocking marks everything the value reaches shared again, since the block may have put new values into it or copied values out. The walk stops at counts that are already shared, so it costs about the part of the value that changed.
- **`Chan[T]`.** A ring buffer that grows as needed up to its capacity, with queues of parked senders and receivers. A value is moved in by `send`, which marks it shared first, and out by `recv`. Closing wakes everyone: receivers get what's left and then `none`, and senders fail with `.Unavailable`.
- **`task.group`.** The body runs in the calling task, and `g.spawn` starts a task running a closure. Its environment is marked shared, and the task holds a reference to it. The group waits for all of them.
- **Measured on `chat`** (milestone 3's reference server, two tasks per client):
  - 34 KiB of memory per idle connection. That's one 16 KiB stack page each for the session and writer tasks, plus a little heap.
  - Before read buffers were freed while idle and channel buffers grew on demand, an idle connection took 120 KiB: every `Chan[str].new(10_000)` allocated its whole ring.
  - Message latency to a 100-client room: p50 0.85 ms, p99 3.5 ms. A single-threaded Python asyncio server gets p50 0.64 ms and p99 1.0 ms: each message here wakes 99 writer tasks across worker threads, through run queues and sometimes a sleeping worker.
- **Blocking C calls (milestone 5).** `blocking` extern calls run on the blocking pool while the task parks. Non-blocking extern calls run on the task stack, so heavy C work should be marked `blocking`.

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
4. **`codegen`**: textual LLVM IR. Each function is emitted once per set of type arguments, from a work queue, along with per-type helpers (`helpers.rs`), `match` (`pat.rs`), intrinsics including `task.map` (`intrin.rs`) and `par` (`par.rs`).
5. **clang**: compiles the IR together with the runtime (`runtime/rt.c`, embedded in `ovt`).

Diagnostics are one line each, with the fix in the message when one is known. Planned: `--json` output, and `ovt put` and `ovt q` on the same front end.

The compiler's tests (`cargo test`) are golden files:
- programs with their expected output, traps and exit status (`tests/run/`)
- programs with their expected errors (`tests/errors/`)
- outlines (`tests/outline/`)
- the `ovt` code blocks in SPEC.md, which must parse and be canonical
- the standard library's `ex` lines (`ovt test --std`)
- reference versions of the task programs (`tests/programs/`), which must pass the tasks' tests

Set `OVT_CFLAGS="-fsanitize=address -g"` to build any program with AddressSanitizer, or `-fsanitize=thread` for ThreadSanitizer.

## Standard library

The standard library (`std/`) is written in Overt, and embedded in `ovt`. Functions declared without a body are intrinsics, implemented in `codegen/intrin.rs` over the runtime. Files like `str.ovt`, `collections.ovt` and `atomic.ovt` declare methods and types visible everywhere; `os.ovt`, `fs.ovt`, `math.ovt`, `log.ovt` and `task.ovt` are modules. `ovt outline <name>` shows a file's declarations and docs, and hides private (`_`) names. The same outlines are what agents read, so the docs in std/ are part of the language's interface.

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

### `hashdir` in Overt, Rust and Go, three runs each (protocol v4, milestone 2)

- **Result:** all nine programs passed all 21 tests, and all hash in parallel. Medians:

  | | Overt | Rust | Go |
  |---|---|---|---|
  | tool calls (to first pass) | 8 (6) | 7 (6) | 4 (3) |
  | input tokens (to first pass) | 448k (282k) | 276k (196k) | 163k (88k) |
  | output tokens (thinking, roughly) | 7.1k (3.8k) | 8.8k (3.9k) | 6.5k (2.7k) |
  | cost | $0.50 | $0.37 | $0.29 |
  | failed builds | 1 | 0 | 0 |
  | program (o200k tokens) | 2.1k | 3.5k | 2.5k |
  | 520 MB tree: wall, CPU | 0.19 s, 2.5 s | 0.13 s, 1.8 s | 0.18 s, 2.4 s |
  | docs: tokens, share of input | 14.9k, 29% | 0 | 0 |

- **A first batch was set aside.** `tests/run.py` ran the program from inside each case's directory, so the command the prompt gives, `python3 tests/run.py bin/hashdir`, failed with a relative path. Seven of nine agents lost a round to it, and it counted as a failed build. The tests now make the path absolute. The batch is kept in `bench/results/02-hashdir.test-bug.jsonl`, and its real stumbles are counted below.
- **Programs are smallest in Overt:** 40% smaller than Rust's and 17% smaller than Go's. `task.map` over `fs.walk` replaces the Rust and Go agents' thread pools, channels and hand-written directory walk. The extra thinking seen on `jsonfmt` didn't appear.
- **Speed is close:** Overt matches Go and is 1.5 times Rust's time, with the work spread over 13 of 16 cores in all three languages.
- **Correctness is where Overt loses.** Overt was the only language with failed builds, and every one came from an Overt rule:
  - **The named-argument rule on `rotr(x, 7)`:** five of six Overt runs across both batches, plus two slips of mine while writing the reference and tests. Every time, the unnamed argument was a literal.
  - **`h[0] +%= a`:** there's no compound form of the wrapping operators (one run).
  - **`[u32; 64]`:** SPEC.md lists fixed-size arrays, which are planned for milestone 5 (one run).
  - **`task.map(files, |p| hash_file(p))?`:** the call inside the closure needs its own `?`. The error didn't say so, and a second, confusing error followed on the outer `?` (one run).
- **Fixed since:** literals needn't be named; `+%=` and the other operator assignments exist; `[T; N]`'s error suggests `[T]`; an unhandled failure in a closure says to use `?` there, and no longer causes a second error at the caller.
- **The hypothesis, again:** Rust agents wrote correct concurrent code on their first build. The borrow checker never got in the way, because the agents chose scoped threads and channels. The rounds Overt is meant to save didn't exist here, and the rounds it cost came from its own strictness.

## Open questions

- Chase-Lev deques instead of mutex run queues, if the mutexes show up in profiles.
- Lower wake-up latency: poll from idle workers instead of a separate poller thread, and spin briefly before sleeping.
- Cancelling a task that waits in `Chan.recv` or for a task lock.
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

`o200k_base` is a proxy, since Claude's tokenizer isn't available offline. Treat the number as relative. After milestone 3, SPEC.md is 4,987 tokens by this measure.

Agents also get the outline of every std module in their prompt (see the benchmark findings), so its size counts too: about 3,200 tokens after milestone 1, and about 4,900 after milestone 3.

`cargo test` (`docs_fit_their_budgets`) checks SPEC.md against 6,000 and SPEC.md plus the outline against 12,000, with tiktoken through `uvx`, or an estimate that errs high without it.

**Why these budgets.** The first cap was 5,000 tokens for SPEC.md alone, a judgment call made before any benchmark ran. It kept the always-loaded document small and forced the language to stay small. Raised after milestone 3, for three reasons:
- **The cap measured only part of what agents get.** They get the outline too, and it had no budget.
- **Tokens of docs are cheap.** A fresh session writes them to the cache once, about $0.01 per 1,000 tokens at these runs' prices, and rereads cost little.
- **Rules cost more than tokens.** On `jsonfmt`, Overt agents spent two to three times as many tokens thinking as Rust agents, which cost more than carrying the docs. So a feature is judged by what it costs to reason about, and the cap stays as a check against drift.

The outline is made like this:

```
for f in std/*.ovt; do ovt outline $(basename $f .ovt); echo; done | uvx --with tiktoken python -c "import sys, tiktoken; print(len(tiktoken.get_encoding('o200k_base').encode(sys.stdin.read())))"
```
