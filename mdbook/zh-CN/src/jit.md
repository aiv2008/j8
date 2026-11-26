# JIT：推测式优化层

`crates/j8jit` 是 j8 的 JIT（x86-64 Unix 和 Windows、AArch64 Linux）。它实现执行器的 `Tier`：热函数与热循环依据内联缓存的反馈建成**推测性的
SSA 图**，优化后出 x86-64 或 AArch64 机器码，在解释器的帧上直接跑；推测落空就**去优化**回解释器。

```text
热函数 / 热循环 ─ build（字节码 + 内联缓存 ⇒ 推测图）─ opt ─ codegen ─▶ 机器码，跑在解释器的帧上
守卫失败 ─▶ j8_deopt 重建解释器帧；留下的提示决定下次怎么编
```

j8 的路线是**跳过基线编译器、直接做推测式优化层**（内联、逃逸分析、去优化、OSR），目标是在常用基准上追上带 JIT 的 V8。

## 打开它

```rust,ignore
use std::rc::Rc;
let executor = j8executor::Executor::default().with_tier(Rc::new(j8jit::Jit::default()));
```

- `dune --jit`、`J8_TEST262_JIT=1`（test262 开着 JIT 跑同一份基线）打开它；
- `Jit::from(n)` 设热度阈值（缺省 `Jit::THRESHOLD` = 64），0 表示每个函数第一次调用就编；
- 调试环境变量：`J8_JIT_LOG`（编译与去优化日志）、`J8_JIT_IR`（打印图）、`J8_JIT_BIN=<目录>`（把机器码写成文件）。

## 三种入口

| `Kind` | 什么时候 | 怎么跑 |
|---|---|---|
| `Enter` | 解释器调用一个热函数 | 在解释器给它建的帧上跑 |
| `Osr(at)` | 循环头 `at` 回跳得够多 | 在当前帧上从循环中途切进去 |
| `Native(n)` | 已编译代码调用已编译代码 | 经被调者的蹦床（trampoline）直接调，**不建解释器帧** |

普通函数（非类构造器、非 `new` 调用、形参不超过 3 个）编成 native 入口：参数放寄存器，int32 结果经第二个返回寄存器
不装箱地传回。每个函数有一个蹦床 `jmp [rip + cell]`（AArch64 上是 `ldr x16, cell; br x16`）：有机器码时指向机器码，否则指向解释器。

脚本 / eval 的顶层代码只跑一次，进入时不编，只编它里面的热循环（OSR）。

## build：字节码 ⇒ 推测图

`build.rs` 对字节码做**抽象解释**：为每个解释器寄存器记下它当前对应的 IR 值，同时依据内联缓存看到的东西推测。

- **数先当 int32**，带守卫；守卫失败会留下提示，下次编译那里用 double；
- **内联小函数**：已知的小函数调用直接内联（深度上限 4，函数体不超过 80 条指令，递归最多展开 2 层），
  去优化时连被内联的帧一起重建；别的调用走已编译代码或运行时；
- **对象先虚拟化**：这里新建的对象字段只记在编译器里，直到有谁需要对象本身才真正分配（逃逸分析）；
- **上下文槽**读写都留在值里，只在别人可能看到时才写回；
- **OSR 进来的循环剥一次皮**：一轮分配、下一轮就替换掉的东西根本不会存在。

字节码按区域（`Region`）走，每个区域内的块按顺序：前向边总是指向还没走过的块，所以一个块的状态就是它所有前驱的
合并；循环头对回边做推测（所有活跃值都有 phi），回边不一致就带着提示重新走（最多 12 次）。

`cfg.rs` 给出块、循环与活跃性：分析里寄存器编号为 receiver 和参数 `0 .. P`、r`i` 是 `P + i`、累加器是 `P + R`；
上下文寄存器总是活跃（不跟踪）。

## IR

`ir.rs`：SSA 形式的基本块，每条指令是一个值，带表示（`Rep`：int32、double、word……）。

```text
Block { phis…, insts…, terminator }      terminator 是 Jump / Branch / Return / Deopt
Inst  { op, rep }                        操作数是值的编号
FrameState { func, pc, slots…, parent }  守卫处每个解释器寄存器存着什么（虚拟对象也算）
```

几类典型的 `Op`：

