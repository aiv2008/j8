# test262 与测试

## test262

`tests/test262` 是 [tc39/test262](https://github.com/tc39/test262) 子模块（浅克隆，`git submodule update --init` 拉取），
运行器 `tests/test262.rs` 是根包的测试（`harness = false`，不发布）。

```sh
cargo test --release --test test262                                                   # 只跑基线里的用例，掉了就失败
cargo test --release --test test262 -- language/expressions/addition built-ins/Math   # 跑这些目录
J8_TEST262_JIT=1 cargo test --release --test test262                                  # 开 JIT（每个函数第一次调用就编）对拍
J8_TEST262_VERBOSE=1 cargo test --release --test test262 -- <路径>                     # 逐条列出没过的原因
J8_TEST262_UPDATE=1 cargo test --release --test test262 -- <路径>                      # 结果写回基线
```

- 每个用例一个子进程、10 秒超时；结果分 pass / fail / unsupported（j8 报 `Error::Unsupported`）/ skip / timeout / crash，按目录汇总；
- 基线 `tests/test262.passing`：过了的用例一行一个，当前 **42,500** 行；不带参数时只跑它做回归检查；
- 非严格模式与 `onlyStrict`（严格模式跑）；模块用例先跑 harness 脚本再跑模块；
- `$262` 宿主对象提供 `global` / `evalScript` / `gc` / `detachArrayBuffer`（`createRealm` 抛 TypeError）；
- 不支持写法的报错行号含拼进去的 harness。

**每次提交零回归**是 j8 的硬规矩，开着 JIT 也要过同一份基线（JIT 阈值设成 0，每个函数第一次调用就编，最大化覆盖）。

## 其他测试

| 测试 | 内容 |
|---|---|
| `tests/runtime.rs`、`tests/event_loop.rs`、`tests/terminate.rs`、`tests/interrupt.rs`、`tests/heap_limit.rs` | 运行时、事件循环、中止执行、中断、堆上限 |
| `tests/binding.rs`、`tests/bindings_more.rs` | 绑定宏 |
| `crates/j8bytecode/tests/v8.rs` | 解码 V8 打印的 738 条指令，逐条核对 |
| `crates/j8executor` 的 `javascript_semantics` | 逐条确定的 JS 语义表 |
| `crates/j8jit/tests`、`j8taskqueue/tests`、`j8lockfree/tests`、`j8napi/tests` | 各 crate 自己的测试 |
| `samples/workerd/tests` | 聊天室压测、一千个 Worker |
| `samples/jumpheap/tests/three.rs` | 三进程跨堆矩阵 |

## 怎么加一个特性

[路线图](roadmap.md)里每一阶段的流程：

1. 先往语义表加用例；
2. clippy 干净；
3. 在快照里跑 test262 基线，零回归；
4. 用 `J8_TEST262_UPDATE=1` 把相关目录里新通过的用例记进基线；
5. 提交、推送（j8 在 GitHub 上走分支 + PR）。

单元测试只钉确定的事实，每次只跑一个，用 `timeout 15` 限时。
