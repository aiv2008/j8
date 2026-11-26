# Building a host

This part shows how to build your own JavaScript runtime on the `j8` crate, the way [dune](dune.md) and
[workerd](workerd.md) are built. The macros attribute by attribute: [Binding macros](bindings.md).

## Setup

```toml
[dependencies]
j8 = "0.4"                                          # default feature `runtime`: executor + built-ins + platform
# j8 = { version = "0.4", features = ["jit"] }      # + the JIT (x86-64 Unix and Windows, AArch64 Linux)
# j8 = { version = "0.4", features = ["json"] }     # + j8::Json<T> (serde)
```

| Path | What it is |
|---|---|
| `#[j8::object]` / `#[j8::class]` / `#[j8::function]` / `#[j8::module]` | bind Rust to JS |
| `j8::runtime!` | declare a runtime: which bound types go where, plus a JS prelude |
| `j8::Runtime` | a ready execution environment: `eval`, `eval_module`, `run_until_idle`, `set`, `text`, `enter` |
| `j8::Host` / `j8::HostPlatform` | the trait `runtime!` implements / the platform serving its native modules (stateless: runtimes may share one) |
| `j8::frame` | `j8framework`: `Scope`, `Error`, `ErrorKind`, `Value`, `Platform`, `Source` … |
| `j8::platform::DefaultPlatform` | the standard-library platform |
| `j8::executor::Executor` | the bytecode executor; `with_tier` adds the JIT |
| `j8::Promise<T>` | a JS promise awaited from Rust |
| `j8::Glue` | `Glue::spawn` / `Glue::settle`: hand-made promises |
| `j8::NATIVE` | `"__j8_native"`, the name of the hidden registry object |

## The smallest host

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

1. `X::platform(inner)` wraps a platform in a `HostPlatform` for runtime `X`. Keep it alive as long as the runtime:
   `Runtime<'p>` borrows it.
2. `j8::EventLoopSet::scope(&platform, threads, |set| …)` starts the loop threads the runtimes live on, for the length of
   the closure.
3. `X::runtime(set, executor)` creates the isolate and the context on the least loaded loop thread, installs everything
   and runs the preludes. A prelude that throws comes back as `Err` here. Every later call (`eval`, `enter`,
   `run_until_idle`) is a message to that thread and waits for its answer; any host thread may make it.
4. `rt.eval(text, file)` runs a script and returns a `ScopeGlobal<'p, ValueBrand>` (a root that outlives the
   call); `rt.text(&g)` turns it into a string with ToString.
5. `rt.run_until_idle()` waits until no async host work is left. Call it after every `eval` if your bindings have
   `async fn`s or return promises; otherwise those promises never settle.

## The split: Rust bindings + a JS prelude

Both samples split the work the same way:

- **Rust** does only what needs the machine (files, sockets, clocks, processes), small and flat.
- **JavaScript** (the prelude) builds the real API on top: classes, argument checking, Node / Web semantics. Much less
  code than doing it all through bindings.

`runtime!` has four sections, all optional, in this order, each followed by a comma:

| Section | Takes | Ends up at |
|---|---|---|
| `globals: [..]` | `#[j8::class]` / `#[j8::object]` types | the global object: `globalThis.Process` |
| `modules: [..]` | `#[j8::module("node:x", "x")]` types | `import … from "node:x"` and `__j8_native["node:x"]` |
| `natives: [..]` | `#[j8::class]` / `#[j8::object]` types | the hidden registry: `__j8_native.env`, `__j8_native.KvStore` |
| `prelude: EXPR` | JavaScript (a `&'static str`) | run once, as a script, after everything is installed |

The JS name is the macro argument (`#[j8::object("env")]` ⇒ `env`), else the Rust type name. Methods are renamed to
camelCase (`exists_sync` ⇒ `existsSync`) unless `#[js(name = "…")]` says otherwise.

`__j8_native` is a non-enumerable property of the global object. User code can still reach it: treat it as out of casual
reach, not as a security boundary. The usual prelude is an IIFE taking it:

```js
(function (native) {
  'use strict';
  const { clock, Timer } = native;        // natives bound in Rust
  globalThis.setTimeout = (f, ms) => { /* … */ };
  native.runMain = (file) => { /* … */ }; // entry points the host calls
})(globalThis.__j8_native);
```

A long prelude can be split into files joined with `concat!(include_str!("a.js"), include_str!("b.js"))`, as
`dune/src/node/mod.rs` does.

### Passing data in: `Runtime::set`

`rt.set("a.b.c", value)` defines `c` on `globalThis.a.b`, converting `value` with `IntoJs`. Numbers, strings, `Vec<T>`,
`BTreeMap<String, T>`, `Option<T>` all work:

