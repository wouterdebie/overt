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
| Name "Overt" | States the principle; no existing language with a similar name whose habits would leak in | Cairn (one letter from Cairo, a real language) |

## Memory model

### Representation

| Kind | Layout |
|---|---|
| plain | inline, C layout (fields in declared order, C alignment) |
| `str`, `[T]` | `{buf: *RcBuf, off: i64, len: i64}`, where `RcBuf` is `{rc: i64, flags: u32, cap: i64, data...}` |
| `Map`, `Set` | pointer to a reference-counted, insertion-ordered table: a dense entry array plus a hash index, like Python's compact dict |
| recursive field | pointer to a reference-counted box, inserted by the compiler when the type graph has a cycle |
| enum | tag plus payload union. `?T` is tag plus `T`; niche optimization comes later |
| closure | `{fn: ptr, env: *RcBox}`; captured values are copied into the environment |
| resource | plain layout plus a drop function; move-only |
| `Shared[T]`, `Atomic`, `Chan` | pointer to a box with an always-atomic count. `Shared` holds `{rc, lock, owner_task, T}` |

### Copy, move, drop

- **Copying a heap-backed value** increments its count (`dup`). **Leaving scope** decrements it (`drop`), freeing at zero and dropping children. **A variable's last use** becomes a move with no count update.
- **Mutating a heap-backed value** requires it to be unique. If `rc > 1`, the buffer is cloned first, as a shallow clone whose elements are `dup`ed. `push` works in place when the buffer is unique and has room.
- **Slices** (`xs[a..b]`) are `{buf, off + a, b - a}` plus a `dup`. Writing to a slice makes it unique first, which copies only the slice's range. Known cost: a small slice keeps a large buffer alive.
- **Resource drops** run at scope end in reverse declaration order. Moved-out variables aren't dropped (static drop flags).
- **Later:** Perceus-style reuse analysis and drop specialization.

### Calling convention

| Mode | Passed as | LLVM attributes | Count traffic |
|---|---|---|---|
| `x: T`, plain and 16 bytes or less | registers | | none |
| `x: T`, otherwise | pointer to the caller's value | `noalias readonly nocapture nonnull` | none (a borrow); the callee `dup`s only what it keeps |
| `x: inout T` | pointer | `noalias nonnull` | none |
| `x: sink T` | by value, owned | | the caller moves or `dup`s; the callee must consume or drop |
| return | registers if 16 bytes or less, else `sret` | | owned |

`noalias` is sound because:
- The exclusivity check guarantees an `inout` argument overlaps no other argument.
- There are no mutable globals.
- A `Shared` value can only be mutated inside `lock`, and a task can't lock the same `Shared` twice. The runtime records the owner task and traps on re-entry.

This is also the biggest single enabler for LLVM's auto-vectorizer.

`inout xs[i]` first makes `xs`'s buffer unique (copy-on-write), then passes a pointer to the element. `inout m[k]` passes a pointer to the table slot. Exclusivity guarantees `xs` or `m` isn't touched during the call, so the buffer can't be reallocated.

### Exclusivity check

The check is static and runs per call. An `inout` argument's access path (`p`, `p.x`, `xs[i]`) must not overlap any other argument's path:
- Disjoint fields are fine: `f(inout p.x, p.y)`.
- Two elements of the same array are rejected, because `i != j` can't be proven.

### Values that cross threads

A task owns its data. Non-atomic counting is safe even when the scheduler moves a task to another worker thread, because the handoff itself synchronizes.

Data that becomes reachable from more than one task gets its `flags` marked *shared*, Lean 4 style. Count operations on marked buffers use atomics. Marking happens when a value:
- is put into a `Shared`, `Chan` or `Atomic`
- is captured by a `par` statement, `task.map` or `task.group` closure
- is passed to `g.spawn`

Marking walks the reachable buffers once and stops at any buffer that's already marked.

## Effects and failure

- Effects are checked by the type checker and erased in codegen.
- A failable function lowers to a function returning a tagged `{T | Err}`, where `Err` is `{kind: u8, msg: str}`. `?` is a branch that returns the error. `else` and `catch` are branches to the handler.
- Effect parameters (`!E`) are monomorphized per call site, like type parameters. Inside the generic body, calling an `E`-typed function passes failure on implicitly, since the body can't know whether `E` contains `fail`. After monomorphization with an `E` without `fail`, that branch disappears.
- Standard higher-order functions (`map`, `filter`, `task.map`, ...) are declared with effect parameters.

## Traps

- **v0:** write the message and `file:line:col` (later a backtrace) to stderr, then `abort()`.
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

The compiler is written in Rust. Its stages:

1. **`lex`**
2. **`parse`**: a lossless syntax tree with spans, which `fmt`, `put` and `outline` also use.
3. **`resolve`**: modules, names, naming rules, shadowing.
4. **`check`**:
   - type inference inside bodies, generics, effects
   - parameter modes, exclusivity, exhaustiveness, unused results
5. **`lower`**: Overt IR, with explicit `dup`/`drop`/move, monomorphized, failure as branches.
6. **`llvm`**: textual LLVM IR.
7. **clang**: compiles the IR and links.

Every diagnostic has a code, a span, a one-line message and an optional fix (a text edit); `--json` gives machine output. `outline`, `put`, `q`, `fmt` and `test` run on the same front end. The compiler's own tests are `.ovt` programs with expected output or expected errors (golden files).

## Build order

The milestones in [ROADMAP.md](ROADMAP.md) set the build order. Each milestone lists the compiler, runtime and standard library work it needs.

## Open questions

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
- Unchecked arithmetic for hot loops, if overflow checks block vectorization in practice.
- Callbacks from C.
- Linux: epoll or io_uring, and an x86_64 context switch.

## Measuring the spec budget

```
uvx --with tiktoken python -c "import tiktoken; e = tiktoken.get_encoding('o200k_base'); print(len(e.encode(open('SPEC.md').read())))"
```

`o200k_base` is a proxy, since Claude's tokenizer isn't available offline. Treat the number as relative: SPEC.md must stay under 5,000 by this measure. At v0 it's about 4,800.
