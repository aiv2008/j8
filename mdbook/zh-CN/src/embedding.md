# 搭一个宿主

这一部分讲怎么在 `j8` crate 上搭自己的 JavaScript 运行时，像 [dune](dune.md) 和 [workerd](workerd.md) 那样。
宏的逐项说明见[绑定宏](bindings.md)。

## 依赖

```toml
[dependencies]
j8 = "0.4"                                          # 缺省 feature `runtime`：执行器 + 内建对象 + 平台
# j8 = { version = "0.4", features = ["jit"] }      # + x86-64 JIT（x86-64 Unix 和 Windows）
# j8 = { version = "0.4", features = ["json"] }     # + j8::Json<T>（serde）
```

| 路径 | 是什么 |
|---|---|
| `#[j8::object]` / `#[j8::class]` / `#[j8::function]` / `#[j8::module]` | 把 Rust 绑成 JS |
| `j8::runtime!` | 声明一个运行时：哪些绑定类型放哪里，加一段 JS 序言 |
| `j8::Runtime` | 准备好的执行环境：`eval`、`eval_module`、`run_until_idle`、`set`、`text`、`enter` |
| `j8::Host` / `j8::HostPlatform` | `runtime!` 实现的 trait / 给它供原生模块的平台（无状态，运行时可共享） |
| `j8::frame` | `j8framework`：`Scope`、`Error`、`ErrorKind`、`Value`、`Platform`、`Source` …… |
| `j8::platform::DefaultPlatform` | 标准库平台 |
| `j8::executor::Executor` | 字节码执行器；`with_tier` 加 JIT |
| `j8::Promise<T>` | 在 Rust 里 await 的 JS promise |
| `j8::Glue` | `Glue::spawn` / `Glue::settle`：手工做的 promise |
| `j8::NATIVE` | `"__j8_native"`，隐藏注册表对象的名字 |

## 最小的宿主

```rust,ignore
use std::num::NonZeroUsize;
use std::process::ExitCode;

use j8::executor::Executor;
use j8::frame::{Scope, Error, ErrorKind};
use j8::platform::DefaultPlatform;

/// 碰机器的部分，放在 `__j8_native.env`
struct Env;

#[j8::object("env")]
impl Env {
    fn var(name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    /// 变量不存在时抛 TypeError
    fn must(a: &mut Scope<'_, '_>, name: &str) -> Result<String, Error> {
        std::env::var(name).map_err(|_| a.throw(ErrorKind::Type, &format!("{name} is not set")))
    }
}

/// 公开接口是 JavaScript，建在原生绑定之上
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
    // 一个运行时一个 loop 线程就够；运行时多了再加
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

步骤总是一样的：

1. `X::platform(inner)` 把一个平台包成运行时 `X` 的 `HostPlatform`。它要活得和运行时一样久：`Runtime<'p>` 借用它。
2. `j8::EventLoopSet::scope(&platform, threads, |set| …)` 在闭包期间起好运行时要住的 loop 线程。
3. `X::runtime(set, executor)` 在负载最轻的 loop 线程上建 isolate 和上下文、装好一切、跑序言。序言抛异常在这里以
   `Err` 返回。之后的每次调用（`eval`、`enter`、`run_until_idle`）都是发给那个线程的消息，并等它回答；任何宿主线程都能发。
4. `rt.eval(text, file)` 跑一段脚本，返回 `ScopeGlobal<'p, ValueBrand>`（活过这次调用的根），`rt.text(&g)` 按 ToString
   转成字符串。
5. `rt.run_until_idle()` 等到没有剩下的异步宿主任务。绑定里有 `async fn` 或返回 promise 时，每次 `eval` 之后都要调它，
   否则那些 promise 永远不会兑现。

## 分工：Rust 绑定 + JS 序言

两个样例都这样分工：

- **Rust** 只做必须碰机器的事（文件、套接字、时钟、进程），保持小而平。
- **JavaScript**（序言）在上面搭真正的接口：类、参数检查、Node / Web 语义。代码量比全用绑定写少得多。

`runtime!` 有四段，都是可选的，必须按这个顺序、每段后面带逗号：

| 段 | 收什么 | 落在哪里 |
|---|---|---|
| `globals: [..]` | `#[j8::class]` / `#[j8::object]` 类型 | 全局对象：`globalThis.Process` |
| `modules: [..]` | `#[j8::module("node:x", "x")]` 类型 | `import … from "node:x"` 与 `__j8_native["node:x"]` |
| `natives: [..]` | `#[j8::class]` / `#[j8::object]` 类型 | 隐藏注册表：`__j8_native.env`、`__j8_native.KvStore` |
| `prelude: EXPR` | 一段 JavaScript（`&'static str`） | 全部装好之后作为脚本跑一次 |

JS 名字是宏参数（`#[j8::object("env")]` ⇒ `env`），没给就是 Rust 类型名。方法名转成 camelCase（`exists_sync` ⇒ `existsSync`），
除非 `#[js(name = "…")]` 另有指定。

`__j8_native` 是全局对象上不可枚举的属性。用户代码仍然拿得到它，所以只当作「随手碰不到」，不是安全边界。惯用的序言形状是
一个接收它的 IIFE：

```js
(function (native) {
  'use strict';
  const { clock, Timer } = native;        // Rust 里绑定的原生对象
  globalThis.setTimeout = (f, ms) => { /* … */ };
  native.runMain = (file) => { /* … */ }; // 宿主调用的入口
})(globalThis.__j8_native);
```

