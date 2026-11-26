# Embedding j8: building a host like dune / workerd

This guide shows how to build your own JavaScript runtime on the `j8` crate, the way
[`dune`](https://github.com/0x00-Community/j8/blob/main/dune) and [`samples/workerd`](https://github.com/0x00-Community/j8/blob/main/samples/workerd) are built. It covers how the pieces fit
together. The attribute-by-attribute reference for the binding macros is
[`crates/macros/USAGE.md`](https://github.com/0x00-Community/j8/blob/main/crates/macros/USAGE.md).

## 1. Setup

```toml
[dependencies]
j8 = "0.4"                                          # default feature `runtime`: executor + built-ins + platform
# j8 = { version = "0.4", features = ["jit"] }      # + the x86-64 JIT (x86-64 Unix and Windows)
# j8 = { version = "0.4", features = ["json"] }     # + j8::Json<T> (serde)
```

What you use from it:

| Path | What it is |
|---|---|
| `#[j8::object]` / `#[j8::class]` / `#[j8::function]` / `#[j8::module]` | Bind Rust to JS |
| `j8::runtime!` | Declare a runtime: which bound types go where, plus a JS prelude |
| `j8::Runtime` | A ready execution environment: `eval`, `eval_module`, `run_until_idle`, `set`, `text`, `enter` |
| `j8::Host` / `j8::HostPlatform` | The trait `runtime!` implements / the platform that serves its native modules (stateless: runtimes may share one) |
| `j8::frame` | `j8framework`: `Scope`, `Error`, `ErrorKind`, `Value`, `Platform`, `Source` … |
| `j8::platform::DefaultPlatform` | The standard-library platform: clock, `console.log` to stdout, ES modules from files |
| `j8::executor::Executor` | The bytecode executor; `with_tier` adds the JIT |
| `j8::Promise<T>` | A JS promise awaited from Rust (see [5.4](#54-awaiting-a-js-promise-from-rust)) |
| `j8::Glue` | `Glue::spawn` / `Glue::settle`: hand-made promises (see [5.3](#53-a-method-that-returns-a-promise)) |
| `j8::NATIVE` | `"__j8_native"`, the name of the hidden registry object |

## 2. The smallest host

```rust,ignore
use std::num::NonZeroUsize;
use std::process::ExitCode;

use j8::executor::Executor;
use j8::frame::{Scope, Error, ErrorKind};
use j8::platform::DefaultPlatform;

/// Machine access, kept in `__j8_native.env`
struct Env;

#[j8::object("env")]
impl Env {
    fn var(name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    /// Throws a TypeError when the variable is missing
    fn must(a: &mut Scope<'_, '_>, name: &str) -> Result<String, Error> {
        std::env::var(name).map_err(|_| a.throw(ErrorKind::Type, &format!("{name} is not set")))
    }
}

/// The public API is JavaScript, built on the natives
const PRELUDE: &str = r#"
(function (native) {
  'use strict';
  globalThis.home = () => native.env.var('HOME');
})(globalThis.__j8_native);
"#;

j8::runtime! {
    pub Hello {
        natives: [Env],
        prelude: PRELUDE,
    }
}

fn main() -> ExitCode {
    let platform = Hello::platform(DefaultPlatform::default());
    // One loop thread is enough for one runtime; give more for many
    j8::EventLoopSet::scope(&platform, NonZeroUsize::MIN, |set| {
        let rt = match Hello::runtime(set, Executor::default()) {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("prelude failed: {e}");
                return ExitCode::FAILURE;
            }
        };
        match rt.eval("home()", "main.js").and_then(|g| rt.run_until_idle().map(|()| g)) {
            Ok(g) => {
                println!("{}", rt.text(&g));
                ExitCode::SUCCESS
            }
            Err(Error::Exception) => {
                eprintln!("Uncaught {}", rt.exception_text());
                ExitCode::FAILURE
            }
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        }
    })
}
```

The steps are always the same:

1. `X::platform(inner)` wraps a platform in a `HostPlatform` for runtime `X`. Keep it alive for as long as the
   runtime: `Runtime<'p>` borrows it.
2. `j8::EventLoopSet::scope(&platform, threads, |set| …)` starts the loop threads the runtimes run on, for the length
   of the closure.
3. `X::runtime(set, executor)` creates the isolate and the context on the least loaded loop thread, installs
   everything and runs the preludes. A prelude that throws comes back as `Err` here. Every later call (`eval`,
   `enter`, `run_until_idle`) is a message to that thread, and waits for its answer; any host thread may make it.
4. `rt.eval(text, file)` runs a script. It returns a `ScopeGlobal<'p, ValueBrand>` (a root that outlives the
   call), which `rt.text(&g)` turns into a string with ToString.
5. `rt.run_until_idle()` waits until no async host work is left. Call it after every `eval` if your
   bindings have `async fn`s or return promises; otherwise those promises never settle. A spawned job made
   with `AsyncJob::new(..).unref()` (such as an unref'd Node-API thread-safe function) still settles, but is not waited for.

### What an `Error` means

| Variant | Meaning | What hosts do |
|---|---|---|
| `Error::Exception` | JS threw; the value waits in the isolate's pending slot, as in V8 | `rt.exception_text()` ⇒ `Uncaught …` with the stack ([4.4](#44-stack-traces)) |
| `Error::Syntax(msg)` | the source does not parse | print `msg` |
| `Error::Unsupported(msg)` | syntax j8 does not support yet | print `msg` |

## 3. Rust bindings + a JS prelude

Both samples split the work the same way:

- **Rust** does only what needs the machine (files, sockets, clocks, processes) and keeps it small and flat.
- **JavaScript** (the prelude) builds the real API on top: classes, argument checking, Node/Web semantics. It is
  much less code than doing the same through bindings.

`runtime!` has four sections. Every one is optional, and they must come in this order, each followed by a comma:

| Section | Takes | Ends up at |
|---|---|---|
| `globals: [..]` | `#[j8::class]` / `#[j8::object]` types | the global object: `globalThis.Process` |
| `modules: [..]` | `#[j8::module("node:x", "x")]` types | `import … from "node:x"` and `__j8_native["node:x"]` |
| `natives: [..]` | `#[j8::class]` / `#[j8::object]` types | the hidden registry: `__j8_native.env`, `__j8_native.KvStore` |
| `prelude: EXPR` | a `&'static str` of JavaScript | run once, as a script, after everything is installed |

The JS name is the macro argument (`#[j8::object("env")]` ⇒ `env`). Without one, it is the Rust type name
(`#[j8::class] impl KvStore` ⇒ `KvStore`). Methods are renamed to camelCase (`exists_sync` ⇒ `existsSync`) unless
`#[js(name = "…")]` says otherwise.

`__j8_native` is a non-enumerable property of the global object. User code can still reach it, so treat it as hidden
from casual use, not as a security boundary. The conventional prelude shape is an IIFE that takes it:

```js
(function (native) {
  'use strict';
  const { clock, Timer } = native;      // natives bound in Rust
  globalThis.setTimeout = (f, ms) => { /* … */ };
  native.runMain = (file) => { /* … */ }; // entry points the host calls (see 3.2)
})(globalThis.__j8_native);
```

A long prelude can be split into several files and joined with `concat!(include_str!("a.js"), include_str!("b.js"))`,
as `dune/src/node/mod.rs` does.

### 3.1 Passing data into a run: `Runtime::set`

`rt.set("a.b.c", value)` defines `c` on the object found at `globalThis.a.b`, converting `value` with `IntoJs`.
Anything that implements `IntoJs` works: numbers, strings, `Vec<T>`, `BTreeMap<String, T>`, `Option<T>`…

```rust,ignore
rt.set("process.argv", args)?;                                        // Vec<String> ⇒ array
rt.set("process.env", std::env::vars().collect::<BTreeMap<_, _>>())?; // map ⇒ object
```

The parent object must already exist (the prelude creates it). This is how `dune` sets `process.argv` and friends
without the prelude knowing about Rust.

### 3.2 Calling into the prelude

To start something the prelude defines, put a function on `__j8_native` and `eval` a call to it. Quote the
arguments as JS string literals (see `Session::quote` in dune):

```rust,ignore
let call = format!("__j8_native.runMain({})", quote(file));
rt.eval(&call, file)?;
rt.run_until_idle()?;
```

For lower-level work use `rt.enter(|a| …)`, which gives you the `Scope` of the context (`a.get_named`, `a.call`,
`a.local`, `a.to_string` …). `Value`s cannot leave the closure. Root them with `a.global(v)` (a
`ScopeGlobal<'p, ValueBrand>`) if you need them afterwards.

## 4. Writing bindings

### 4.1 Parameters and return values

A quick summary (USAGE.md has the full table):

- Plain parameters are converted with `FromJs`: `f64`, `i32`, `u32`, `usize`, `bool`, `String`, `Option<T>`,
  `Vec<T>`, `Value`, `JsObject`, `JsString`. `&str` is ToString, borrowed.
- `&mut Scope<'_, '_>` anywhere in the list receives the current scope (it does not count as a JS argument).
- `j8::Bytes` takes an `ArrayBuffer` / typed array / `DataView`; returning `j8::Bytes` creates a `Uint8Array`.
- `Value<'scope>` / `j8::Function<'scope>` fields keep a JS value or callback in a class (they are traced);
  `ScopeGlobal<'p, ValueBrand>` keeps one outside the heap.
- Return values are converted with `IntoJs`. `Result<T, Error>` throws on `Err`.

### 4.2 Throwing

Take `&mut Scope` and return `Result<T, Error>`. Build the error with `a.throw(kind, message)`:

```rust,ignore
fn must(a: &mut Scope<'_, '_>, name: &str) -> Result<String, Error> {
    std::env::var(name).map_err(|_| a.throw(ErrorKind::Type, &format!("{name} is not set")))
}
```

When the error is only known later, or you want structured errors (Node's `err.code`), return a type of your own whose
`IntoJs` throws. dune's `host.rs` does this with `Io<T>`:

```rust,ignore
pub struct Io<T>(pub Result<T, Failure>);

impl<'scope, T: IntoJs<'scope>> IntoJs<'scope> for Io<T> {
    fn into_js(self, a: &mut Scope<'scope, '_>) -> Result<Value<'scope>, Error> {
        match self.0 {
            Ok(v) => v.into_js(a),
            Err(f) => Err(a.throw(f.kind, &f.message)),   // "ENOENT: no such file or directory, open 'a.txt'"
        }
    }
}

#[j8::object("fs")]
impl Fs {
    fn read(path: &str) -> Io<j8::Bytes> { /* … */ }
}
```

The prelude then reads the code off the front of the message and sets `err.code`. Because `IntoJs` runs on the
runtime thread, this also works for the results of `async fn`s.

### 4.3 What a bound type must be

- A `#[j8::class]` type is stored inside the JS object, in the GC heap: it is `Send + Sync`, derives `j8::Trace`
  and may carry `'scope`. Fields that are neither JS values nor `Trace` (an `Arc`) take `#[collect(require_static)]`.
  Use `Arc` / atomics for state shared with threads (see `Timer` in `dune/src/web/timer.rs`).
- A method that calls back into JS that reaches the same object again (`&mut self` while already borrowed) throws a
  TypeError instead of panicking.
- JS values in a class are traced edges, not roots: a cycle through a host object is collected like any other.
  Outside the heap (a thread, a channel) keep a `ScopeGlobal` root instead.

### 4.4 Stack traces

Every error object gets a V8-style `stack`: one made with `new Error()`, one the engine throws, and one a binding
throws with `a.throw` (its top frame is the JS call of the binding).

```text
Error: boom
    at inner (main.js:2:10)
    at new Box (main.js:6:14)
    at main.js:9:1
```

- `stack` is an accessor on `Error.prototype`, as in the TC39 proposal (V8 puts one on each error instead): the text is
  made when it is read, from the error's `name` and `message` at that time; assigning a string puts an own `stack` in
  front of it.
- `Error.stackTraceLimit` (10, read from `Error` itself when an error is made) caps the frames; `Error.captureStackTrace(obj)`
  puts a `stack` on any object.
- An error keeps only its frames (code and bytecode offset, and whether it constructs); names, scripts, lines and
  columns are looked up when `stack` is read, so making errors that nobody reads costs no formatting.
- `a.stack_trace(limit)` gives the JS frames running now, innermost first, as `CallSite { function, script, line,
  column, constructor }`: lines and columns count from 1, columns in UTF-16 units, and `Display` prints the frame the
  way `stack` does. `a.stack_text(v)` is the `stack` of a thrown value, or its ToString when it has none;
  `Runtime::exception_text` uses it, so an uncaught error prints with its frames.
- Not shown yet: frames of built-in functions (`at Array.map`), receiver type names (`at Object.method`), async frames
  (`at async f`), and the frame to hide (`captureStackTrace`'s second argument). With the JIT on, the frame of a
  function running as compiled code can be missing or point at the line it was entered on.

### 4.5 Debugging

`Executor::with_debugger(&d)` attaches a `Debugger` the executor borrows: `set_breakpoint(scope, script, line, column)`
/ `clear_breakpoint(id)`, `pause_on_exceptions(mode)`, and `d.paused(&mut Paused)` called on the JS thread at every
stop, which reads the frames (`frames`, as `CallSite`s), their scopes (`scopes`) and `this`, evaluates in a frame
(`evaluate`) and returns `Continue`, `StepInto`, `StepOver` or `StepOut`. `pause()` stops at the next statement; from
another thread, reach the executor with an `Interrupter` (5.7). A debugger turns the JIT off for that executor and
compiles with debug information (statement positions, local names); without one nothing changes. See the book's
Debugger chapter for how it works.

## 5. Async work and the event loop

j8 does not include an async runtime (no tokio). A `j8executor::EventLoop` is one JS event loop: an isolate, its
engine and one context, and a j8taskqueue queue; `j8::Runtime` is one event loop (`eval` and `run_until_idle` drive
it). The queue holds three kinds of tasks:

1. **scripts**, which run a check interval of steps at a time (1000 by default; V8's interrupt check, at frame
   entries and backward jumps) and then yield back to the queue;
2. **settlements**, which resolve a promise with what its host work produced and drain the microtasks;
3. **polls** of host futures that were woken (no JS runs in them; a ready future queues its settlement).

Scripts and settlements are chained: each waits for the one before, so JS runs one task at a time, to completion.
Polls wait for nothing. So every time the bytecode advances, the queue is checked: host work that became ready
meanwhile, from any thread, is polled before the script goes on, and its JS runs right after the script.

When nothing can run, the thread **parks** until a waker fires. `run_until_idle` returns once no script and no
future of the event loop is outstanding.

So **a future must wake its waker when it can make progress**, or the loop sleeps forever. Futures from crates that
need their own reactor (tokio's sockets and timers) do not work as they are. Run that work on a thread or on your own
reactor, and wake the waker from there.

**Event loops live on loop threads.** `j8::EventLoopSet::scope(&platform, threads, |set| …)` starts
`threads` loop threads for the duration of the closure; `j8::Runtime::on::<Host>(set, executor)` (or `set.spawn`)
puts an event loop there. Each event loop lives on one thread and is not shared, so a thread runs its event loops
with no locks: 8 threads and 1000 event loops make 125 on each. Host threads reach them by messages (submitting a
script, `enter`), and wait for the answer.

- **Load balancing.** A new event loop goes to the least loaded thread (event loops there plus those waiting for a
  turn). A thread with nothing to run asks the busiest one for half of the event loops waiting in its queue (work
  stealing), or for half the difference when none waits. `set.rebalance()` evens the threads out on demand;
  `set.split(thread)` starts a new thread and gives it half of `thread`'s event loops. An event loop moves only
  between turns, with its isolate, engine, paused script and pending futures; wakes and messages follow it.
- **One quota.** Every turn runs the set's slice of steps (`set.set_slice`) times the event loop's weight
  (`EventLoop::set_weight`), checking the queue every `set.set_check` steps. `set.loads()`, `set.spent()` and
  `EventLoop::spent()` show where the work went.

Many runtimes run the same preludes and import the same modules: give their executors one `CodeCache`, and each such
source is parsed and compiled once (preludes run as `ScriptKind::Shared`; one-off scripts are not kept).

```rust,ignore
let platform = Host::platform(DefaultPlatform::default());
let code = CodeCache::shared();
EventLoopSet::scope(&platform, NonZeroUsize::new(8).unwrap(), |set| {
    let mut runtimes: Vec<j8::Runtime<'_>> = (0..1000).map(|_| j8::Runtime::on::<Host>(set, Executor::default().with_cache(Arc::clone(&code))).unwrap()).collect();
    // drive them from as many host threads as you like: each call waits for its event loop's thread
});
```

### 5.1 `async fn` bindings

An `async fn` takes owned parameters only: no `self`, `&str`, `&mut Scope`, `This` or `Rest`. Its future runs later,
after the call has returned. The call returns a promise at once, and the result is converted with `IntoJs` when the
promise settles (so `Io<T>`-style errors reject it).

### 5.2 Blocking work: offload to a thread

dune runs every blocking call (files, sockets, `sleep`) on a thread of its own with `host::Offload`. Copy it from
[`dune/src/host.rs`](https://github.com/0x00-Community/j8/blob/main/dune/src/host.rs); the core is:

```rust,ignore
fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
    let this = self.get_mut();
    if let Some(work) = this.work.take() {                 // first poll: start the thread
        let (tx, rx) = mpsc::channel();
        let waker = cx.waker().clone();
        std::thread::spawn(move || {
            let _ = tx.send(work());
            waker.wake();                                  // ⇐ without this, run_until_idle never returns
        });
        this.done = Some(rx);
    }
    match this.done.as_ref().map(Receiver::try_recv) {
        Some(Ok(v)) => Poll::Ready(v),
        Some(Err(TryRecvError::Empty)) => Poll::Pending,
        _ => panic!("offloaded work panicked"),
    }
}
```

```rust,ignore
#[j8::object("fs")]
impl Fs {
    async fn read_file(path: String) -> Io<j8::Bytes> {
        Offload::new(move || Io::new(std::fs::read(&path).map(j8::Bytes), "open", &path)).await
    }
}
```

### 5.3 A method that returns a promise

When the async work needs `&self`, the `Scope`, or should be cancellable, write a normal method and make the promise
yourself with `j8::Glue`:

- `Glue::spawn(a, future)` creates a pending promise, hands `future` to the event loop and returns the promise.
- `Glue::settle(value)` is what the future must produce: `value` is converted with `IntoJs` when the promise settles.

```rust,ignore
#[j8::class]
impl Timer {
    fn wait<'scope>(&self, a: &mut Scope<'scope, '_>, ms: f64) -> Result<Value<'scope>, Error> {
        let sleep = Sleep { cancelled: self.cancelled.clone(), /* … */ };
        Glue::spawn(a, async move { Glue::settle(sleep.await) })
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}
```

The future must be `Send + 'static`: clone what it needs out of `self` (here an `Arc<AtomicBool>`).

A future that becomes ready because of JS (like `cancel` above) needs no wake-up: the loop polls everything again after
every settlement, before it parks.

Async work belongs to the isolate that started it: each `Runtime` has its own queue, and only its own
`run_until_idle` settles it, so several runtimes can share one platform. An isolate made without an event loop (by
hand, without `CreateParams { event_loop: true, .. }`) refuses the work, and the call throws a TypeError.

### 5.4 Awaiting a JS promise from Rust

`j8::Promise<T>` goes the other way: a parameter of this type takes a JS promise, and the Rust side `.await`s it. It
is an owned parameter, so `async fn` bindings can take it:

```rust,ignore
#[j8::object("math")]
impl Math {
    async fn double(p: j8::Promise<f64>) -> Result<f64, Error> {
        Ok(p.await? * 2.0)          // a rejection passes on to this function's own promise
    }
}
```

- It takes its argument the way `await` does: a promise is watched directly (a `then` patched onto it is not called),
  a thenable is adopted, any other value counts as already fulfilled.
- The fulfilled value is converted with `FromJs` into `T` on the runtime thread when the promise settles. A
  rejection, or a value that does not convert, comes out as `Err(Error::Parked(ticket))` (the reason parked in
  the isolate), so `?` rejects the binding's own promise with the same reason.
- Settling wakes the future, so it works with `run_until_idle`, and also when the future is polled somewhere else.
- A promise that never settles keeps the future pending, like `await` in JS. The event loop then keeps waiting.

For a JS callback that should run more than once (an event handler), keep a `j8::Function` instead and call it inside
`rt.enter` or another binding.

### 5.5 Keeping the process alive

`run_until_idle` returns once nothing is pending. Repeating timers, servers and the like stay alive by always keeping
a future pending, and stop by settling it. A cleared timer should settle its promise (dune's `Timer::cancel`), or the
loop waits for it.

### 5.6 Unhandled rejections

When the microtasks have drained, every promise that was rejected and still has no handler goes to
`Platform::unhandled_rejection` (Node's `unhandledRejection`). The default does nothing; `HostPlatform` hands it to
the platform it wraps:

```rust,ignore
impl Platform for MyPlatform {
    fn unhandled_rejection<'scope>(&self, a: &mut Scope<'scope, '_>, _promise: JsObject<'scope>, reason: Value<'scope>) {
        if let Ok(text) = a.stack_text(reason) {
            eprintln!("Uncaught (in promise) {}", &**text);
        }
    }
    // now, print, … as before
}
```

A handler attached before the microtasks drain (`await`, `then`, `catch`) takes the promise off the list. One attached
on a later turn does not take back a report.

### 5.7 Running code on the JS thread from another thread

`EventLoop::interrupter()` (or `Runtime::interrupter()`) gives an `Interrupter`, `Clone + Send`. `interrupt(f)` runs
`f(&Executor, &mut Scope)` on the event loop's thread: in the middle of running JS at its next check (every 1024
instructions, back edges in JIT code; a busy loop in a callback included), or at the start of the next turn when the
loop is idle. JIT code leaves to the interpreter at the check, so `f` may run JS that changes what the machine code
assumed. A long-running native builtin (a big `sort`, regular expression backtracking, a big `JSON.parse`) delays `f`
until it returns. A debugger that sets a breakpoint or pauses a busy loop needs exactly this. Without a request the
checks cost what they did before. See the book's Framework chapter.

### 5.8 A heap limit

`EventLoop::set_heap_limit(Some(bytes), callback)` (or `Runtime::set_heap_limit`) limits the bytes the GC heap holds
(paragc's `Heap::used`: its blocks plus its large objects; `heap_used()` reports it). `callback` is
`Box<dyn FnMut(current, initial) -> new_limit>`, the shape of V8's near-heap-limit callback, but it runs once the heap has
gone past the limit, not before:

- The block or large object taken past the limit interrupts the running JS (5.7). On the JS thread the callback returns
  the new limit: JS goes on under it, and is terminated with `Error::Terminated` over it (also without a callback). V8
  instead crashes with out of memory when the callback does not raise the limit; j8 stops the JS and keeps the process.
  This reaches an allocating loop in a module body or a promise callback, which never reach a safe point.
- An interrupt that arrives while no JS runs (the host allocated outside JS) does nothing; the next safe point takes over.
- At each safe point (after a turn or a slice) the isolate runs a full collection once the heap has grown by half the
  limit since the last one, then arms the limit again, so garbage left by earlier tasks does not count and a page whose
  live data stays large is not collected every turn. A safe point never terminates anything.
- Only the GC heap counts: memory objects own outside it (`ArrayBuffer` bytes, host objects' `Vec`s) does not. Within
  one task garbage counts, because j8 collects only at safe points.
- The limit needs an event loop: it is enforced by interrupts. `Isolate::set_heap_limit` is the piece the event loop
  builds on; an isolate run without one only collects at safe points.

### 5.9 What the event loop does not do yet

- **No reactor.** Futures that need tokio's (or another runtime's) reactor never wake. Run such work on a thread
  or a reactor of your own.
- **Settlements follow the polling order.** Futures that become ready in the same turn settle in the order they
  were polled, not the order they finished. JS that cares about order (timers) has to sort it out itself.
- **No timers in the loop.** A timer is host work like any other (a thread that sleeps, then wakes its future).
- **No cancellation from JS.** A future cannot be dropped from the JS side. Watch a flag of your own (dune's
  `Timer::cancel`).
- **`async fn` cannot take `&self`.** Use `Glue::spawn` (5.3).
- **A panic in a future propagates out of the loop** (out of `eval` / `run_until_idle`). An error while settling
  stops the loop with the remaining work still queued.
- **Microtasks are not sliced.** A settlement drains its microtasks to the end in one turn.

## 6. Modules

### 6.1 ES modules from files

`rt.eval_module(text, file)` runs `text` as an ES module. `DefaultPlatform` resolves `import` relative to the
importing file (canonicalized paths, no `node_modules`). For other rules (packages, URLs, a virtual file system),
write your own `Platform::load_module` (6.3).

`eval_module` returns the promise for the module's evaluation. A module graph without top-level `await` has finished
when `eval_module` returns, and the promise is already fulfilled. With top-level `await`, the promise settles once
every awaiting module in the graph has finished: drive it with `run_until_idle`, then read its state with
`Scope::promise_state`. A module that throws before it first suspends makes `eval_module` return
`Err(Error::Exception)`, as before; one that throws later rejects the promise.

### 6.2 Native modules in Rust: `#[j8::module]`

```rust,ignore
#[j8::module("myhost:kv", "kv")]
impl Kv {
    fn get(key: &str) -> Option<String> { /* … */ }
}

j8::runtime! { pub MyHost { modules: [Kv], } }
```

`import kv, { get } from "myhost:kv"` works through `HostPlatform`. A prelude can reach the same object as
`__j8_native["myhost:kv"]`, for example to implement `require`.

### 6.3 Built-in modules written in JavaScript: a custom `Platform`

dune's `node:` modules are JavaScript objects built by the prelude, not Rust types. It serves them to `import` with a
platform of its own, `NodePlatform` in `dune/src/node/mod.rs`. Its `load_module` returns a small generated ES module
that re-exports an object from `__j8_native`, and passes every other specifier on to the inner platform:

```rust,ignore
impl<P: Platform> Platform for NodePlatform<P> {
    fn load_module(&self, referrer: &str, specifier: &str) -> Option<(String, String)> {
        match Builtin::try_from(specifier) {
            // "const m = globalThis.__j8_native.builtin(\"fs\"); export default m; export const readFileSync = m.readFileSync; …"
            Ok(b) => Some((format!("node:{}", b.spec().0), b.source())),
            Err(()) => self.inner.load_module(referrer, specifier),
        }
    }
    fn now(&self) -> Duration { self.inner.now() }
    fn wall_clock(&self) -> Duration { self.inner.wall_clock() }
    fn print(&self, line: &str) { self.inner.print(line); }
}

impl<P: Pool> Pool for NodePlatform<P> {               // Platform requires Pool: forward it too
    fn threads(&self) -> usize { self.inner.threads() }
    fn run(&self, jobs: Vec<Task<'_>>) { self.inner.run(jobs); }
}
```

The first element of the returned pair is the module's normalized name. Modules are cached by it: a second `import`
that resolves to the same name gets the same module. A specifier that resolves to `None` fails the import with
`Error::Syntax("… Cannot find module '…'")`.

Platforms stack in any order, since none of them holds the async work (that lives in each runtime's isolate):

```rust,ignore
let platform = Dune::platform(NodePlatform::from(DefaultPlatform::default()));
```

Other `Platform` methods you may override: `print` (where `console.log` goes), `now` / `wall_clock` (a fake clock for
tests), `locale` / `time_zone` (the default locale and time zone of `Intl` and `Date`, with the `intl` feature).

## 7. Building on another runtime

```rust,ignore
j8::runtime! { pub Web  { natives: [Stdio, Clock, Codec, Random, Timer, Http], prelude: WEB_PRELUDE, } }
j8::runtime! { pub Dune: Web { globals: [Process], natives: [Fs, Os, Child], prelude: NODE_PRELUDE, } }
```

`Dune` installs everything `Web` does first, then its own types, and runs `Web`'s prelude before its own. Module
lookups try the child's modules first, then the parent's. The parent does not change, so a third party can extend
`Dune` without forking it.

## 8. The JIT

With feature `jit` (x86-64 Unix and Windows):

```rust,ignore
let executor = Executor::default().with_tier(j8::jit::Jit::default());
let rt = MyHost::runtime(set, executor)?;
```

Hot functions and loops are compiled to machine code. The test262 baseline also passes with the JIT on
(`J8_TEST262_JIT=1`).

## 9. Sharing objects across processes (Linux, feature `crossheap`)

An isolate can take a paragc cross-heap (a heap in shared memory, reserved at the same fixed address in every process)
as its old generation. Objects allocated with `alloc_shared` (types that are both `Trace` and paragc's `Shared`) move
into it when they survive a young collection, and other processes find them by a 32-byte signature:

```rust,ignore
let space = paragc::CrossHeapSpace::reserve_fixed()?;
isolate.attach_cross_heap(unsafe { space.create_unchecked("/dev/shm/my-heap", threads) }?)?;
let cx = isolate.create_context(&j8builtins::Builtins);
let ticket = isolate.enter(&cx, &executor, |scope| {
    let gc = scope.mc.gc_scope().alloc_shared(Payload { value: 42, next: None.into() });
    scope.shares.share(scope.mc, gc)
});
for (ticket, signature) in isolate.take_signatures() {
    // send `<[u8; 32]>::from(signature?)` to another process
}
```

j8 collects only between turns, so a shared object is promoted and signed when the `enter` that shared it returns.
A signature keeps nothing alive: the receiver opens the same heap (`CrossHeapSpace::open_fd` with a passed descriptor)
and calls `CrossHeapScope::resolve`, which gives `None` once the object is gone. JS objects themselves are not
shareable yet; that comes in later steps.

## 10. Where to look in the samples

| You want to … | Look at |
|---|---|
| set up a runtime, run a file, report errors, exit codes | `dune/src/main.rs`, `dune/src/session.rs` |
| a REPL (several `eval`s sharing one global object) | `dune/src/repl.rs` |
| throw Node-style errors with codes; run blocking work off-thread | `dune/src/host.rs` |
| bind plain objects (`stdio`, `clock`, `codec`, `random`) | `dune/src/web/mod.rs` |
| a cancellable promise (`Glue::spawn`) | `dune/src/web/timer.rs` |
| async file system | `dune/src/node/fs.rs` + `fs.js` |
| a custom `Platform` for JS-written modules; `Runtime::set` | `dune/src/node/mod.rs` |
| a module loader (`require`, `node_modules`, `package.json`) in JS | `dune/src/node/loader.js` |
| a class with Rust state, used only by the prelude | `workerd/src/main.rs` (`KvStore`) + `prelude.js` |
| call into a JS-defined entry point per request | `workerd/src/main.rs` (`Worker`) |
