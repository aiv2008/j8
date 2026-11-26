use std::num::NonZeroUsize;

use j8builtins::Builtins;
use j8executor::Executor;
use j8framework::{CreateParams, Isolate, Source};
use j8platform::DefaultPlatform;

const THRESHOLD: NonZeroUsize = NonZeroUsize::new(256 << 10).unwrap();
const MIN_COLLECTIONS: u64 = 3;

const MODULE: &str = "
let head = null;
for (let i = 0; i < 200000; i++) {
  const garbage = { a: [i, i + 1], s: 'x' + i };
  if (garbage.a[1] !== i + 1) throw new Error('garbage changed');
  if (i % 100 === 0) head = { v: i, next: head, tag: 'n' + i };
}
let sum = 0, n = 0;
for (let p = head; p; p = p.next) {
  if (p.tag !== 'n' + p.v) throw new Error('bad node ' + p.v);
  sum += p.v;
  n++;
}
globalThis.result = n + ':' + sum;
";

const EXPECTED: &str = "2000:199900000";

const SCRIPT: &str = "
let sink = [];
function render(n) {
  const kids = [];
  for (let i = 0; i < 50; i++) kids.push({ type: 'div', props: { key: n + ':' + i, children: [String(i)] } });
  return kids.map((k) => ({ ...k, rendered: true }));
}
let total = 0;
for (let r = 0; r < 4000; r++) { sink = render(r); total += sink.length; }
var result = total + ':' + sink[49].props.key;
";

const SCRIPT_EXPECTED: &str = "200000:3999:49";

fn run(jit: bool) -> (String, u64) {
    let platform = DefaultPlatform::default();
    let mut isolate = Isolate::new(&platform, CreateParams { collect_threshold: Some(THRESHOLD), ..CreateParams::default() });
    let executor = if jit { Executor::default().with_tier(j8jit::Jit::default()) } else { Executor::default() };
    let cx = isolate.create_context(&Builtins);
    let before = isolate.gc_cycles();
    executor.eval_module(&mut isolate, &cx, &Source { text: MODULE, file: "gc_safepoint.mjs" }).expect("the module runs");
    let collections = isolate.gc_cycles() - before;
    let result = executor.eval(&mut isolate, &cx, &Source { text: "result", file: "result.js" }).expect("the result");
    let text = isolate.enter(&cx, &executor, |a| {
        let v = a.local(&result);
        String::from(&**a.to_string(v).expect("ToString"))
    });
    (text, collections)
}

#[test]
fn a_module_body_is_collected_while_it_runs() {
    for jit in [false, true] {
        let (result, collections) = run(jit);
        assert_eq!(result, EXPECTED, "jit {jit}");
        assert!(collections >= MIN_COLLECTIONS, "jit {jit}: only {collections} collections, so none ran inside the module body");
    }
}

fn run_script(jit: bool) -> String {
    let platform = DefaultPlatform::default();
    let mut isolate = Isolate::new(&platform, CreateParams { collect_threshold: Some(THRESHOLD), ..CreateParams::default() });
    let executor = if jit { Executor::default().with_tier(j8jit::Jit::default()) } else { Executor::default() };
    let cx = isolate.create_context(&Builtins);
    executor.eval(&mut isolate, &cx, &Source { text: SCRIPT, file: "gc_safepoint.js" }).expect("the script runs");
    let result = executor.eval(&mut isolate, &cx, &Source { text: "result", file: "result.js" }).expect("the result");
    isolate.enter(&cx, &executor, |a| {
        let v = a.local(&result);
        String::from(&**a.to_string(v).expect("ToString"))
    })
}

#[test]
fn a_script_survives_collections_while_it_runs() {
    for jit in [false, true] {
        assert_eq!(run_script(jit), SCRIPT_EXPECTED, "jit {jit}");
    }
}

const MAP_MODULE: &str = "
const rows = Array.from({ length: 20000 }, (_, i) => i);
const out = rows.map((r) => {
  const kids = [];
  for (let i = 0; i < 20; i++) kids.push({ key: r + ':' + i, children: [String(i)] });
  return kids[19].key;
});
globalThis.result = out.length + ':' + out[19999] + ':' + out.filter((k, i) => k === i + ':19').length;
";

const MAP_EXPECTED: &str = "20000:19999:19:20000";

#[test]
fn a_map_callback_is_collected_while_it_runs() {
    for jit in [false, true] {
        let platform = DefaultPlatform::default();
        let mut isolate = Isolate::new(&platform, CreateParams { collect_threshold: Some(THRESHOLD), ..CreateParams::default() });
        let executor = if jit { Executor::default().with_tier(j8jit::Jit::default()) } else { Executor::default() };
        let cx = isolate.create_context(&Builtins);
        let before = isolate.gc_cycles();
        executor.eval_module(&mut isolate, &cx, &Source { text: MAP_MODULE, file: "map.mjs" }).expect("the module runs");
        let collections = isolate.gc_cycles() - before;
        let result = executor.eval(&mut isolate, &cx, &Source { text: "result", file: "result.js" }).expect("the result");
        let text = isolate.enter(&cx, &executor, |a| {
            let v = a.local(&result);
            String::from(&**a.to_string(v).expect("ToString"))
        });
        assert_eq!(text, MAP_EXPECTED, "jit {jit}");
        if !jit {
            assert!(collections >= MIN_COLLECTIONS, "only {collections} collections, so none ran inside the callbacks");
        }
    }
}
