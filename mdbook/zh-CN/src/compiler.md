# 编译器：JS ⇒ 字节码

`crates/j8compiler` 把 JavaScript 编成 [V8 字节码](bytecode.md)（`j8bytecode::Module`），用 [oxc](https://oxc.rs) 解析。
**所有 JS 运行前都先过这里**——脚本、ES 模块、动态代码（`eval`、`Function`）。j8 不设 AST 解释层。

```text
源码 ─ oxc 解析 + 语义检查 ─▶ AST ─ 作用域解析 ─▶ 代码生成（BytecodeGenerator）─ BytecodeArrayBuilder ─▶ Module
```

## 文件

| 文件 | 内容 | 照 V8 的 |
|---|---|---|
| `scope.rs` | 每层作用域声明了哪些名字（`Decls`），哪些绑定被内层函数捕获（`Resolver`） | `Scope` 分析 |
| `compiler.rs` | 作用域、函数、语句 | `BytecodeGenerator` |
| `expr.rs` | 表达式（结果总是留在 acc） | `BytecodeGenerator` |
| `builder.rs` | 发射一个函数的字节码 | `BytecodeArrayBuilder` / `BytecodeArrayWriter` / `BytecodeRegisterAllocator` / `ConstantArrayBuilder` / `HandlerTableBuilder` / `FeedbackVectorSpec` |

## 解析与早期错误

oxc 解析出 AST，再用 `oxc_semantic` 做语义检查。规范要求的**早期错误**（重复声明、非法的 `break` 目标、
严格模式下的禁用写法……）在这里报 `SyntaxError`；正则字面量也在解析时校验。j8 还不支持的写法报
`Error::Unsupported`，带 `文件:行:列`。

## 变量放哪里

和 V8 一样，变量按「谁会看到它」决定位置：

| 变量 | 位置 | 字节码 |
|---|---|---|
| 函数自己的、没被捕获的变量 | 寄存器（参数是 `a0` …，`this` 是 receiver） | `Ldar` / `Star` |
| 被内层函数捕获的变量 | 上下文槽：每个有捕获的作用域层一个上下文 | `LdaCurrentContextSlot` / `LdaContextSlot` |
| 脚本顶层的 `var` / 函数 | 全局对象的属性（`DeclareGlobals` 声明） | `LdaGlobal` / `StaGlobal` |
| 脚本顶层的 `let` / `const` / `class` | 脚本上下文（跨脚本共享，不挂在全局对象上） | `LdaCurrentContextSlot` … |
| `with` 体内、或可能被宽松模式直接 eval 看到的名字 | 按名字查 | `LdaLookupSlot` |

预扫和代码生成用同一份 `Decls`，保证两边的名字解析一致。`for (let …)` 每一轮复制一份上下文，闭包各拿各的那一轮。

暂时性死区（TDZ）：`let` / `const` / `class` 在初始化之前读写要抛 `ReferenceError`。能在编译期证明已经初始化的使用处，
检查直接省掉。

## 指令选择

`builder.rs` 按 V8 的方式挑字节码：

- `Star` 到 r0 … r15 用单字节的短 `Star`，0 用 `LdaZero`，小整数用 `LdaSmi`；
- 调用按参数个数和 receiver 选 `CallUndefinedReceiver0/1/2`、`CallProperty0/1/2`、`CallAnyReceiver`……；
- 内建的固有操作走 `InvokeIntrinsic`，运行时函数走 `CallRuntime`；
- 前向跳转先占位、再回填；偏移装不进操作数时，用一个预留的常量池项撑着（`JumpConstant` 系列）；
- 寄存器按栈的次序分配，反馈槽按出现顺序编号。

## 异常与 finally

`try` 块在处理表里登记 `[start, end)` 范围与处理位置；抛出时执行器查表，找不到就展开到调用者。
`finally` 用「完成类型 + 值」一对寄存器实现：穿过它的 `return` / `break` / `continue` 先绕进 `finally`，再按记下的
完成类型接着走（`ContinuationToken`）。

## 动态代码

- **间接 eval / `Function`**：当作新脚本编译，跑在全局作用域。
- **直接 eval**：`Compile::compile_direct` 按调用点的**作用域表**编译。调用点能看到的每个绑定都进环境，
  eval 代码接在调用点的环境上跑；宽松模式下 eval 里的 `var` / 函数落进调用它的函数（已有同名绑定就复用，
  否则进一个像 `with` 一样按名字查的 eval 变量对象）；词法冲突报 `SyntaxError`。
- **生成器 / async 函数构造器**：`GeneratorFunction` 等同理，编出对应 `FunctionKind` 的函数。

## 用法

宿主一般不直接调编译器，由执行器自己调用：

```rust,ignore
use j8compiler::{Compile, DebugInfo};
use j8framework::{CodeKind, Source};

let module = j8bytecode::Module::compile(&Source { text: "1 + 2", file: "a.js" }, CodeKind::Script, DebugInfo::Omit)?;
let (module, info) = j8bytecode::Module::compile_module(&source, DebugInfo::Omit)?;   // ES 模块，连同导入 / 导出信息
```

`DebugInfo::Record` 另外为[调试器](debugger.md)记下语句位置和局部变量名（`BytecodeArray::debug`）；执行器只在接了调试器时才要它。
