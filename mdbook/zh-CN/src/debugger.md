# 调试器

`j8executor` 为嵌入方提供调试器：按脚本和行号设断点，在异常和 `debugger` 语句处暂停，单步进入、跳过、跳出，列出调用帧及其作用域和变量，在暂停的帧里求值表达式。这些足以在上面实现 Chrome DevTools 协议的 Debugger 和 Runtime 两个域。

## 接上调试器

```rust,ignore
struct Inspector;

impl j8executor::Debugger for Inspector {
    fn paused(&self, p: &mut j8executor::Paused<'_, '_, '_, '_>) -> j8executor::Action {
        let frames = p.frames();                       // CallSite，最内层在前：函数、脚本、行、列
        let scopes = p.scopes(0);                      // Local，然后由内向外的各层上下文，最后是 Global
        let v = p.evaluate(0, "x + 1");                // 在最上层的帧里求值，帧的局部变量可见
        j8executor::Action::StepOver                   // 或 Continue、StepInto、StepOut
    }
}

let inspector = Inspector;
let executor = Executor::default().with_debugger(&inspector);
isolate.enter(&cx, &executor, |scope| {
    let bp = executor.set_breakpoint(scope, "main.js", 7, 0);    // Some(Breakpoint { id, location })
    executor.pause_on_exceptions(PauseOnExceptions::Uncaught);
});
```

- 执行器借用调试器（`&'p (dyn Debugger + Sync)`），调试器要比执行器活得长。
- 每次停下，`paused` 都在 JS 线程上、在这次执行里面被调用；`Paused::reason` 说明原因（`Breakpoint(ids)`、`Step`、`Pause`、
  `debugger;` 对应的 `Statement`、`Exception { value, uncaught }`）。它运行期间 JS 停着；要和前端（DevTools）通信的嵌入方
  在 `paused` 里跑自己的消息循环，返回前端要的动作。读到的值是这次暂停所在进入的 `Value<'scope>`，`Paused::scope` 就是那次
  进入的 `Scope`，可以用平常的操作去读。`frames` 和 `Scope::stack_trace` 给出的是同样的 `CallSite`。
- `set_breakpoint(scope, script, line, column)` 解析到同名脚本各函数中位于 (line, column) 处或其后的第一条语句（行列从 1 开始，
  与调用栈一致），返回实际落点。脚本还没加载时请求会保留：同名的代码下次运行时再解析，此前 `location` 为 `None`。
  `clear_breakpoint(id)` 删除断点。
- `evaluate(frame, source)` 把源码按直接 eval 编译，挂在该帧的上下文上，前面加一层该帧寄存器里的局部变量，以该帧的 `this`
  运行。对放在上下文里的变量赋值会保留；对寄存器局部变量赋值只改表达式看到的那份副本。暂停不嵌套：`evaluate` 运行的代码里的
  断点不会停。
- 还没初始化的 `let` / `const`（处在暂时性死区）读作 `undefined`。
- `pause()` 在下一条要执行的语句处停下（`Reason::Pause`），即 DevTools 的暂停按钮。执行器在事件循环里时，别的线程经
  `Interrupter`（见[框架层](framework.md)）够到它：`interrupter.interrupt(|executor, _| executor.pause())` 让忙循环在下一次检查时停下，
  `set_breakpoint`、`clear_breakpoint`、`pause_on_exceptions` 也这样发过去，循环空闲或正在运行都会生效。

## 实现方式和开销

- **断点就是换一个处理函数。** 每条解码后的指令都带着自己的处理函数（见[执行器](executor.md)）；设断点就是把语句第一条指令的
  处理函数换成 `brk`，它先报告调试器，再运行这条指令原本的处理函数。清除断点时换回原来的。处理函数放在 `Cell<Handler>` 里，
  读取仍是一次加载，所以没有断点的代码运行方式和以前完全一样，不在每条指令上做任何检查。
- **调试信息只为调试器编译。** 带调试器的执行器以 `DebugInfo::Record` 编译：编译器记下语句位置（断点和单步能停的地方），以及
  哪个寄存器在哪段字节码范围里存哪个变量，放在 `BytecodeArray::debug` 里。没有调试器时以 `DebugInfo::Omit` 编译，这张表为
  `None`，每个函数只多一个字；调用栈读的表达式位置两种方式完全相同，代码缓存把两种结果分开存。在 9 MB 的脚本上（TypeScript
  的 `typescript.js`，21,693 个函数），记录调试信息多花约 1% 的编译时间、约 4 MB 内存：98,597 个语句位置，49,174 个局部变量
  （名字共 0.45 MB），每个有内容的函数一张表。
- **单步只武装够得着的语句。** `StepOver`、`StepOut` 武装栈上各函数的语句；`StepInto` 和 `pause()` 另外武装每个函数（包括期间
  新加载的）的第一条语句，下一个进入的函数就停在那里。武装过的在下一次暂停时全部换回。`brk` 按帧深度决定停不停：进入是任意深度，
  跳过是同层或更浅，跳出是更浅。
- **异常只报告一次**，在执行器第一次展开它的地方。重新抛出（`ReThrow`，或异常穿过内建函数从嵌套的执行里出来）会在待处理异常上
  做标记，不再报告。会不会被捕获按各处理器的预测判断，做法同 V8：`catch` 算捕获；`finally` 和迭代器关闭会重新抛出，继续往外找；
  异步函数里的异常会拒绝它的 promise，这个 promise 已经有处理者（`await` 或 `then`）时算捕获，否则继续看调用者。
- **调试时不用 JIT。** `with_debugger` 会去掉[分层](jit.md)，所有函数都解释执行，每个断点都能停到。没有调试器时一切照旧：
  分层和热路径都不受影响。

## 还没有的

- 按 URL 模式设断点、条件断点和日志点（嵌入方可以在 `paused` 里求值条件后返回 `Continue`）。
- 修改变量（`setVariableValue`）、重启帧，以及已经挂起的异步函数的帧。
- 单步进入恢复执行的生成器或异步函数，以及从最后一帧跳出到下一个任务。
- JIT 开着时暂停：调试器在构造执行器时接上。
