# j8napi

[English](README.md) · **简体中文**

[j8](https://github.com/0x00-Community/j8) 的 [Node-API](https://nodejs.org/api/n-api.html)：原生插件（包括
[napi-rs](https://napi.rs) 写的模块）链接的那套 C 接口，实现在 j8 的引擎上。`napi_*` 函数按原名导出；嵌入方把它们链接进来
（静态链接，或者从可执行文件导出给运行时加载的插件）。

## 用法

```rust
// 在某次 `Isolate::enter` 里，拿着插件所属 context 的 agent
let exports = j8napi::Env::open(&mut scope, napi_register_module_v1)?;
scope.set_named(scope.context.global.into(), "addon", exports)?;
```

`Env::open` 建插件的环境（和 context 的 global 对象一样长寿），调注册函数，给出它的 exports。

## 怎么落到 j8 上

- `napi_value` 是环境句柄栈上的一格，存值的原始字；句柄只在一次进入 isolate 里有效，期间不回收。
- 插件的函数是能 `new` 的 `NativeClosure`：引擎按 `new.target.prototype` 造好 `this`，所以 JS 类能继承插件的类。
- `napi_wrap` 与类型标签放在对象的私有成员里；被回收的宿主对象把终结回调排进队列，下次进回调时在 JS 线程上跑。
- `napi_ref`、挂起的异常、deferred 都拿持久句柄。

## 与 Node 的差异

- 计数降到 0 的 `napi_ref` 仍是强引用（堆还没有弱引用）。
- 字符串按 UTF-8 存：UTF-16 输入里落单的代理项会被替换。
- Buffer 就是普通的 `Uint8Array`（没有 Node 的 `Buffer` 类）。
- 异步工作与线程安全函数要 isolate 带事件循环（`EventLoopSet`）；异步工作各开一个线程跑。
  和 Node 一样，线程安全函数在释放或 `napi_unref_threadsafe_function` 之前让事件循环一直跑（`run_until_idle` 等着它）。
  没有 libuv 循环（`napi_get_uv_event_loop` 失败）。

## 许可

MIT，随 [j8](https://github.com/0x00-Community/j8) 一起。