```rust,ignore
rt.set("process.argv", args)?;                                        // Vec<String> ⇒ array
rt.set("process.env", std::env::vars().collect::<BTreeMap<_, _>>())?; // map ⇒ object
```

The parent object must exist already (the prelude makes it). That is how dune sets `process.argv` and friends without
the prelude knowing about Rust.

### Calling into the prelude

Put a function on `__j8_native` and `eval` a call to it, quoting the arguments as JS string literals (see dune's
`Session::quote`):

```rust,ignore
let call = format!("__j8_native.runMain({})", quote(file));
rt.eval(&call, file)?;
rt.run_until_idle()?;
```

For lower-level work, `rt.enter(|a| …)` gives you the context's `Scope` (`a.get_named`, `a.call`, `a.local`,
`a.to_string` …). `Value`s can't leave the closure; root them with `a.global(v)` (a
`ScopeGlobal<'p, ValueBrand>`) for later.

## What an Error means

| Variant | Meaning | What hosts do |
|---|---|---|
| `Error::Exception` | JS threw; the value waits in the isolate's pending slot, as in V8 | `rt.exception_text()` ⇒ `Uncaught …` with the stack |
| `Error::Syntax(msg)` | the source doesn't parse | print `msg` |
| `Error::Unsupported(msg)` | syntax j8 doesn't support yet | print `msg` |

## Stack traces

Every error object gets a V8-style `stack` (`new Error()`, errors the engine throws, and `a.throw` in a binding,
whose top frame is the JS call of the binding):

```text
Error: boom
    at inner (main.js:2:10)
    at new Box (main.js:6:14)
    at main.js:9:1
```

- `stack` is an accessor on `Error.prototype`, as in the TC39 proposal (V8 puts one on each error): made when read, from
  the error's `name` and `message` at that time; assigning a string puts an own `stack` in front of it. `Error.stackTraceLimit` (10, read from `Error` itself) caps the frames; `Error.captureStackTrace(obj)` puts one on
  any object.
- An error keeps only its frames (code, bytecode offset, whether it constructs); names, scripts, lines and columns are
  looked up when `stack` is read, so errors nobody reads cost no formatting.
- `a.stack_trace(limit)` gives the running JS frames, innermost first, as `CallSite { function, script, line, column,
  constructor }` (1-based, columns in UTF-16 units; `Display` prints the frame as `stack` does). `a.stack_text(v)` is a
  thrown value's `stack`, or its ToString without one; `Runtime::exception_text` uses it.
- Positions come from a table the compiler writes next to the bytecode (`BytecodeArray::positions`, bytecode offset
  ⇒ line and column), marked at statements, calls, `new` and property loads, as V8 does.
- Not shown yet: built-in frames, receiver type names (`Object.method`), async frames, `captureStackTrace`'s frame to
  hide. With the JIT on, a function running as compiled code can be missing or point at the line it was entered on.

## Building on another runtime

```rust,ignore
j8::runtime! { pub Web  { natives: [Stdio, Clock, Codec, Random, Timer, Http], prelude: WEB_PRELUDE, } }
j8::runtime! { pub Dune: Web { globals: [Process], natives: [Fs, Os, Child], prelude: NODE_PRELUDE, } }
```

`Dune` installs everything `Web` does first, then its own types, and runs `Web`'s prelude before its own. Module lookups
try the child's modules first, then the parent's. The parent doesn't change, so a third party can extend `Dune` without
forking it.

## The JIT

```rust,ignore
let executor = Executor::default().with_tier(j8::jit::Jit::default());
let rt = MyHost::runtime(set, executor)?;
```

## Where to look in the samples

| You want to … | Look at |
|---|---|
| set up a runtime, run a file, report errors, exit codes | `dune/src/cli/`, `dune/src/session.rs` |
| a REPL (several `eval`s sharing one global object) | `dune/src/repl.rs` |
| Node-style errors with codes; blocking work off-thread | `dune/src/host.rs` |
| bind plain objects (`stdio`, `clock`, `codec`, `random`) | `dune/src/web/mod.rs` |
| a cancellable promise (`Glue::spawn`) | `dune/src/web/timer.rs` |
| an async file system | `dune/src/node/fs.rs` + `fs.js` |
| a custom `Platform` for JS-written modules; `Runtime::set` | `dune/src/node/mod.rs` |
| a module loader in JS (`require`, `node_modules`, `package.json`) | `dune/src/node/loader.js` |
| a class with Rust state, used only by the prelude | `samples/workerd/src/main.rs` (`KvStore`) + `prelude.js` |
| call a JS-defined entry point per request | `samples/workerd/src/main.rs` (`Worker`) |