| 类别 | 例子 |
|---|---|
| 常量与参数 | `I32`、`F64`、`Word`、`Param`、`Callee`、`Phi` |
| 算术 | `Int(op, a, b, fs)`（可能溢出时带帧状态）、`Float`、`IntToFloat` |
| 装箱 / 拆箱 | `BoxInt`、`BoxFloat`、`UnboxInt(v, fs)`（守卫：是 int32 且不是 −0）、`UnboxFloat` |
| 守卫 | `CheckObject`、`CheckShape`、`CheckFunc`（闭包跑的是某个函数）、`CheckNotHole` |
| 内存 | `LoadWord`、`LoadPtr`、`StoreCell`（必要时带写屏障）、`ContextCell` |
| 调用 | `Call(helper, …)`（运行时函数）、`CallJit`（调已编译代码） |

每个守卫带一个 `FrameState`：失败时解释器从哪条指令、以什么寄存器内容继续。

## opt 与 codegen

- `opt.rs`：只选一个值的 phi 去掉，没人用的指令去掉；
- 循环不变量外提（LICM）、跨循环的路径事实、常数取模变乘法等在构建与优化中完成；
- `codegen.rs`：**图直接到机器码**：按逆后序在块上做线性扫描寄存器分配，然后一遍指令选择。它按 x86-64 的寄存器角色
  （RAX … R15）和标志条件来写；`asm.rs` 按目标选编码器：`asm/x64.rs`（x86-64）或 `asm/a64.rs`（AArch64：每个角色映射到
  一个 AAPCS64 寄存器，在 NZCV 上模拟 x86 的标志条件，每次调用后把 `x0`/`x1` 搬进 RAX/RDX 角色）。建帧和调用者保存寄存器的
  进出栈是编码器的 `enter` / `leave` / `save` / `restore`。

```text
rbp 帧     −8 … −40 存 rbx r12 r13 r14 r15，−48 … −88 入口指针，下面是溢出槽
r15        始终是 JIT 上下文（Ctx）
临时       rax rcx rdx r11 xmm14 xmm15（不参与分配）
可分配     rsi rdi r8 r9 r10（调用者保存），rbx r12 r13 r14（被调者保存），xmm0 … xmm13
```

守卫跳到冷桩：把帧状态里的值存进 `Ctx::BUF`，调 `j8_deopt`。

## 运行时

`runtime.rs`：机器码调用和读取的东西。

```text
Ctx  +0 预算  +8 待处理  +16 全局对象  +24 脚本作用域  +32 Object.prototype  +40 返回值  +48 返回 pc  +56 acc
     +64 深度  +72 深度上限  +128 ARGS（16 个字）  +256 BUF（去优化的值）
```

- `j8_helper`：慢路运行时函数（属性访问、分配、各种运算的通用路径）；
- `j8_poll`：循环里定期检查预算、终止与中断请求；有请求时在检查点去优化、交给解释器处理，和解释器一样能被事件循环打断；
- `j8_deopt`：去优化入口；
- 可执行内存在 Unix 上用 `mmap` 分配，在 Windows 上用 `VirtualAlloc`。

## 去优化与学习

守卫失败时，`j8_deopt` 按帧状态重建解释器帧（包括内联掉的被调者），解释器接着跑。JIT 按失败原因留下提示：

| 原因 | 提示 |
|---|---|
| int32 推测失败 | 那里下次用 double（`HINT_FLOAT`） |
| 类型 / shape 不符 | 那里下次走通用路径（`HINT_GENERIC`） |
| 被调函数不是推测的那个 | 不内联 + 通用（`HINT_NO_INLINE`） |
| 遇到 JIT 不编的字节码 | 被内联的调用下次不内联；函数自己的代码就此放弃 |

失败的那份机器码作废，下次热起来再按提示重编。一个函数去优化 24 次后 JIT 放弃它，以后一直解释。

## MLIR 那条路

j8jit 还导出 `Compiler` / `Backend` / `MachineCode`（`compile.rs`、`x86_64.rs`）：MLIR（`llvm` 方言）经
[mlirpass](https://crates.io/crates/mlirpass)（`mem2reg`、`canonicalize`、`cse`）再由 j8jit 自己的 x86-64 后端出机器码，
不需要 LLVM。这是更早的第一档 JIT 走的路；现在热路径上用的是上面的图 ⇒ 机器码直出，V8 字节码到 MLIR 的下降正在重写。
