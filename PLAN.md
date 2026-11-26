# j8 plan: complete core ECMAScript for test262

The main line is raising the test262 pass rate. Each phase: add cases to the semantics table → clippy clean → run the test262 baseline in a snapshot with zero regressions → record newly passing cases in the relevant directories with `J8_TEST262_UPDATE=1` → commit and push.

Out of scope: Temporal, ShadowRealm, explicit resource management (using / DisposableStack), import defer, decorators, source phase imports.

## Phase 1: infrastructure

- [x] Dynamic code: `Invoke::eval_code`, global `Function` / `eval`, GeneratorFunction / AsyncFunction / AsyncGeneratorFunction constructors
- [x] Early errors: oxc_semantic checks, regex literals validated at parse time
- [x] Strict mode: `this` wrapping, failed assignment / delete, assignment to undeclared names, `arguments.callee`; direct eval in strict code
- [x] Global declarations: top-level `var` / functions check before creating (CanDeclareGlobalVar / Function), `undefined` / `NaN` / `Infinity` value properties
- [x] `$262`: `global` / `evalScript` / `gc` (`createRealm` / `detachArrayBuffer` throw TypeError; phase 4 wires up the latter)

## Phase 2: Object / Array / property model

- [x] Object internal methods (j8framework property.rs): [[GetOwnProperty]] / [[DefineOwnProperty]] (ValidateAndApplyPropertyDescriptor, ArraySetLength, String wrapper objects) / [[OwnPropertyKeys]] / extensibility / prototype; dense array part + ordinary properties stored by index (holes, elements with different attributes)
- [x] `extensible`: preventExtensions / isExtensible / seal / isSealed / isFrozen
- [x] Object completion: defineProperties, getOwnPropertySymbols / Descriptors, setPrototypeOf, fromEntries, is, hasOwn, groupBy, propertyIsEnumerable, isPrototypeOf, `__proto__`, `__defineGetter__` etc.; literal `__proto__: v`
- [x] Array methods generic over array-likes; add from / splice / copyWithin / flat / flatMap / findLast(Index) / reduceRight / toReversed / toSorted / toSpliced / with / unscopables / species
- [x] `length` of built-in functions; host functions split into constructible or not, constructors pick the prototype from new.target via `Agent::new_target`; `[Symbol.toStringTag]`

## Phase 3: reflection and iterators

- [x] Reflect
- [x] Proxy (including revocable): j8framework proxy.rs, internal-method entry points and call / construct dispatch to traps with invariant checks
- [x] Iterator helpers (global Iterator, Iterator.from, map / filter / take / drop / flatMap / reduce / toArray / forEach / some / every / find)
- [x] Set methods, Map.groupBy

## Phase 4: binary data

- [x] ArrayBuffer (resizable, transfer, detach) / SharedArrayBuffer (growable) / DataView / TypedArray (12 kinds, including Float16) / Atomics (single-threaded semantics; multi-agent cases can't run); `$262.detachArrayBuffer`

## Phase 5: Date / RegExp / strings and numbers

- [x] Date (local time zone from the platform with the `intl` feature, UTC otherwise)
- [x] RegExp Symbol protocol, RegExpExec, generic flags getter, lastIndex, `d`, species, compile, escape
- [x] String completion (fromCodePoint / raw / normalize / annexB etc.; strings are still stored as UTF-8, so lone surrogates are lost)
- [x] Number formatting (toFixed / toPrecision / toExponential with exact decimals), Math completion (including sumPrecise / f16round)
- [x] Symbol.for, EvalError / URIError / AggregateError, cause, URI functions, Promise rewritten on NewPromiseCapability (subclasses, allSettled / any / withResolvers / try), WeakRef / FinalizationRegistry (strong references)

## Phase 6: dynamic parts of the language

- [x] Direct eval: every binding visible at the call site goes into the environment, CallEval carries a scope table, eval code runs chained onto the call site's environment; non-strict `var` / functions land in the calling function (reusing an existing binding of the same name, otherwise going into an eval var object looked up like `with`); lexical conflicts raise SyntaxError; annexB block-level functions inside eval (the conflict rule for `var arguments` declared by eval in parameter expressions is not done yet)
- [x] `with` (names in the body look up the with object first, honoring `Symbol.unscopables`, also visible to inner functions; the `this` of calls is the object)
- [x] annexB block-level function hoisting (functions and scripts, including function declarations in if clauses; not yet in eval code)
- [x] Generators / async generators initialize parameters at call time (errors throw synchronously)
- [x] arguments linked to parameters (non-strict, simple parameters: parameters go into the environment, indices map to slots, the link breaks after delete / redefining as accessor / making read-only)
- [x] Modules and dynamic import: module environment, import bindings read the exporter's slots live, ResolveExport (indirect / star / ambiguous), namespace objects, import(), import.meta, `Platform::load_module`; the runner runs harness scripts first, then the module; top-level await follows the cyclic module record evaluation (import defer / source phase not yet)

## Phase 7: Intl (the `intl` feature)

- [x] Locales: `Intl.getCanonicalLocales`, `supportedLocalesOf`, ResolveLocale over ICU4X's likely-subtags data, `Intl.supportedValuesOf`; the default locale and time zone come from `Platform::locale` / `Platform::time_zone`
- [x] `Intl.Collator`, `String.prototype.localeCompare`, `toLocaleUpperCase` / `toLocaleLowerCase`
- [x] `Intl.NumberFormat` (rounding, significant / fraction digits, scientific / engineering / compact notation, sign display, `formatToParts`), `Number` / `BigInt` `toLocaleString`
- [x] `Intl.DateTimeFormat` (components, `dateStyle` / `timeStyle`, IANA time zones and offsets, `formatToParts`), `Date` `toLocale*String`, Date's local time zone
- [x] `Intl.PluralRules` (`select`, `selectRange`)
- [x] `Intl.ListFormat`, `Intl.Segmenter` (grapheme / word / sentence, `containing`, segment iterators), `Intl.Locale` (options, getters, `maximize` / `minimize`, `getCalendars` / `getCollations` / `getHourCycles` / `getNumberingSystems` / `getTimeZones` / `getTextInfo` / `getWeekInfo`); ICU4X cannot hold 5–8 letter language subtags
- [x] `Intl.RelativeTimeFormat`, `Intl.DisplayNames` (language / region / script names from icu_experimental; currency, calendar and dateTimeField fall back to the code)
- [x] `Intl.DurationFormat` (PartitionDurationFormatPattern over NumberFormat + ListFormat), CLDR unit patterns for the duration / length / mass / volume / area units ICU4X ships
- [ ] `formatRange` / `formatRangeToParts`, CLDR currency / percent patterns and the remaining units, the legacy `Intl.FallbackSymbol`
