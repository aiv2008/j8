# 路线图

j8 的主线是 test262 通过率（`PLAN.md`）。每一阶段：往语义表加用例 → clippy 干净 → 在快照里跑基线零回归 → 记下新通过的用例 → 提交。

**范围外**：Temporal、ShadowRealm、显式资源管理（`using` / `DisposableStack`）、import defer、装饰器、source phase imports。

## 已完成

| 阶段 | 内容 |
|---|---|
| 1 基础设施 | 动态代码（`eval`、`Function`、生成器 / async 构造器）、早期错误、严格模式、全局声明检查、`$262` |
| 2 Object / Array / 属性模型 | 规范的内部方法、数组的稠密部分与空洞、`Object` 补全、Array 方法对类数组通用、内建函数的 `length`、按 `new.target` 定原型 |
| 3 反射与迭代器 | Reflect、Proxy（含 revocable）、迭代器助手、Set 方法、`Map.groupBy` |
| 4 二进制数据 | ArrayBuffer（可调大小、transfer、detach）、SharedArrayBuffer、DataView、12 种 TypedArray（含 Float16）、Atomics |
| 5 Date / RegExp / 字符串与数 | Date（不开 `intl` 时本地时区按 UTC）、RegExp 的 Symbol 协议与 `d` 标志、String 补全、精确的数格式化、`Math.sumPrecise`、Promise 重写在 NewPromiseCapability 上 |
| 6 语言的动态部分 | 直接 eval、`with`、annexB 块级函数提升、生成器参数初始化、`arguments` 与形参联动、模块与动态 `import()` |
| 7 Intl（`intl` feature） | 语言区域与 `supportedLocalesOf`、`Collator`、`NumberFormat`、`DateTimeFormat`、`PluralRules`、`toLocale*` 方法、Date 按平台给的本地时区 |

## 还差的

- import defer / source phase；
- 形参表达式里 eval 声明 `var arguments` 的冲突规则；
- eval 代码里的 annexB 块级函数；
- 字符串改为能表示落单代理项的存储（当前 UTF-8）；
- `WeakRef` / `FinalizationRegistry` 的真弱引用（当前强引用）；
- Intl 的其余部分：`RelativeTimeFormat`、`formatRange`、CLDR 的货币 / 单位 / 百分比格式、`Locale`、
  `DisplayNames`、`ListFormat`、`Segmenter`。

## JIT

路线是**跳过基线编译器，直接做推测式优化层**：内联、逃逸分析、去优化、OSR 已经落地；接下来是让内建函数密集的基准
（array、sort、string、collections、json、regexp）受益，目标是在常用基准上超过带 JIT 的 V8。

## 运行时

- dune：补齐顶层 `await`、`https:`、`http` / `net` 服务端、流、异步子进程、`worker_threads`、`zlib`、`crypto` 哈希等；
  npm 包 `@jabberwocky238/dune` 发布；
- Node-API：真弱引用。
