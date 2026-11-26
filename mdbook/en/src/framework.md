# Framework: Isolate, Scope and values

`crates/j8framework` is the core layer: the traits the other layers implement and the structs / enums they share. It
**depends on no other j8 layer**; implementations live in the layer crates (`j8platform` implements `Platform`, and so on).

## Core types

| Name | Role | V8 |
|---|---|---|
| `Platform` / `Pool` | the threads and clocks the host lends the engine | `v8::Platform` |
| `Isolate` | an independent JS world: one GC heap (paragc) and a profiler | `v8::Isolate` |
| `Scope` | one entry into an isolate, with a `Context` and the executor; values in it carry the `'scope` brand and can't leave it; all JS operation semantics hang off it, calls back into JS go through `Invoke` | `HandleScope` + `Context::Scope` |
| `Value<'scope>` | a JS value: V8's `Local`, it can't leave the closure | `Local<Value>` |
| `ScopeGlobal<'p, B>` | a root (paragc `ScopeGlobal`): survives `enter` calls, released on drop; `B` names what it holds (`ValueBrand`, `ContextBrand`, …) | `Global<T>` |
| `Context` | the execution context: the global object and built-in prototypes (`Intrinsics`, from j8builtins) | `Context` |
| `Profiler` | call / back-edge counters | — |
| `Host` / `HostObject` | a Rust value inside a JS object, traced by the collector ([Bindings](bindings.md)) | `cppgc` objects |

## Entering an isolate

```rust,ignore
let platform = DefaultPlatform::default();
let mut isolate = Isolate::new(&platform, CreateParams::default());
let executor = Executor::default();
let cx = isolate.create_context(&Builtins);   // ScopeGlobal<ContextBrand>
let result = executor.eval(&mut isolate, &cx, &Source { text: "1 + 2 * 3", file: "a.js" })?;
isolate.enter(&cx, &executor, |scope| {
    let v = scope.local(&result);       // ScopeGlobal ⇒ Value<'scope>, valid only in this closure
});
```

- `Isolate::enter(&cx, &executor, |scope| …)` enters once; `'scope` is that entry's **brand**. Nothing is collected during the closure;
  after it returns, a collection runs if enough was allocated (usually young, a full one every few).
- `Value<'scope>` can't leave the closure, **checked at compile time**: Rust lifetimes stand in for V8's `HandleScope`.
- To keep a value across entries, root it: `scope.global(v)` gives a `ScopeGlobal<'p, ValueBrand>`,
  `scope.mc.stash::<B>(x)` roots any heap object (`ContextBrand` for a context). `local` / `mc.fetch` read them back.
- The executor (`j8executor::Executor<'p>`) runs scripts: `eval` / `eval_module`, or `start` / `resume` in slices.
  Event loops live in j8executor too ([Event loops](event-loop.md)).

## Values: one NaN-boxed word

A `Value` is one 64-bit word (`ValueBits`):

- **numbers** are their own `f64` bits (every NaN is stored as `CANONICAL_NAN`);
- **everything else** lives in the negative NaN space: the top 16 bits are `0xFFF0 | (tag + 1)`, the low 48 bits the
  payload, a bool or a GC address (user-space addresses fit in 48 bits).

The `ValueTag`s: `UNDEFINED`, `NULL`, `BOOL`, `NUMBER`, `UNINIT` (a binding in its temporal dead zone; only in registers
and environment slots, checked away before reads and writes, it never reaches JS), `LAZY` (a property value computed on
first read), `STRING`, `SYMBOL`, `BIGINT`, `OBJECT`, `ACCESSOR`, `CONTEXT`.

"Is it a number", "is it an object" are one compare each, without a full decode. Registers, the accumulator,
environment slots and property slots all hold the same word, so JIT code reads and writes it directly and a `ValueCell`
stores it as is.

```rust,ignore
let v = Value::Number(1.5);
match v.kind() {
    ValueKind::Number(n) => …,
    ValueKind::Object(o) => …,
    _ => …,
}
```

## Scope: operation semantics

A `Scope` is one entry with everything a run needs: the current `Context`, the heap's allocator (`mc`), the executor's
`Invoke`, `new.target` and the pending exception. The spec's abstract operations are its methods (`ops.rs`): conversions (ToNumber, ToString, ToPrimitive …), operators
(`BinaryOp`, `CompareOp`, `UnaryOp`), property access (along the prototype chain, through the
[internal methods](objects.md)). The executor, the built-ins and binding code share them, so semantics are written once.

```rust,ignore
let s = a.to_string(v)?;
let f = a.get_named(obj, "then")?;
let r = a.call(f, this, &[arg])?;
let err = a.throw(ErrorKind::Type, "x is not a function");   // an Error; `?` it out of a host function to throw
```

