# test262 and testing

## test262

`tests/test262` is the [tc39/test262](https://github.com/tc39/test262) submodule (a shallow clone; fetch it with
`git submodule update --init`), and the runner `tests/test262.rs` is a test of the root package (`harness = false`, not
published).

```sh
cargo test --release --test test262                                                   # only the baseline; fails on any loss
cargo test --release --test test262 -- language/expressions/addition built-ins/Math   # these directories
J8_TEST262_JIT=1 cargo test --release --test test262                                  # with the JIT (every function compiled on its first call)
J8_TEST262_VERBOSE=1 cargo test --release --test test262 -- <path>                    # why each failing case failed
J8_TEST262_UPDATE=1 cargo test --release --test test262 -- <path>                     # write the results into the baseline
```

- one child process per case, 10 s timeout; results are pass / fail / unsupported (j8 reported `Error::Unsupported`) /
  skip / timeout / crash, summarized by directory;
- the baseline `tests/test262.passing` lists passing cases one per line, **42,500** now; without arguments only those run,
  as the regression check;
- sloppy mode plus `onlyStrict` (run strict); module cases run the harness scripts first, then the module;
- the `$262` host object provides `global` / `evalScript` / `gc` / `detachArrayBuffer` (`createRealm` throws TypeError);
- line numbers in errors from unsupported syntax include the prepended harness.

**Zero regressions on every commit** is a hard rule, and the same baseline must pass with the JIT on (threshold 0, every
function compiled on its first call, for the widest coverage).

## Other tests

| Test | Covers |
|---|---|
| `tests/runtime.rs`, `tests/event_loop.rs`, `tests/terminate.rs`, `tests/interrupt.rs`, `tests/heap_limit.rs` | runtimes, event loops, termination, interrupts, the heap limit |
| `tests/binding.rs`, `tests/bindings_more.rs` | the binding macros |
| `crates/j8bytecode/tests/v8.rs` | decodes 738 instructions V8 printed, checking each |
| `javascript_semantics` in `crates/j8executor` | a table of exactly determined JS semantics |
| `crates/j8jit/tests`, `j8taskqueue/tests`, `j8lockfree/tests`, `j8napi/tests` | each crate's own |
| `samples/workerd/tests` | the chat room stress test, a thousand Workers |
| `samples/jumpheap/tests/three.rs` | matrices across three processes' heaps |

## Adding a feature

Each [roadmap](roadmap.md) phase goes:

1. add cases to the semantics table;
2. clippy clean;
3. run the test262 baseline in a snapshot, zero regressions;
4. record newly passing cases of the relevant directories with `J8_TEST262_UPDATE=1`;
5. commit and push (j8 uses branches and pull requests on GitHub).

Unit tests only pin facts that are certain; run one at a time, under `timeout 15`.
