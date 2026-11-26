# j8builtins

[English](README.md) · **简体中文**

[j8](https://github.com/0x00-Community/j8) 的 JavaScript 标准内建对象。实现 `j8framework::Intrinsics`：
新建 `Context` 时由 `Builtins::install` 把全局对象、全局函数和全部内建构造函数、原型、方法装进去。

## 位置

`j8framework` 只定义 `Intrinsics` 接口，这里是缺省实现。宿主建上下文时传进去，也可以换掉或裁剪：

```rust
let cx = isolate.create_context(&j8builtins::Builtins);
```

只依赖 `j8framework`，不依赖执行器：每个内建函数都是宿主函数 `fn(&mut Scope, this, &[Value]) -> Result<Value, Error>`，
用 `Scope` 上的运算语义写成。

## 内容

| 文件 | 内建对象 |
| --- | --- |
| `global.rs` | 全局对象、`parseInt` / `isNaN` / `eval` … |
| `object.rs`、`function.rs`、`reflect.rs` | `Object`、`Function`、`Reflect` / `Proxy` |
| `array.rs`、`typed.rs`、`buffer.rs` | 数组、类型数组、`ArrayBuffer` / `DataView` |
| `string.rs`、`text.rs`、`regexp.rs`、`json.rs` | 字符串、`Symbol`、正则（[regress](https://crates.io/crates/regress)）、JSON |
| `math.rs`、`bigint.rs`、`date.rs` | `Math`、`BigInt`、`Date` |
| `collections.rs`、`weak.rs` | `Map` / `Set`、`WeakMap` / `WeakSet` / `WeakRef` / `FinalizationRegistry` |
| `promise.rs`、`asyncgen.rs`、`iter.rs` | Promise、异步生成器、迭代器与迭代器辅助方法 |
| `error.rs` | `Error` 系列 |
| `intl/` | `Intl`（`Collator`、`NumberFormat`、`DateTimeFormat`、`PluralRules`）与 `toLocale*` 方法，开 `intl` feature 时编入（ICU4X） |

热点内建函数有快路径：可观察的行为不可能不同时（`RegExp.prototype` 没被动过、往稠密数组末尾追加、字符串与数字的拼接）跳过完整的规范步骤。
正确性以 test262 为准。

## 许可

MIT，随 [j8](https://github.com/0x00-Community/j8) 一起。
