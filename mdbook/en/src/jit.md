# JIT: the speculative optimizing tier

`crates/j8jit` is j8's JIT (x86-64 Unix and Windows, AArch64 Linux). It implements the executor's `Tier`: hot functions and hot loops are built
into a **speculative SSA graph** from the inline caches' feedback, optimized, and emitted as x86-64 or AArch64 machine code that
runs directly on the interpreter's frames; when a speculation fails, it **deoptimizes** back to the interpreter.

```text
hot function / loop ─ build (bytecode + inline caches ⇒ speculative graph) ─ opt ─ codegen ─▶ machine code, on the interpreter's frame
a guard fails ─▶ j8_deopt rebuilds the interpreter frames; the hint it leaves shapes the next compilation
```

j8's plan is to **skip a baseline compiler and go straight to a speculative optimizing tier** (inlining, escape
analysis, deoptimization, OSR), aiming to match V8 with its JIT on common benchmarks.

## Turning it on

```rust,ignore
use std::rc::Rc;
let executor = j8executor::Executor::default().with_tier(Rc::new(j8jit::Jit::default()));
```

- `dune --jit` and `J8_TEST262_JIT=1` (test262 runs its baseline with the JIT) turn it on;
- `Jit::from(n)` sets the hotness threshold (default `Jit::THRESHOLD` = 64); 0 compiles every function on its first
  call;
- debugging: `J8_JIT_LOG` (compilations and deopts), `J8_JIT_IR` (prints the graph), `J8_JIT_BIN=<dir>` (writes the
  machine code to files).

## Three entries

| `Kind` | When | How it runs |
|---|---|---|
| `Enter` | the interpreter calls a hot function | on the frame the interpreter built for it |
| `Osr(at)` | loop header `at` has jumped back often enough | on the current frame, entering mid-loop |
| `Native(n)` | compiled code calls compiled code | straight through the callee's trampoline, **no interpreter frame** |

Plain functions (not class constructors, not called with `new`, at most 3 formals) get a native entry: arguments in
registers, int32 results back unboxed through a second return register. Every function has a trampoline
`jmp [rip + cell]` (AArch64: `ldr x16, cell; br x16`): its machine code when it has some, otherwise the interpreter.

Script and eval top-level code runs once: it isn't compiled on entry, only its hot loops are (OSR).

## build: bytecode ⇒ speculative graph

`build.rs` is an **abstract interpretation** of the bytecode: for every interpreter register it keeps the IR value it
holds, speculating on what the inline caches saw.

- **Numbers start as int32**, guarded; a failed guard records a hint, and the next compilation uses doubles there;
- **small functions are inlined** (depth at most 4, bodies of at most 80 instructions, recursion unrolled at most 2
  levels), their frames rebuilt on a deopt; other calls go to compiled code or the runtime;
- **objects stay virtual**: fields of objects made here are only tracked by the compiler until something needs the
  object itself (escape analysis);
- **context slots** read and written here are kept in values and written back only when someone else could look;
- **a loop entered on-stack is peeled once**: what one iteration allocates and the next replaces never exists.

The bytecode is walked region by region (`Region`), each region's blocks in order: forward edges always go to blocks
not walked yet, so a block's state is the merge of its predecessors'; a loop header speculates on its back edges (phis
for everything live), and the walk restarts with a hint when a back edge disagrees (at most 12 times).

`cfg.rs` gives the blocks, loops and liveness: for the analysis, the receiver and parameters are `0 .. P`, r`i` is
`P + i`, the accumulator `P + R`; the context register is always live (not tracked).

## The IR

`ir.rs`: basic blocks in SSA form, each instruction one value with a representation (`Rep`: int32, double, word …).

```text
Block { phis…, insts…, terminator }      a terminator is Jump / Branch / Return / Deopt
Inst  { op, rep }                        operands are value ids
FrameState { func, pc, slots…, parent }  what each interpreter register holds at a guard (virtual objects too)
```

Typical `Op`s:

