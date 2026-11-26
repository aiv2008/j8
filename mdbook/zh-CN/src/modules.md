# 模块

## 文件里的 ES 模块

`rt.eval_module(text, file)` 把 `text` 当 ES 模块跑。`DefaultPlatform` 按引用方的文件解析 `import`（规范化路径，不处理
`node_modules`）。别的规则（包、URL、虚拟文件系统）就自己写 `Platform::load_module`。

`eval_module` 返回这个模块求值的 promise。没有顶层 `await` 时，`eval_module` 返回时它已经兑现；有顶层 `await` 时，
由 `run_until_idle` 推进等待中的模块，整张模块图跑完它才落定。模块在第一次挂起前抛出的错误，`eval_module` 仍然直接返回错误。

执行器侧：加载、链接是同步的；求值按规范的循环模块记录来做，用了顶层 `await` 的模块当作 async 函数跑，导入它的模块等它
跑完再跑。import 绑定实时读导出方的槽；`ResolveExport` 处理间接导出、`export *`、歧义；支持命名空间对象、动态
`import()`、`import.meta`。

## Rust 写的原生模块：`#[j8::module]`

```rust,ignore
#[j8::module("myhost:kv", "kv")]
impl Kv {
    fn get(key: &str) -> Option<String> { /* … */ }
}

j8::runtime! { pub MyHost { modules: [Kv], } }
```

`import kv, { get } from "myhost:kv"` 经 `HostPlatform` 工作。序言可以用 `__j8_native["myhost:kv"]` 拿到同一个对象，
比如用来实现 `require`。

## 自定义平台

dune 的 `node:` 模块是序言建的 JavaScript 对象，不是 Rust 类型。它用自己的平台 `NodePlatform`（`dune/src/node/mod.rs`）
把它们供给 `import`：`load_module` 返回一小段生成的 ES 模块，从 `__j8_native` 重新导出一个对象；别的说明符交给里面那层平台。

```rust,ignore
impl<P: Platform> Platform for NodePlatform<P> {
    fn load_module(&self, referrer: &str, specifier: &str) -> Option<(String, String)> {
        match Builtin::try_from(specifier) {
            // "const m = globalThis.__j8_native.builtin(\"fs\"); export default m; export const readFileSync = m.readFileSync; …"
            Ok(b) => Some((format!("node:{}", b.spec().0), b.source())),
            Err(()) => self.inner.load_module(referrer, specifier),
        }
    }
    fn now(&self) -> Duration { self.inner.now() }
    fn wall_clock(&self) -> Duration { self.inner.wall_clock() }
    fn print(&self, line: &str) { self.inner.print(line); }
}

impl<P: Pool> Pool for NodePlatform<P> {               // Platform 要求 Pool：也转发
    fn threads(&self) -> usize { self.inner.threads() }
    fn run(&self, jobs: Vec<Task<'_>>) { self.inner.run(jobs); }
}
```

返回值的第一个元素是模块的规范名，模块按它缓存：第二次解析到同一个名字的 `import` 拿到同一个模块。解析成 `None` 的说明符让
import 失败，报 `Error::Syntax("… Cannot find module '…'")`。

## CommonJS

j8 本身不知道 CommonJS。dune 的 `require`、`module`、`node_modules` 与 `package.json`（`main` / `exports` / `imports`）解析
都是序言里的 JavaScript（`dune/src/node/loader.js`），ESM 侧的裸说明符解析在 `dune/src/node/esm.rs`。见 [dune](dune.md)。
