<a id="top"></a>
<div align="center">

# j8

### V8, rewritten in Rust.
**A bytecode interpreter, a JIT and a parallel garbage-collected heap ([paragc](https://crates.io/crates/paragc)).**

[![Rust](https://img.shields.io/badge/Rust-100%25-orange?style=for-the-badge&logo=rust)](https://www.rust-lang.org)
[![test262](https://img.shields.io/badge/test262-42%2C500%20passing-success?style=for-the-badge)](#-test262)
[![License](https://img.shields.io/badge/License-MIT-brightgreen?style=for-the-badge)](#-license)

**🇺🇸 English** · [🇨🇳 简体中文](README.zh-CN.md)

</div>

---

## ⚡ What is j8

A JavaScript engine in Rust, layered like V8:

- **Bytecode:** all JS (including `eval` and `Function`) compiles to V8 Ignition bytecode, byte for byte. There is no AST interpreter.
- **JIT:** hot functions and loops become a speculative SSA graph, emitted as x86-64 or AArch64 machine code.
- **Heap:** [paragc](https://crates.io/crates/paragc). There is no `'static` and no global state.
- **Parsing:** oxc.

Core ECMAScript is covered except what [PLAN.md](PLAN.md) puts out of scope (Temporal, ShadowRealm, …).
`Intl` (Collator, NumberFormat, DateTimeFormat, PluralRules, ListFormat, Segmenter, Locale, RelativeTimeFormat, DisplayNames, DurationFormat) is the opt-in `intl` feature, built on ICU4X.

| Check | Result |
| --- | --- |
| test262 baseline | **42,500** passing, interpreter and JIT, no regression allowed |
| vs V8 jitless | **0.4×** the time (geometric mean over 11 benchmarks) |
| vs V8 (with JIT) | **1.1×** the time, see [benchmark.md](samples/benchv8/benchmark.md) |

## 📦 Install

```sh
cargo add j8                                    # executor + builtins + standard platform
cargo add j8 --features jit                     # plus the JIT (x86-64 Unix / Windows, AArch64 Linux)
cargo add j8 --features intl                    # plus Intl and the host time zone for Date (ICU4X data, about 7 MB)
cargo add j8 --features crossheap               # Linux: share heap objects across processes
cargo add j8 --no-default-features --features compiler   # JS ⇒ bytecode only
```

```rust
use j8builtins::Builtins;
use j8executor::Executor;
use j8framework::{CreateParams, Isolate, Source};
use j8platform::DefaultPlatform;

let platform = DefaultPlatform::default();
let mut isolate = Isolate::new(&platform, CreateParams::default());
let executor = Executor::default();
let cx = isolate.create_context(&Builtins);
let result = executor.eval(&mut isolate, &cx, &Source { text: "1 + 2 * 3", file: "a.js" })?;
isolate.enter(&cx, &executor, |scope| {
    let v = scope.local(&result);
});
```

Binding Rust types to JS: `#[j8::class]` / `#[j8::object]` / `#[j8::function]`, see [crates/macros/USAGE.md](crates/macros/USAGE.md).
Writing a host: [EMBEDDING.md](EMBEDDING.md). Internals: [the book](https://j8.app238.com).

## 🚀 Quick start

```sh
cd dune && cargo run --release -- examples/node.js         # dune: a Node-compatible runtime (--web, -p, REPL, --jit)
cd samples/workerd && cargo run -- examples/counter.js --bind COUNTER=Counter --request "GET /c/inc?name=a"
cd samples/j8bcdbg && cargo run -- compile -e 'let s = 0; for (let i = 0; i < 10; i++) s += i; s'
cd samples/benchv8 && cargo run --release                   # benchmarks against V8
```

## 🧱 Crates

| crate | Role |
|---|---|
| `j8` | Root package: re-exports the crates below by feature, plus the binding macros |
| `j8framework` | Core types and traits: `Isolate`, `Scope`, `Context`, `Value`, `Platform`, `Intrinsics`, JS operation semantics |
| `j8bytecode` | Ignition bytecode, encoder / decoder (tables generated from V8's headers) |
| `j8compiler` | JS ⇒ bytecode |
| `j8executor` | Bytecode executor, event loops |
| `j8jit` | The JIT tier |
| `j8builtins` | Built-in objects |
| `j8platform` | `DefaultPlatform`: thread pool, clock |
| `j8binding` / `j8macros` | Rust ⇄ JS bindings |
| `j8napi` | Node-API |
| `j8taskqueue` / `j8lockfree` | Lock-free task queue and data structures |

`dune/` and `samples/*` are separate packages that depend on the published `j8`.

## 📜 test262

```sh
git submodule update --init
cargo test --release --test test262                          # the baseline (tests/test262.passing), fails on any drop
cargo test --release --test test262 -- built-ins/Math         # some directories
J8_TEST262_JIT=1      cargo test --release --test test262     # with the JIT on every function
J8_TEST262_VERBOSE=1  cargo test --release --test test262 -- <path>   # why each test failed
J8_TEST262_UPDATE=1   cargo test --release --test test262 -- <path>   # write results back to the baseline
```

## ✍️ Conventions

- Private `mod`s, flat `pub use` exports; ⛔ `pub mod`
- ⛔ `static` / `'static` (except `&'static str` in enums)
- Strings become enums right after deserialization; conversions go through `From` / `TryFrom`
- No comments in source files; documentation lives in the READMEs and the book

## 📄 License

MIT.

<div align="center">

[⬆️ Back to top](#top) · [🇨🇳 简体中文](README.zh-CN.md)

</div>
