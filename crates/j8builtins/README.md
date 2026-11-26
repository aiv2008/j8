# j8builtins

**English** · [简体中文](README.zh-CN.md)

The JavaScript standard built-in objects of [j8](https://github.com/0x00-Community/j8). It implements
`j8framework::Intrinsics`: when a new `Context` is created, `Builtins::install` puts the global object, the global
functions and every built-in constructor, prototype and method into it.

## Where it sits

`j8framework` only defines the `Intrinsics` interface; this crate is the default implementation. An embedder passes it
when creating a context, and can replace or trim it:

```rust
let cx = isolate.create_context(&j8builtins::Builtins);
```

It depends only on `j8framework`, not on the executor: every built-in is a host function
`fn(&mut Scope, this, &[Value]) -> Result<Value, Error>` written against the operation semantics on `Scope`.

## Contents

| File | Built-ins |
| --- | --- |
| `global.rs` | The global object, `parseInt` / `isNaN` / `eval` … |
| `object.rs`, `function.rs`, `reflect.rs` | `Object`, `Function`, `Reflect` / `Proxy` |
| `array.rs`, `typed.rs`, `buffer.rs` | Arrays, typed arrays, `ArrayBuffer` / `DataView` |
| `string.rs`, `text.rs`, `regexp.rs`, `json.rs` | Strings, `Symbol`, regular expressions ([regress](https://crates.io/crates/regress)), JSON |
| `math.rs`, `bigint.rs`, `date.rs` | `Math`, `BigInt`, `Date` |
| `collections.rs`, `weak.rs` | `Map` / `Set`, `WeakMap` / `WeakSet` / `WeakRef` / `FinalizationRegistry` |
| `promise.rs`, `asyncgen.rs`, `iter.rs` | Promises, async generators, iterators and iterator helpers |
| `error.rs` | The `Error` family |
| `intl/` | `Intl` (`Collator`, `NumberFormat`, `DateTimeFormat`, `PluralRules`) and the `toLocale*` methods, behind the `intl` feature (ICU4X) |

Hot built-ins have fast paths that skip the full spec algorithm when nothing observable can differ (an untouched
`RegExp.prototype`, appending to a dense array, joining strings and numbers). Correctness is checked against test262.

## License

MIT, as part of [j8](https://github.com/0x00-Community/j8).
