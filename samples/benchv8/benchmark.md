# j8 vs. V8 performance

The same set of JS benchmarks (`bench/*.js`) runs on j8, V8 `--jitless` (interpreter only) and V8 (with JIT). V8 is embedded via rusty_v8 `=152.2.0`.

## Running

```text
cd samples/benchv8
cargo run --release                     # run all three engines once each and print the comparison table
cargo run --release -- --engine j8      # run only one: j8 / v8 / v8-jitless
cargo run --release -- --runs 10        # runs per benchmark, the shortest is kept (default 5)
```

- V8's `--jitless` is a process-wide flag, so each engine runs in its own child process.
- Timing covers compile + execute only; each run uses a fresh execution environment, and the shortest run is kept.
- Each script's completion value is a checksum that must match across the three engines (the check column in the table).
- benchv8 is a workspace of its own (excluded from the root workspace), so a normal `cargo build --workspace` doesn't pull V8's prebuilt library.

## Results

j8 runs with its JIT (`j8::jit::Jit`; `J8_NOJIT=1` runs the interpreter only), built from j8 `8899f14` with nightly
(explicit tail calls in the interpreter) as a path dependency. `--runs 10`, pinned to the performance cores
(`taskset -c 0-3`). The load average was 6–8 (other builds running), so rows of a few milliseconds move by about ±10 %.
All 11 benchmarks produce matching checksums on all three engines.

| Benchmark | j8 ms | V8 jitless ms | V8 ms | j8 / jitless | j8 / V8 |
|---|---:|---:|---:|---:|---:|
| fib | 1.2 | 9.5 | 1.4 | 0.1× | 0.9× |
| loop | 57.2 | 202.8 | 83.4 | 0.3× | 0.7× |
| objects | 0.9 | 17.2 | 2.7 | 0.1× | 0.3× |
| array | 12.2 | 16.4 | 7.8 | 0.7× | 1.6× |
| sort | 34.6 | 39.3 | 19.9 | 0.9× | 1.7× |
| string | 23.3 | 12.2 | 12.2 | 1.9× | 1.9× |
| closures | 2.6 | 21.6 | 2.7 | 0.1× | 1.0× |
| classes | 1.5 | 29.3 | 3.5 | 0.1× | 0.4× |
| collections | 29.1 | 33.4 | 25.7 | 0.9× | 1.1× |
| json | 21.6 | 14.3 | 12.7 | 1.5× | 1.7× |
| regexp | 5.6 | 3.6 | 2.1 | 1.6× | 2.7× |
| **Geometric mean** | | | | **0.4×** | **1.1×** |

The executor's call-stack cleanup (#98, #101) left j8's own times unchanged: geometric mean −0.5 % for #98
(`18c8a57` ⇒ `b1a2e53`) and +0.5 % for #101 (`79383c2` ⇒ `fe1fcff`), both within the noise above.

## What the JIT does for them

- **fib**: a native function per JS function, called directly (arguments in registers, results unboxed through a second
  return register), the recursive call inlined two levels deep; the global `fib` is read through an epoch cache
- **closures**: the closure is inlined into the loop (checked by function identity), its context slots live in registers,
  `% 100000` is a multiplication
- **classes / objects**: constructors and methods inlined, objects that don't escape are never allocated (fields as values,
  also across loop iterations: the loop entered on-stack is peeled once), shape checks and loads moved out of the loop
- **loop**: `(s + i * i) % 1000003` with `i * i` past int32 runs as an int64 remainder of doubles
- array, sort, string, collections, json and regexp spend their time in builtins (and the GC); the JIT doesn't help them yet
