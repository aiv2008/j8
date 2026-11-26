# 平台

`Platform` 是宿主借给引擎的东西：线程池、时钟、`console.log` 的输出、模块加载。`crates/j8platform` 是它的标准库实现
`DefaultPlatform`。

## DefaultPlatform

- **线程池**：[moremath](https://crates.io/crates/moremath) 的 `ThreadMulti`，起一次反复用，任务可借用调用方数据
  （经 `j8framework::Pool`）；后台任务与并行内建函数跑在上面。GC 堆有自己的回收线程。
- **时钟**：单调时钟用 `Instant`，`Date` 用 `SystemTime`。
- **模块**：相对路径按引用方所在目录解析，从磁盘读（不处理 `node_modules`）。
- `console.log` 打到标准输出。
- **语言区域与时区**：构造时读一次，语言区域取 `LC_ALL` / `LC_MESSAGES` / `LANG`（`zh_CN.UTF-8` 变成 `zh-CN`），时区取
  `TZ` 或 `/etc/localtime` 链接。开 `intl` feature 时 `Intl` 与 `Date` 用它们。

```rust,ignore
let platform = j8platform::DefaultPlatform::default();            // 每个 CPU 一个工作线程
let small = j8platform::DefaultPlatform::from(NonZeroUsize::MIN);  // 单线程
let mut isolate = j8framework::Isolate::new(&platform, Default::default());
```

## trait

```rust,ignore
pub trait Pool: Sync {
    fn threads(&self) -> usize;
    fn run(&self, jobs: Vec<Task<'_>>);          // 跑完一批才返回；任务借用调用方数据
}

pub trait Platform: Pool {
    fn now(&self) -> Duration;                    // 单调时钟
    fn wall_clock(&self) -> Duration;             // Date 用
    fn print(&self, line: &str);                  // console.log
    fn load_module(&self, referrer: &str, specifier: &str) -> Option<(String, String)>;  // (规范化名字, 源码)
    fn unhandled_rejection(&self, scope: &mut Scope, promise: JsObject, reason: Value);           // 默认什么也不做
    fn locale(&self) -> Option<String>;           // BCP 47 标签，默认 None（en-US）
    fn time_zone(&self) -> Option<String>;        // IANA 名或 ±HH:MM，默认 None（UTC）
}
```

（示意，完整签名见 `j8framework/src/platform.rs`。）

## 叠平台

平台都不持有异步任务（那些住在每个运行时的 isolate 里），所以可以任意叠加、也可以被多个运行时共享。dune 的
`NodePlatform` 包着 `DefaultPlatform`：拦下 `node:` 内建模块的说明符，其余转给里面那层：

```rust,ignore
let platform = Dune::platform(NodePlatform::from(DefaultPlatform::default()));
```

可以覆盖的方法：`print`（把 `console.log` 接到别处）、`now` / `wall_clock`（测试用假时钟）、`locale` / `time_zone`
（用用户设置而不是进程环境）、`load_module`（包、URL、
虚拟文件系统）。写法见[模块](modules.md#自定义平台)。
