# 目录

[前言](README.md)

# 入门

- [j8 是什么](intro.md)
- [安装 dune](install.md)
- [快速开始](quickstart.md)
- [总体架构：一段 JS 的一生](architecture.md)

# 引擎内部

- [字节码：与 V8 Ignition 逐字节一致](bytecode.md)
- [编译器：JS ⇒ 字节码](compiler.md)
- [执行器：解释字节码](executor.md)
- [JIT：推测式优化层](jit.md)
- [框架层：Isolate、Scope 与值](framework.md)
- [对象模型：Shape、存储与属性](objects.md)
- [堆与 GC：建在 paragc 上](heap.md)
- [内建对象](builtins.md)
- [事件循环与任务队列](event-loop.md)
- [无锁基础件](lockfree.md)
- [平台](platform.md)

# 嵌入 j8

- [搭一个宿主](embedding.md)
- [绑定宏](bindings.md)
- [用宏开发自定义内容：迭代器与异步](custom-host.md)
- [异步与 Promise](async.md)
- [模块](modules.md)
- [Node-API](napi.md)
- [调试器](debugger.md)

# 运行时与样例

- [dune 与 dunx](dune.md)
- [workerd 与 jumpheap](workerd.md)
- [调试与基准工具](tools.md)

# 质量与路线

- [test262 与测试](testing.md)
- [性能](performance.md)
- [代码约定](conventions.md)
- [路线图](roadmap.md)

---

[术语表](glossary.md)
