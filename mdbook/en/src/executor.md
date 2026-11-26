# Executor: interpreting bytecode

`crates/j8executor` runs [V8 bytecode](bytecode.md): `Executor<'p>` is the execution entry, and the event loops live
here too. It is j8's last layer for now: it always carries the [compiler](compiler.md), and scripts, modules and `eval` /
`Function` are all compiled to bytecode before they run.

## One executor per isolate

An `Executor` holds the loaded functions (`Loaded`), and closures on the heap point back to them by `FuncId`. So an
executor serves one isolate only; isolates share compiled code through a `CodeCache` (below).

## Frames and registers

- **An explicit frame stack**: JS calling JS doesn't recurse on the host (Rust) stack; the executor keeps its own frame
  stack. The call depth limit is 10,000 (counting JS re-entered from host functions); beyond it, `RangeError`.
- **The host stack has a limit of its own, in bytes**: the depth limit counts levels and guards the executor's frame
  stack on the heap, but a host function that calls back into JS (`join` → `toString` → `join`, a Proxy trap,
  `Function.prototype.call`) or recurses itself (`JSON.stringify` / `JSON.parse` / the reviver, `Array.prototype.flat`,
  Proxies nested without traps) uses the host stack, and how much each level takes varies. `Scope::check_stack` throws
  `RangeError` once the stack is `CreateParams::stack_size` below where the isolate was entered (default 984 KiB, V8's
  `--stack-size`). It runs where host code calls into JS (`Invoke` call / construct / resume), at each Proxy trap
  lookup, and in the recursive steps above. The thread that enters the isolate needs that much stack free below the
  point of entry, plus some room for the host function that is running when the check fails: 1 MiB with the default.
- **One register file** shared by all frames. A frame owns a stretch of it: the receiver, the formal parameters, the
  registers from r0. Register operands in the bytecode are V8 frame slots (r0 is −7, the receiver +2, the context −1,
  the closure −2) and are mapped onto that stretch at load time.
- **Contexts** are heap `Env`s: registers and the accumulator carry them as `Value::context` words between
  `CreateFunctionContext` / `PushContext` / `PopContext`.

## Threaded code

A function's bytecode is decoded once, at load time, and every decoded instruction carries the **handler** that runs
it (`Handler` in `threaded.rs`):

```text
handler(cx, pc, regs, acc)  ─ done ─▶  jumps straight to the next instruction's handler
```

Handlers get the hot state (the instruction, the frame's r0, the accumulator) in machine registers. With a nightly
compiler the jump is a guaranteed tail call (`become`; `build.rs` turns on `cfg(tail_calls)` when it sees a nightly);
on stable it returns to the loop in `Executor::run`, which calls the next one.

Hot bytecodes have handlers of their own; the rest, and hot ones off their fast path, go through `slow`, which syncs the
state and runs the general `Executor::step`. The chain is left when the frame changes (calls and returns), on an
exception, or when the execution ends.

The handler sits in a `Cell`: the [debugger](debugger.md) swaps it for a breakpoint and swaps it back, with no check on
the instructions it leaves alone.

## Inline caches

`ic.rs`: one cache per `GetNamedProperty` / `SetNamedProperty` / `DefineNamedOwnProperty` site.

- **Keyed only by shape address**: the GC moves objects, so a cache must not hold object pointers; it also holds a
  `Weak` so a recognized address is not reused by another shape.
- Dictionary shapes mutate in place (same address, different contents), so they are never cached.
- Read: a data property owned by the receiver, or by a prototype 1 to `DEPTH` levels up (the shape of every level on
  the way is checked).
- Write: a writable data property owned by the receiver.
- Add property: old shape ⇒ new shape; assignments also check the shape of every level of the prototype chain (nobody
  has since added a setter / read-only property of the same name).

The caches are also the [JIT](jit.md)'s **type feedback**: it speculates on the shapes they saw and emits shape guards
plus direct slot loads and stores.

## Sliced execution

`Executor::eval` takes `&mut Isolate` and enters it in slices: at most ten thousand instructions each; between slices the
execution state is rooted, the heap gets to collect, and the run continues. The environment survives slices as a
`ScopeGlobal<'p, ContextBrand>`. Every 1024 instructions the budget, termination and interrupt requests are checked; inside an event loop that is
when the task queue is checked ([Event loops](event-loop.md)).

Known deviation: JS re-entered from a host function (`map`'s callback) runs to completion inside the current slice,
without collection. `Function.prototype.call` / `apply` are unrolled by the executor into ordinary calls and don't count
as re-entry.

## Exceptions

The bytecode's **handler table**: a throw looks for an entry covering the current offset in the frame; if found, it
restores the context register and jumps to the handler, otherwise it unwinds to the caller.

## Generators and async

Generators, async functions and async generators share the same bytecodes (`SuspendGenerator` / `ResumeGenerator` /
`SwitchOnGeneratorState`); suspending saves the registers into the generator object. `await` drives the coroutine
through `j8framework`'s promise machinery; after a script, the microtask queue is drained.

## ES modules

`module.rs`: loading and linking (synchronous: dependencies are read through `Platform::load_module`); evaluation,
including top-level `await`, is `Scope::evaluate_module` in j8framework. Import bindings read the exporter's slots live;
`ResolveExport` handles indirect exports, `export *` and ambiguity; namespace objects, dynamic `import()` and
`import.meta` are supported.

## CodeCache

When many isolates run the same preludes and import the same module files, give their executors one shared
`CodeCache`:

- each source is parsed and compiled once, and every executor loads its own copy;
- only what is meant to be shared goes in (scripts run as `ScriptKind::Shared`, and ES modules); one-off scripts don't;
- no source text is kept: entries are found by file name, length and a 128-bit hash of the text (two randomly keyed
  SipHash, so collisions can't be crafted).

```rust,ignore
let code = CodeCache::shared();
let executor = Executor::default().with_cache(Arc::clone(&code));
```

## The JIT hook

`Executor::with_tier(tier)` attaches a `Tier` ([JIT](jit.md)). The executor asks it:

| When | Call |
|---|---|
| a call is about to run a frame from its first instruction (asked every 64 calls) | `Tier::enter` |
| a `JumpLoop` has been taken many times | `Tier::osr` |
| a frame continues mid-way (after a pause, or a hand-over from machine code) | `Tier::resume` |
| an ordinary call between compiled functions, without an interpreter frame | `Tier::call` |

`Interpret` means interpret as usual; `Resumed` means machine code ran and the frames were set with `Entry::resume`, so
the interpreter continues with the top one; `Threw` means the machine code threw. `jit.rs` defines everything a JIT sees:
the decoded bytecode and its feedback (`Function`), the frame about to run (`Entry`), and how machine code hands an
execution back (`DeoptFrame`).

## Use

```rust,ignore
let executor = j8executor::Executor::default();
let result = executor.eval(&mut isolate, &cx, &j8framework::Source { text: "1 + 2", file: "a.js" })?;
```

The `javascript_semantics` test is a table of exactly determined JS semantics.
