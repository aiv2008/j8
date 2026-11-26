# Roadmap

j8's main line is the test262 pass rate (`PLAN.md`). Every phase: add cases to the semantics table → clippy clean → run
the baseline in a snapshot with zero regressions → record the newly passing cases → commit.

**Out of scope**: Temporal, ShadowRealm, explicit resource management (`using` / `DisposableStack`), import
defer, decorators, source phase imports.

## Done

| Phase | Contents |
|---|---|
| 1 Infrastructure | dynamic code (`eval`, `Function`, generator / async constructors), early errors, strict mode, global declaration checks, `$262` |
| 2 Object / Array / property model | the spec's internal methods, dense arrays with holes, `Object` completed, Array methods generic over array-likes, `length` of built-ins, prototypes from `new.target` |
| 3 Reflection and iterators | Reflect, Proxy (revocable too), iterator helpers, Set methods, `Map.groupBy` |
| 4 Binary data | ArrayBuffer (resizable, transfer, detach), SharedArrayBuffer, DataView, 12 TypedArrays (Float16 included), Atomics |
| 5 Date / RegExp / strings and numbers | Date (local time as UTC without `intl`), RegExp's Symbol protocol and `d` flag, String completed, exact number formatting, `Math.sumPrecise`, Promise rewritten on NewPromiseCapability |
| 6 The dynamic parts of the language | direct eval, `with`, Annex B block-level function hoisting, generator parameter initialization, `arguments` linked to parameters, modules and dynamic `import()` |
| 7 Intl (`intl` feature) | locales and `supportedLocalesOf`, `Collator`, `NumberFormat`, `DateTimeFormat`, `PluralRules`, the `toLocale*` methods, Date's local time zone from the platform |

## Still to do

- import defer / source phase;
- the conflict rule for `var arguments` declared by eval in parameter expressions;
- Annex B block-level functions inside eval code;
- string storage that can represent lone surrogates (UTF-8 for now);
- real weak references for `WeakRef` / `FinalizationRegistry` (strong for now);
- the rest of Intl: `RelativeTimeFormat`, `formatRange`, CLDR currency / unit / percent patterns, `Locale`,
  `DisplayNames`, `ListFormat`, `Segmenter`.

## JIT

The plan is to **skip a baseline compiler and go straight to a speculative optimizing tier**: inlining, escape
analysis, deoptimization and OSR are in; next, make the built-in-heavy benchmarks (array, sort, string, collections,
json, regexp) benefit, aiming to beat V8 with its JIT on common benchmarks.

## Runtimes

- dune: top-level `await`, `https:`, `http` / `net` servers, streams, async child processes, `worker_threads`, `zlib`,
  `crypto` hashes and more; publishing the npm package `@jabberwocky238/dune`;
- Node-API: real weak references.
