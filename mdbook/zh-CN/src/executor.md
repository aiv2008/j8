# 执行器：解释字节码

`crates/j8executor` 执行 [V8 字节码](bytecode.md)：`Executor<'p>` 是执行入口，事件循环也在这里。它目前是 j8 的最后一层：固定带着
[编译器](compiler.md)，脚本、模块、`eval` / `Function` 都先编成字节码再跑。

## 一个 Executor 服务一个 isolate

`Executor` 持有加载过的函数（`Loaded`），堆上的闭包按 `FuncId` 指回来。所以一个 executor 只服务一个 isolate；
多个 isolate 要共享编译结果，用 `CodeCache`（见下文）。

## 帧与寄存器

- **显式帧栈**：JS 调 JS 不靠宿主（Rust）递归，执行器自己维护帧栈。调用深度上限 10,000（含宿主函数里重入的 JS），
  超了抛 `RangeError`。
- **宿主栈另有按字节数的上限**：调用深度上限按层数计，管的是堆上的执行器帧栈；而宿主函数回调 JS（`join` → `toString`
  → `join`、Proxy 的陷阱、`Function.prototype.call`）或自己递归（`JSON.stringify` / `JSON.parse` / reviver、
  `Array.prototype.flat`、不带陷阱层层嵌套的 Proxy）用的是宿主栈，每层用多少不一定。`Scope::check_stack` 在栈比进入
  isolate 时深了 `CreateParams::stack_size`（默认 984 KiB，即 V8 的 `--stack-size`）时抛 `RangeError`。检查点在宿主
  调回 JS 的地方（`Invoke` 的 call / construct / resume）、每次查 Proxy 陷阱时，以及上面那些递归的每一步。进入
  isolate 的线程在进入点以下要留出这么多栈，再加上检查失败时正在运行的宿主函数要用的一点：默认设置下 1 MiB。
- **一个寄存器文件**：全部帧共用。一帧占其中一段：receiver、形参、r0 起的寄存器。字节码里的寄存器操作数是 V8
  的帧槽号（r0 是 −7、receiver 是 +2、上下文 −1、闭包 −2），加载时映射到这段上。
- **上下文**就是堆上的 `Env`：寄存器与累加器在 `CreateFunctionContext` / `PushContext` / `PopContext` 之间以
  `Value::context` 字携带它们。

## 线程化代码

函数加载时字节码只解码一次，每条解码后的指令带着跑它的**处理函数**（`threaded.rs` 的 `Handler`）：

```text
handler(cx, pc, regs, acc)  ─ 做完 ─▶  直接跳到下一条指令的 handler
```

处理函数把热状态（当前指令、本帧 r0 指针、累加器）放在机器寄存器里传。用 nightly 编译器时是有保证的尾调用
（`become`，`build.rs` 探测到 nightly 就打开 `cfg(tail_calls)`）；稳定版编译器上则返回到 `Executor::run` 的循环，
由它调下一个。

热门字节码有自己的处理函数；其余字节码、以及热门字节码离开快路时，经 `slow` 同步状态后走通用的 `Executor::step`。
帧变化（调用、返回）、异常、执行结束时离开处理函数链。

处理函数放在 `Cell` 里：[调试器](debugger.md)设断点时把它换掉，清除时换回，不碰的指令不做任何检查。

## 内联缓存

`ic.rs`：每个 `GetNamedProperty` / `SetNamedProperty` / `DefineNamedOwnProperty` 点一个缓存。

- **只按 shape 地址识别**：GC 会搬对象，缓存里不能存对象指针；另存一个 `Weak`，防止认出的地址被别的 shape 复用。
- 字典模式的 shape 原地修改（地址不变、内容变了），所以从不缓存。
- 读：自有数据属性，或往上 1 到 `DEPTH` 层原型上的数据属性（路上每一层的 shape 都检查）。
- 写：自有的可写数据属性。
- 加属性：旧 shape ⇒ 新 shape；赋值还要检查原型链每一层的 shape（没有谁后来加了同名 setter 或只读属性）。

这些缓存同时就是 [JIT](jit.md) 的**类型反馈**：JIT 按缓存见过的 shape 推测，生成 shape 守卫加直接的槽读写。

## 分片执行

`Executor::eval` 拿 `&mut Isolate`，自己分片进入：每片最多一万条指令，停下时把执行状态挂到根上、让堆回收一段，
再接着跑。执行环境以 `ScopeGlobal<'p, ContextBrand>` 跨片保留。每 1024 条指令检查一次预算、终止与中断请求——在事件循环里，这就是检查
任务队列的时机（[事件循环](event-loop.md)）。

已知偏差：宿主函数里重入的 JS（如 `map` 的回调）在当前片里跑完，期间不回收。`Function.prototype.call` / `apply`
由执行器直接展开成普通调用，不算重入。

## 异常

查字节码的**处理表**：抛出时在本帧找覆盖当前偏移的处理项，找到就恢复上下文寄存器、跳到处理位置；找不到就展开到调用者。

## 生成器与 async

生成器、async 函数、async 生成器都是同一套字节码（`SuspendGenerator` / `ResumeGenerator` / `SwitchOnGeneratorState`），
挂起时把寄存器存进生成器对象。`await` 经 `j8framework` 的 Promise 机制驱动协程；脚本跑完后排空微任务队列。

## ES 模块

`module.rs`：加载、链接（同步：依赖经 `Platform::load_module` 读进来）；求值（含顶层 `await`）是 j8framework 的
`Scope::evaluate_module`。import 绑定实时读导出方的槽，`ResolveExport` 处理间接导出、`export *` 与歧义；命名空间对象、
动态 `import()`、`import.meta` 都支持。

## CodeCache

很多 isolate 跑同样的序言、导入同样的模块文件时，给它们的 executor 一个共享的 `CodeCache`：

- 每份源码只解析、编译一次，每个 executor 加载自己的一份拷贝；
- 只放该共享的东西（以 `ScriptKind::Shared` 运行的脚本与 ES 模块），一次性的脚本不入缓存；
- 不存源码：按文件名、长度和文本的 128 位哈希（两个随机密钥的 SipHash，碰撞造不出来）查找。

```rust,ignore
let code = CodeCache::shared();
let executor = Executor::default().with_cache(Arc::clone(&code));
```

## JIT 钩子

`Executor::with_tier(tier)` 挂上一个 `Tier`（[JIT](jit.md)）。执行器在这些地方问它：

| 时机 | 调用 |
|---|---|
| 一次调用即将从第一条指令跑一帧（每 64 次调用问一次） | `Tier::enter` |
| 某个 `JumpLoop` 回跳了很多次 | `Tier::osr` |
| 帧从中途继续（暂停后、机器码交回来后） | `Tier::resume` |
| 已编译函数之间的普通调用，不建解释器帧 | `Tier::call` |

回答 `Interpret` 就照常解释；`Resumed` 表示机器码跑过了，帧已经按 `Entry::resume` 设好，解释器从栈顶那帧接着跑；
`Threw` 表示机器码里抛了异常。`jit.rs` 定义了 JIT 能看到的全部东西：解码后的字节码与反馈（`Function`）、
即将跑的帧（`Entry`）、机器码把执行交回解释器的方式（`DeoptFrame`）。

## 用法

```rust,ignore
let mut executor = j8executor::Executor::default();
let result = executor.eval(&mut isolate, &cx, &j8framework::Source { text: "1 + 2", file: "a.js" })?;
```

`javascript_semantics` 测试是一张逐条确定的 JS 语义表。
