<a id="top"></a>
<div align="center">

# j8

### 用 Rust 重写 V8.
**字节码解释器 + JIT + 并行回收的堆 ([paragc](https://crates.io/crates/paragc)).**

[![Rust](https://img.shields.io/badge/Rust-100%25-orange?style=for-the-badge&logo=rust)](https://www.rust-lang.org)
[![test262](https://img.shields.io/badge/test262-42%2C500%20%E9%80%9A%E8%BF%87-success?style=for-the-badge)](#-test262)
[![License](https://img.shields.io/badge/License-MIT-brightgreen?style=for-the-badge)](#-许可证)

[🇺🇸 English](README.md) · **🇨🇳 简体中文**

</div>

---

## ⚡ j8 是什么

用 Rust 写的 JavaScript 引擎, 分层同 V8:

- **字节码:** 所有 JS (含 `eval`、`Function`) 先编成 V8 Ignition 字节码, 逐字节一致; 没有 AST 解释层.
- **JIT:** 热函数和热循环建成推测式 SSA 图, 出 x86-64 / AArch64 机器码.
- **堆:** [paragc](https://crates.io/crates/paragc); 没有 `'static`, 没有全局状态.
- **解析:** oxc.

核心 ECMAScript 已覆盖, [PLAN.md](PLAN.md) 列为不做的除外 (Temporal、ShadowRealm 等).
`Intl` (Collator、NumberFormat、DateTimeFormat、PluralRules、ListFormat、Segmenter、Locale、RelativeTimeFormat、DisplayNames、DurationFormat) 是可选的 `intl` feature, 基于 ICU4X.

| 项目 | 结果 |
| --- | --- |
| test262 基线 | **42,500** 条通过, 解释器与 JIT 都跑, 不许回归 |
| 对 V8 jitless | 耗时 **0.4×** (11 个基准几何平均) |
| 对 V8 (带 JIT) | 耗时 **1.1×**, 见 [benchmark.md](samples/benchv8/benchmark.md) |

## 📦 安装

```sh
cargo add j8                                    # 执行器 + 内建对象 + 标准平台
cargo add j8 --features jit                     # 加 JIT (x86-64 Unix / Windows、AArch64 Linux)
cargo add j8 --features intl                    # 加 Intl 与 Date 的宿主时区 (ICU4X 数据, 约 7 MB)
cargo add j8 --features crossheap               # Linux: 跨进程共享堆对象
cargo add j8 --no-default-features --features compiler   # 只要 JS ⇒ 字节码
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

把 Rust 类型绑成 JS: `#[j8::class]` / `#[j8::object]` / `#[j8::function]`, 见 [crates/macros/USAGE.md](crates/macros/USAGE.md).
写宿主: [EMBEDDING.md](EMBEDDING.md). 内部原理: [j8 之书](https://j8.app238.com).

## 🚀 快速开始

```sh
cd dune && cargo run --release -- examples/node.js         # dune: 兼容 Node 的运行时 (--web、-p、REPL、--jit)
cd samples/workerd && cargo run -- examples/counter.js --bind COUNTER=Counter --request "GET /c/inc?name=a"
cd samples/j8bcdbg && cargo run -- compile -e 'let s = 0; for (let i = 0; i < 10; i++) s += i; s'
cd samples/benchv8 && cargo run --release                   # 和 V8 对比的基准
```

## 🧱 结构

| crate | 职责 |
|---|---|
| `j8` | 根包: 按 feature 重新导出下面各 crate, 以及绑定宏 |
| `j8framework` | 核心类型与 trait: `Isolate`、`Scope`、`Context`、`Value`、`Platform`、`Intrinsics`, JS 运算语义 |
| `j8bytecode` | Ignition 字节码, 编码 / 解码 (表格从 V8 头文件生成) |
| `j8compiler` | JS ⇒ 字节码 |
| `j8executor` | 字节码执行器、事件循环 |
| `j8jit` | JIT 层 |
| `j8builtins` | 内建对象 |
| `j8platform` | `DefaultPlatform`: 线程池、时钟 |
| `j8binding` / `j8macros` | Rust ⇄ JS 绑定 |
| `j8napi` | Node-API |
| `j8taskqueue` / `j8lockfree` | 无锁任务队列与数据结构 |

`dune/` 和 `samples/*` 是独立的包, 依赖已发布的 `j8`.

## 📜 test262

```sh
git submodule update --init
cargo test --release --test test262                          # 跑基线 (tests/test262.passing), 掉一条就失败
cargo test --release --test test262 -- built-ins/Math         # 只跑某些目录
J8_TEST262_JIT=1      cargo test --release --test test262     # 每个函数都走 JIT
J8_TEST262_VERBOSE=1  cargo test --release --test test262 -- <path>   # 列出失败原因
J8_TEST262_UPDATE=1   cargo test --release --test test262 -- <path>   # 结果写回基线
```

## ✍️ 约定

- 子模块私有 `mod`, 用 `pub use` 平铺导出; ⛔ `pub mod`
- ⛔ `static` / `'static` (枚举里的 `&'static str` 除外)
- 字符串反序列化后立即转枚举; 转换走 `From` / `TryFrom`
- 源码不写注释; 文档在各 README 和 j8 之书里

## 📄 许可证

MIT.

<div align="center">

[⬆️ 回到顶部](#top) · [🇺🇸 English](README.md)

</div>