长序言可以拆成几个文件，用 `concat!(include_str!("a.js"), include_str!("b.js"))` 拼起来（`dune/src/node/mod.rs` 就是这样）。

### 往运行时里传数据：`Runtime::set`

`rt.set("a.b.c", value)` 在 `globalThis.a.b` 上定义 `c`，`value` 经 `IntoJs` 转换。数、字符串、`Vec<T>`、
`BTreeMap<String, T>`、`Option<T>` 都行：

```rust,ignore
rt.set("process.argv", args)?;                                        // Vec<String> ⇒ 数组
rt.set("process.env", std::env::vars().collect::<BTreeMap<_, _>>())?; // map ⇒ 对象
```

父对象必须已经存在（由序言建）。dune 就这样设置 `process.argv` 等，序言不需要知道 Rust 的存在。

### 调用序言里的入口

把函数挂到 `__j8_native` 上，再 `eval` 一个对它的调用；参数要写成 JS 字符串字面量（见 dune 的 `Session::quote`）：

```rust,ignore
let call = format!("__j8_native.runMain({})", quote(file));
rt.eval(&call, file)?;
rt.run_until_idle()?;
```

更底层的活用 `rt.enter(|a| …)`，拿到上下文的 `Scope`（`a.get_named`、`a.call`、`a.local`、`a.to_string` ……）。
`Value` 出不了闭包，之后还要用就用 `a.global(v)` 挂到根上（`ScopeGlobal<'p, ValueBrand>`）。

## Error 的含义

| 变体 | 含义 | 宿主怎么做 |
|---|---|---|
| `Error::Exception` | JS 抛了，值照 V8 等在 isolate 的待处理槽里 | `rt.exception_text()` ⇒ 带调用栈的 `Uncaught …` |
| `Error::Syntax(msg)` | 源码解析不了 | 打印 `msg` |
| `Error::Unsupported(msg)` | j8 还不支持的写法 | 打印 `msg` |

## 调用栈

每个错误对象都带 V8 样式的 `stack`（`new Error()`、引擎抛的错误、绑定里 `a.throw` 抛的错误，后者最上一帧是调用绑定的那行 JS）：

```text
Error: boom
    at inner (main.js:2:10)
    at new Box (main.js:6:14)
    at main.js:9:1
```

- `stack` 照 TC39 提案是 `Error.prototype` 上的访问器属性（V8 则给每个错误各放一个）：读的时候按错误当时的 `name` 和 `message` 生成；赋一个字符串会在实例上放一个自有的 `stack` 挡在前面。`Error.stackTraceLimit`（10，建错误时从 `Error` 本身读）限制帧数；`Error.captureStackTrace(obj)` 给任意对象加上 `stack`。
- 错误只记下帧（代码、字节码偏移、是否构造调用），函数名、脚本、行列等到读 `stack` 时才查，所以建了没人读的错误不花格式化的开销。
- `a.stack_trace(limit)` 给出正在运行的 JS 帧，最内层在前，类型是 `CallSite { function, script, line, column, constructor }`（从 1 数，列按 UTF-16 码元；`Display` 按 `stack` 里的样子打印一帧）。`a.stack_text(v)` 取抛出值的 `stack`，没有时取 ToString；`Runtime::exception_text` 用的就是它。
- 位置来自编译器写在字节码旁的表（`BytecodeArray::positions`，字节码偏移 ⇒ 行和列），和 V8 一样标在语句、调用、`new` 和属性读取上。
- 还不显示：内置函数的帧、接收者的类型名（`Object.method`）、异步帧、`captureStackTrace` 要隐藏的帧。打开 JIT 时，以编译代码运行的函数可能缺帧，或者指向进入它时的那一行。

## 在别的运行时上扩展

```rust,ignore
j8::runtime! { pub Web  { natives: [Stdio, Clock, Codec, Random, Timer, Http], prelude: WEB_PRELUDE, } }
j8::runtime! { pub Dune: Web { globals: [Process], natives: [Fs, Os, Child], prelude: NODE_PRELUDE, } }
```

`Dune` 先装 `Web` 的全部东西，再装自己的，先跑 `Web` 的序言再跑自己的。模块查找先试子运行时的模块、再试父运行时的。
父运行时不变，所以第三方可以在不 fork 的情况下扩展 `Dune`。

## 打开 JIT

```rust,ignore
let executor = Executor::default().with_tier(j8::jit::Jit::default());
let rt = MyHost::runtime(set, executor)?;
```

## 样例里去哪找

| 你想…… | 看 |
|---|---|
| 建运行时、跑文件、报错、退出码 | `dune/src/cli/`、`dune/src/session.rs` |
| REPL（多次 `eval` 共享一个全局对象） | `dune/src/repl.rs` |
| 抛带 code 的 Node 式错误；把阻塞活放到别的线程 | `dune/src/host.rs` |
| 绑定普通对象（`stdio`、`clock`、`codec`、`random`） | `dune/src/web/mod.rs` |
| 可取消的 promise（`Glue::spawn`） | `dune/src/web/timer.rs` |
| 异步文件系统 | `dune/src/node/fs.rs` + `fs.js` |
| 给 JS 写的模块配自定义 `Platform`；`Runtime::set` | `dune/src/node/mod.rs` |
| 用 JS 写的模块加载器（`require`、`node_modules`、`package.json`） | `dune/src/node/loader.js` |
| 带 Rust 状态、只给序言用的类 | `samples/workerd/src/main.rs`（`KvStore`）+ `prelude.js` |
| 每个请求调一次 JS 定义的入口 | `samples/workerd/src/main.rs`（`Worker`） |
