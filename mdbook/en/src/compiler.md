# Compiler: JS ⇒ bytecode

`crates/j8compiler` compiles JavaScript to [V8 bytecode](bytecode.md) (`j8bytecode::Module`), parsing with
[oxc](https://oxc.rs). **Every piece of JS goes through it before running**: scripts, ES modules and dynamic code
(`eval`, `Function`). j8 has no AST interpreter.

```text
source ─ oxc parse + semantic checks ─▶ AST ─ scope resolution ─▶ codegen (BytecodeGenerator) ─ BytecodeArrayBuilder ─▶ Module
```

## Files

| File | Contents | After V8's |
|---|---|---|
| `scope.rs` | which names each scope level declares (`Decls`), which bindings inner functions capture (`Resolver`) | scope analysis |
| `compiler.rs` | scopes, functions, statements | `BytecodeGenerator` |
| `expr.rs` | expressions (results always in acc) | `BytecodeGenerator` |
| `builder.rs` | emitting one function's bytecode | `BytecodeArrayBuilder` / `BytecodeArrayWriter` / `BytecodeRegisterAllocator` / `ConstantArrayBuilder` / `HandlerTableBuilder` / `FeedbackVectorSpec` |

## Parsing and early errors

oxc produces the AST and `oxc_semantic` checks it. The **early errors** the spec requires (duplicate declarations,
bad `break` targets, strict-mode restrictions …) are reported here as `SyntaxError`; regex literals are validated at
parse time too. Syntax j8 does not support yet is reported as `Error::Unsupported` with `file:line:column`.

## Where variables live

As in V8, by who can see the variable:

| Variable | Lives in | Bytecode |
|---|---|---|
| a function's own variables nobody captures | registers (parameters `a0` …, `this` the receiver) | `Ldar` / `Star` |
| variables inner functions capture | context slots: one context per scope level that has any | `LdaCurrentContextSlot` / `LdaContextSlot` |
| top-level script `var`s / functions | properties of the global object (declared by `DeclareGlobals`) | `LdaGlobal` / `StaGlobal` |
| top-level script `let` / `const` / `class` | the script context (shared across scripts, not on the global object) | `LdaCurrentContextSlot` … |
| names inside `with`, or reachable by a sloppy direct eval | looked up by name | `LdaLookupSlot` |

The pre-scan and code generation use the same `Decls`, so name resolution agrees. A `for (let …)` copies its context
every iteration, so closures each get their own iteration.

The temporal dead zone: reading or writing a `let` / `const` / `class` before initialization throws a
`ReferenceError`. Uses proven initialized at compile time skip the check.

## Instruction selection

`builder.rs` picks bytecodes the way V8 does:

- `Star` to r0 … r15 is a one-byte short `Star`, 0 is `LdaZero`, small integers are `LdaSmi`;
- calls pick `CallUndefinedReceiver0/1/2`, `CallProperty0/1/2`, `CallAnyReceiver` … by argument count and receiver;
- intrinsics go through `InvokeIntrinsic`, runtime functions through `CallRuntime`;
- forward jumps are patched; when an offset doesn't fit its operand, a reserved constant pool entry backs it (the
  `JumpConstant` family);
- registers are allocated in stack order, feedback slots numbered in order of appearance.

## Exceptions and finally

A `try` block registers its `[start, end)` and handler in the handler table; a throw looks it up, and unwinds to the
caller when nothing covers it. `finally` uses a pair of registers, completion kind and value: a `return` / `break` /
`continue` crossing it detours through the `finally` and then carries on as recorded (`ContinuationToken`).

## Dynamic code

- **Indirect eval / `Function`**: compiled as a new script, run in the global scope.
- **Direct eval**: `Compile::compile_direct` compiles against the call site's **scope table**. Every binding visible at
  the call site goes into the environment and the eval code runs chained onto the call site's environment; sloppy
  `var`s / functions in eval land in the calling function (reusing a binding of the same name, otherwise going into an
  eval variable object looked up like `with`); lexical conflicts raise `SyntaxError`.
- **Generator / async function constructors**: `GeneratorFunction` and friends compile a function of that
  `FunctionKind`.

## Use

Hosts rarely call the compiler; the executor does:

```rust,ignore
use j8compiler::{Compile, DebugInfo};
use j8framework::{CodeKind, Source};

let module = j8bytecode::Module::compile(&Source { text: "1 + 2", file: "a.js" }, CodeKind::Script, DebugInfo::Omit)?;
let (module, info) = j8bytecode::Module::compile_module(&source, DebugInfo::Omit)?;   // an ES module, with its imports / exports
```

`DebugInfo::Record` also keeps statement positions and local variable names for a [debugger](debugger.md)
(`BytecodeArray::debug`); the executor asks for it only when a debugger is attached.
