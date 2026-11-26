# 内建对象

`crates/j8builtins` 是 JavaScript 标准内建对象，实现 `j8framework::Intrinsics`：新建 `Context` 时由 `Builtins::install`
把全局对象、全局函数和全部内建构造器、原型、方法装进去。

```rust,ignore
let cx = isolate.create_context(&j8builtins::Builtins);
```

`j8framework` 只定义 `Intrinsics` 接口，这里是缺省实现；宿主可以换掉或裁剪。j8builtins **只依赖 `j8framework`**，
不依赖执行器：每个内建函数都是宿主函数 `fn(&mut Scope, this, &[Value]) -> Result<Value, Error>`，用
`Scope` 上的运算语义写成；需要回调 JS 时经 `Invoke`。

## 文件

| 文件 | 内建对象 |
|---|---|
| `global.rs` | 全局对象、`parseInt` / `parseFloat` / `isNaN` / `eval` / URI 函数 |
| `object.rs`、`function.rs`、`reflect.rs` | `Object`、`Function`、`Reflect` / `Proxy` |
| `array.rs`、`typed.rs`、`buffer.rs` | `Array`、TypedArray（12 种，含 Float16）、`ArrayBuffer` / `SharedArrayBuffer` / `DataView` / `Atomics` |
| `string.rs`、`text.rs` | `String`、`Symbol` |
| `regexp.rs`、`dfa.rs` | `RegExp`（[regress](https://crates.io/crates/regress) + 一个 DFA 快路） |
| `json.rs` | `JSON` |
| `math.rs`、`bigint.rs`、`date.rs` | `Math`、`BigInt`、`Date` |
| `collections.rs`、`weak.rs` | `Map` / `Set`、`WeakMap` / `WeakSet` / `WeakRef` / `FinalizationRegistry` |
| `promise.rs`、`asyncgen.rs`、`iter.rs` | `Promise`、异步生成器、迭代器与迭代器助手 |
| `error.rs` | `Error`、`TypeError` … `AggregateError`，`cause` |
| `helpers.rs` | 共用的规范步骤 |
| `intl/` | `Intl` 与依赖语言区域的方法，开 `intl` feature 时编入（见下文） |

## 覆盖范围

- **Object / Array** 的全部方法：Array 方法对类数组通用、species；`from` / `flat` / `toSorted` / `with` ……；
  `join` / `toLocaleString` 遇到正在 join 的数组给 `""`（和 V8、SpiderMonkey 一样），正在 join 的数组记在 `Scope` 上
  （`Scope::joining`，一个 `Visiting` 栈，`JSON.stringify` 找环用的也是它）；
- **反射**：`Reflect`、`Proxy`（含 revocable）；
- **迭代器助手**：全局 `Iterator`、`Iterator.from`、`map` / `filter` / `take` / `drop` / `flatMap` / `reduce` ……；
  Set 方法（`union` 等）、`Map.groupBy` / `Object.groupBy`；
- **二进制数据**：可调大小的 `ArrayBuffer`（`transfer`、detach）、可增长的 `SharedArrayBuffer`、`DataView`、
  `Atomics`（单线程语义）；
- **Date**（开 `intl` feature 时本地时区取 `Platform::time_zone`，否则按 UTC 处理）；**数的格式化**（`toFixed` /
  `toPrecision` / `toExponential` 精确到十进制）、`Math.sumPrecise` / `f16round`；
- **RegExp**：Symbol 协议、`lastIndex`、`d` 标志、具名分组、species、`RegExp.escape`；字符串的
  `match` / `matchAll` / `replace` / `replaceAll` / `search` / `split`；
- **Promise** 建在 NewPromiseCapability 上：子类化、`all` / `race` / `allSettled` / `any` / `withResolvers` / `try`；
- 内建构造器按 `new.target` 定原型，所以 `class X extends Map` 之类能工作；宿主函数区分能否 `new`。

范围外：Temporal、ShadowRealm 等（见[路线图](roadmap.md)）。

## Intl

`intl` feature（`j8builtins/intl`，根包 `j8/intl` 转出）在 [ICU4X](https://github.com/unicode-org/icu4x) 的编译期数据上实现
ECMA-402，运行时不需要数据文件：

- `Intl.getCanonicalLocales`、`Intl.supportedValuesOf`、`supportedLocalesOf`；协商语言区域时，ICU4X 的 likely-subtags
  数据认得其语言的就算可用；
- `Intl.Collator` 与 `String.prototype.localeCompare`；`toLocaleUpperCase` / `toLocaleLowerCase`（土耳其语、阿塞拜疆语、
  立陶宛语规则）；
- `Intl.NumberFormat`：位数选项与全部舍入模式、科学 / 工程 / 紧凑记数法、符号显示、分组、数字系统、`formatToParts`；
  `Number` / `BigInt` 的 `toLocaleString`。货币和单位样式输出 ISO 代码或单位标识符，百分比用普通的 `%`（这几种的 CLDR
  格式不在 ICU4X 的稳定数据里）；
- `Intl.DateTimeFormat`：各字段选项、`dateStyle` / `timeStyle`、小时制、历法、IANA 时区与 UTC 偏移（时区规则用
  [jiff](https://crates.io/crates/jiff) 内置的 tzdb）、`formatToParts`；`Date` 的 `toLocaleString` / `toLocaleDateString`
  / `toLocaleTimeString`；
- `Intl.PluralRules`：基数与序数规则，`select` 先按 NumberFormat 的方式舍入，`selectRange`；
- 默认语言区域与时区取 `Platform::locale` / `Platform::time_zone`，取不到时用 `en-US` 与 `UTC`；有时区时，`Date` 的
  本地时间方法和 `toString` 也用它。

选项对象按规范给的顺序读，test262 用例装的 getter 触发顺序一致。每个 `Intl` 实例是宿主对象，持有构造时建好的 ICU4X
格式化器。数据在去掉符号的二进制里约占 7 MB。

## 正则：两个引擎

- 主引擎是 regress：按 UTF-16 码元匹配的回溯引擎；
- `dfa.rs` 是第二个引擎：对于 `regex` crate 在全 ASCII 输入上能与 ECMAScript 完全一致匹配的模式，用有限自动机加字面量
  预过滤代替回溯。`Dfa::new` 翻译模式，拒绝一切语义可能不同的东西：反向引用、环视、具名分组、`v` 模式、`m` 下的锚点、
  量词里的捕获组（ECMAScript 每轮重置它们）、Annex B 的怪写法、非 ASCII 的模式文本。其余情况和非 ASCII 输入仍走 regress。
- 正则字面量共享 realm 里编译好的入口与字符串；没有 `g` / `y` 的 `test()` 跳过 `lastIndex`。

## 快路

热点内建函数有快路：**可观察的行为不可能不同**时跳过完整的规范步骤——比如 `RegExp.prototype` 没被动过（pristine 检查）、
往稠密数组末尾追加、字符串与数字的拼接、JSON 解析时重复的键序列直接解析成同一个 shape。正确性以 test262 为准。

大数据的纯计算（排序的键、数的转换）可以经 `j8framework::Par` 切块放到平台线程池上并行，再把结果写回。
