# j8compiler

[English](README.md) · **简体中文**

[j8](https://github.com/0x00-Community/j8) 的编译器：JavaScript ⇒ j8 字节码（`j8bytecode::Module`），用
[oxc](https://oxc.rs) 解析。所有 JS 运行前都先过这里——脚本、ES 模块、动态代码（`eval`、`Function`）；不设 AST 解释层。

```text
源码 ─ oxc 解析 + 语义检查 ─▶ AST ─ 作用域解析 ─ 生成代码 ─ 窥孔融合 ─▶ Module
```

## 用法

```rust
use j8compiler::{Compile, DebugInfo};
use j8framework::{CodeKind, Source};

let module = j8bytecode::Module::compile(&Source { text: "1 + 2", file: "a.js" }, CodeKind::Script, DebugInfo::Omit)?;
let (module, info) = j8bytecode::Module::compile_module(&source, DebugInfo::Omit)?;  // ES 模块，连同导入 / 导出信息
```

`Compile::compile_direct` 按调用点的作用域表编直接 eval 的代码。`DebugInfo::Record` 另外为调试器记下语句位置和局部变量名
（`BytecodeArray::debug`）。这些都由 `j8executor` 自己调用，宿主一般不直接用。

## 许可

MIT，随 [j8](https://github.com/0x00-Community/j8) 一起。
