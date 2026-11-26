# j8napi

**English** · [简体中文](README.zh-CN.md)

[Node-API](https://nodejs.org/api/n-api.html) for [j8](https://github.com/0x00-Community/j8): the C ABI that native
addons — including modules written with [napi-rs](https://napi.rs) — link against, implemented on j8's engine. The
`napi_*` functions are exported unmangled; an embedder links them in (statically, or exports them from its executable
for addons it loads at run time).

## Usage

```rust
// inside an `Isolate::enter`, with an agent of the context the addon belongs to
let exports = j8napi::Env::open(&mut scope, napi_register_module_v1)?;
scope.set_named(scope.context.global.into(), "addon", exports)?;
```

`Env::open` creates the addon's environment (it lives as long as the context's global object), calls the register
function and returns its exports.

## How it maps onto j8

- `napi_value` is a slot on the environment's handle stack holding the raw value word; handles live within one entry
  into the isolate, during which nothing is collected.
- Addon functions are `NativeClosure`s that can be `new`ed: the engine creates `this` from `new.target.prototype`, so
  JS classes can extend addon classes.
- `napi_wrap` and type tags live in the object's private members; finalizers of collected host objects are queued and
  run on the JS thread at the next callback.
- `napi_ref`, the pending exception and deferreds hold persistent handles.

## Differences from Node

- A `napi_ref` whose count drops to 0 stays a strong reference (the heap has no weak references yet).
- Strings are stored as UTF-8: lone surrogates in UTF-16 input are replaced.
- Buffers are plain `Uint8Array`s (there is no Node `Buffer` class).
- Async work and thread-safe functions need an isolate with an event loop (`EventLoopSet`); async work runs on a
  thread of its own. As in Node, a thread-safe function keeps the loop running (`run_until_idle` waits) until it is
  released or `napi_unref_threadsafe_function` is called. There is no libuv loop (`napi_get_uv_event_loop` fails).

## License

MIT, with [j8](https://github.com/0x00-Community/j8).
