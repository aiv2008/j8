# j8compiler

**English** · [简体中文](README.zh-CN.md)

The compiler of [j8](https://github.com/0x00-Community/j8): JavaScript ⇒ j8 bytecode (`j8bytecode::Module`), parsed
with [oxc](https://oxc.rs). Every piece of JS goes through here before it runs — scripts, ES modules, and dynamic code
(`eval`, `Function`); there is no AST interpreter.

```text
source ─ oxc parser + semantic checks ─▶ AST ─ scope resolution ─ code generation ─ peephole fusion ─▶ Module
```

## Usage

```rust
use j8compiler::{Compile, DebugInfo};
use j8framework::{CodeKind, Source};

let module = j8bytecode::Module::compile(&Source { text: "1 + 2", file: "a.js" }, CodeKind::Script, DebugInfo::Omit)?;
let (module, info) = j8bytecode::Module::compile_module(&source, DebugInfo::Omit)?;  // an ES module plus its imports / exports
```

`Compile::compile_direct` compiles direct-eval code against the call site's scope table. `DebugInfo::Record` also keeps
statement positions and local variable names for a debugger (`BytecodeArray::debug`). `j8executor` calls all of
these itself; embedders normally don't.

## License

MIT, as part of [j8](https://github.com/0x00-Community/j8).