| Kind | Examples |
|---|---|
| constants and parameters | `I32`, `F64`, `Word`, `Param`, `Callee`, `Phi` |
| arithmetic | `Int(op, a, b, fs)` (with a frame state when it can overflow), `Float`, `IntToFloat` |
| boxing | `BoxInt`, `BoxFloat`, `UnboxInt(v, fs)` (guard: an int32, not −0), `UnboxFloat` |
| guards | `CheckObject`, `CheckShape`, `CheckFunc` (the closure runs a given function), `CheckNotHole` |
| memory | `LoadWord`, `LoadPtr`, `StoreCell` (with a write barrier when needed), `ContextCell` |
| calls | `Call(helper, …)` (runtime functions), `CallJit` (compiled code) |

Every guard carries a `FrameState`: which instruction, and which register contents, the interpreter resumes with.

## opt and codegen

- `opt.rs`: phis that pick one value go away, and so does whatever nothing needs;
- loop-invariant code motion, path facts across loops, constant modulus as multiplication and so on happen while
  building and optimizing;
- `codegen.rs`: **graph ⇒ machine code directly**: linear-scan register allocation over the blocks in reverse postorder,
  then one pass of instruction selection. It is written against x86-64 register roles (RAX … R15) and flag conditions;
  `asm.rs` picks the encoder for the target: `asm/x64.rs` (x86-64) or `asm/a64.rs` (AArch64, which maps each role to
  an AAPCS64 register, emulates x86 flag conditions on NZCV, and moves `x0`/`x1` into the RAX/RDX roles after every
  call). Frame setup and caller-saved spills are the encoder's `enter` / `leave` / `save` / `restore`.

```text
rbp frame   saved rbx r12 r13 r14 r15 at −8 … −40, entry pointers at −48 … −88, spill slots below
r15         the JIT context (Ctx) the whole time
scratch     rax rcx rdx r11 xmm14 xmm15 (never allocated)
allocated   rsi rdi r8 r9 r10 (caller-saved), rbx r12 r13 r14 (callee-saved), xmm0 … xmm13
```

Guards jump to cold stubs that store the frame state's values into `Ctx::BUF` and call `j8_deopt`.

## The runtime

`runtime.rs`: what machine code calls and reads.

```text
Ctx  +0 budget  +8 pending  +16 global  +24 script scope  +32 Object.prototype  +40 ret  +48 ret pc  +56 acc
     +64 depth  +72 depth limit  +128 ARGS (16 words)  +256 BUF (deopt values)
```

- `j8_helper`: slow-path runtime functions (property access, allocation, generic operators);
- `j8_poll`: loops check the budget, termination and interrupt requests; a set request deoptimizes at the check and
  the interpreter handles it, so machine code can be interrupted like the interpreter;
- `j8_deopt`: the deoptimization entry;
- executable memory comes from `mmap` on Unix and `VirtualAlloc` on Windows.

## Deopts and learning

When a guard fails, `j8_deopt` rebuilds the interpreter frames from the frame state (inlined callees included) and the
interpreter goes on. The JIT leaves a hint by reason:

| Reason | Hint |
|---|---|
| int32 speculation failed | doubles there next time (`HINT_FLOAT`) |
| type / shape mismatch | the generic path there next time (`HINT_GENERIC`) |
| the callee was not the one speculated | no inlining + generic (`HINT_NO_INLINE`) |
| bytecode the JIT doesn't compile | that inlined call isn't inlined next time; a function's own code is given up |

The code that failed is dropped and recompiled with the hints when it is hot again. After 24 deopts of one function,
the JIT gives up on it, and it is interpreted from then on.

## The MLIR path

j8jit also exports `Compiler` / `Backend` / `MachineCode` (`compile.rs`, `x86_64.rs`): MLIR (`llvm` dialect) through
[mlirpass](https://crates.io/crates/mlirpass) (`mem2reg`, `canonicalize`, `cse`), then j8jit's own x86-64 back end,
without LLVM. That was the road of the earlier first-tier JIT; the hot path today is the graph ⇒ machine code above,
and lowering V8 bytecode to MLIR is being rewritten.