Built-ins and host functions all have the signature `fn(&mut Scope, this, &[Value]) -> Result<Value, Error>`.

A built-in that calls the same function many times (`map`, `filter`, `reduce`, `sort`'s comparator …) resolves it once
with `a.callback(f, this)` and calls the returned `Callback` in the loop. The engine fills it in through
`Invoke::callback`: with the JIT, a plain function of up to three parameters goes straight to its compiled code (through
the function's stub, so a later compile or deopt is picked up); anything else falls back to `Scope::call`.

```rust,ignore
let cb = a.callback(f, this_arg);
for (k, v) in items {
    let r = cb.call(a, &[v, k.into(), o.into()])?;
}
```

## Error

| Variant | Meaning |
|---|---|
| `Error::Exception` | JS threw; the value is in the pending slot (`scope.take_exception()`), as in V8 |
| `Error::Parked(ticket)` | JS threw, and host code parked the value in the isolate (`scope.park()`) to settle it on a later turn; `scope.unpark(ticket)` puts it back in the pending slot |
| `Error::Syntax(msg)` | the source doesn't parse (early errors included) |
| `Error::Unsupported(msg)` | syntax j8 doesn't support yet, with `file:line:column` |
| `Error::Terminated` | stopped by `IsolateHandle::terminate` |

## Terminating execution

`IsolateHandle::terminate` stops JS from another thread (V8's `TerminateExecution`): the execution layers notice the
request at checkpoints (function entries, back edges; in JIT code, back edges) and throw `Error::Terminated`. It is not
a JS exception: `catch` / `finally` can't catch it, and it goes all the way out. Leaving the outermost entry resets it,
so the isolate stays usable.

## Interrupting execution

An `Interrupter` (from `EventLoop::interrupter` or `Runtime::interrupter`) runs a closure on the event loop's JS thread
from any thread (V8's `RequestInterrupt`): `interrupter.interrupt(|executor, scope| …)` queues it with the executor,
sets the interrupt bit and rings the loop. Running JS sees the bit at the termination checkpoints (every 1024
instructions; in JIT code, back edges), runs the queued closures with the executor and the current scope, and goes on.
JIT code doesn't run them in its own frame: it deoptimizes at the check and the interpreter runs them and continues
from the same instruction, so a closure that runs JS and changes globals or shapes can't break what the machine code
assumed. An idle event loop runs them at the start of its next turn, so a closure doesn't wait for the next JS. A
long-running native builtin (a big `sort`, regular expression backtracking, a big `JSON.parse`) has no checkpoints: an
interrupt waits until it returns.

The termination byte has two uses: its low bits hold the termination state (requested, unwinding) and one more bit is
the pending interrupt. Termination only reads and changes its own bits, so it works as before alongside an interrupt.
With nothing pending the checkpoints read the byte once, as before; both requests are handled behind the branch that a
set byte already takes.

## Heap limit

`EventLoop::set_heap_limit(Some(bytes), callback)` limits the bytes the GC heap holds (paragc's `Heap::used`: blocks plus
large objects). The isolate arms the limit in paragc, which checks it only when it takes a block or makes a large object;
the first one past the limit calls the hook, and the event loop's hook queues an interrupt. On the JS thread
`Scope::check_heap_limit` asks `callback(current, initial)` for the new limit (the shape of V8's near-heap-limit callback,
but called after the limit is passed): JS goes on under it and is terminated over it, where V8 would crash out of
memory. An interrupt served while no JS runs does nothing. At each safe point the isolate runs a full collection once the
heap has grown by half the limit since the last one, then arms the limit again: garbage from earlier tasks does not
count, and a heap whose live data stays large is not collected every turn. Memory outside the GC heap (`ArrayBuffer`
bytes) is not counted.

## Other parts

| File | Contents |
|---|---|
| `promise.rs` | promises and microtasks: resolve / reject, `then` reactions, driving async coroutines, draining the queue |
| `module.rs` | module records, import / export entries, `ResolvedBinding` |
| `evaluation.rs` | module evaluation: the cyclic module record algorithm, top-level `await` and the evaluation promise |
| `proxy.rs` | Proxy: internal-method entry points dispatched to traps, with invariant checks |
| `typed.rs`, `backing.rs` | TypedArray elements; where an ArrayBuffer's bytes live (its own, host-mapped, shared between isolates) |
| `text.rs` | strings (see the [object model](objects.md)) |
| `parallel.rs` | parallel built-ins: plain-data work on large inputs split into chunks on the platform's pool (fork–join) |
| `layout.rs` | byte offsets into heap objects that machine code reads and writes directly |
| `lazy.rs` | properties computed on first read (V8's `AccessorInfo` on function maps: `name` isn't made with the closure) |
| `private.rs` | private class members `#x` |
