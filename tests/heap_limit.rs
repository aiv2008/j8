use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use j8builtins::Builtins;
use j8executor::{EventLoop, EventLoopSet, Executor};
use j8framework::{Error, HeapLimitCallback, ScriptKind, Source, Value};
use j8platform::DefaultPlatform;

const LIMIT: usize = 32 << 20;
const GROW_IN_A_MODULE: &str = "(function () { const keep = []; for (let i = 0; i < 65536; i++) keep.push(new Array(1000).fill(0)) })()";
const GROW_IN_A_CALLBACK: &str = "Promise.resolve().then(() => { const keep = []; for (let i = 0; i < 65536; i++) keep.push(new Array(1000).fill(0)) })";
const GARBAGE_IN_ONE_MODULE: &str = "let n = 0; for (let i = 0; i < 20000; i++) n += new Array(1000).fill(0).length; globalThis.made = n;";
const GARBAGE: &str = "(function () { let n = 0; for (let i = 0; i < 500; i++) n += new Array(1000).fill(0).length })()";

fn limited<R>(jit: bool, callback: Option<HeapLimitCallback<'static>>, f: impl FnOnce(&EventLoop<'_>) -> R) -> R {
    let platform = DefaultPlatform::default();
    EventLoopSet::scope(&platform, NonZeroUsize::MIN, |set| {
        let executor = if jit { Executor::default().with_tier(j8jit::Jit::from(0)) } else { Executor::default() };
        let js = set.spawn(executor, &Builtins);
        js.set_heap_limit(NonZeroUsize::new(LIMIT), callback);
        f(&js)
    })
}

fn run(js: &EventLoop<'_>, text: &str, kind: ScriptKind) -> Result<(), Error> {
    js.eval(&Source { text, file: "main.js" }, kind).and_then(|_| js.run())
}

fn collections(js: &EventLoop<'_>) -> u64 {
    js.enter(|a| a.mc.generation())
}

fn stops(text: &str, jit: bool) {
    limited(jit, None, |js| {
        let r = run(js, text, ScriptKind::Module);
        assert!(matches!(r, Err(Error::Terminated)), "{:?}", r.as_ref().err());
        assert!(js.heap_used() < LIMIT, "the garbage left behind is collected: {}", js.heap_used());
        assert!(run(js, "globalThis.after = new Array(1000).fill(1).length", ScriptKind::Classic).is_ok(), "the loop stays usable");
    });
}

#[test]
fn growing_in_a_module_stops() {
    stops(GROW_IN_A_MODULE, false);
}

#[test]
fn growing_in_a_module_stops_compiled() {
    stops(GROW_IN_A_MODULE, true);
}

#[test]
fn growing_in_a_promise_callback_stops() {
    stops(GROW_IN_A_CALLBACK, false);
}

#[test]
fn garbage_inside_one_module_is_collected_before_the_limit_stops_it() {
    for jit in [false, true] {
        limited(jit, None, |js| {
            let r = run(js, GARBAGE_IN_ONE_MODULE, ScriptKind::Module);
            assert!(r.is_ok(), "jit {jit}: {:?}", r.err());
            assert!(js.heap_used() < LIMIT, "jit {jit}: {}", js.heap_used());
        });
    }
}

#[test]
fn garbage_between_tasks_does_not_count() {
    limited(false, None, |js| {
        for _ in 0..100 {
            let r = run(js, GARBAGE, ScriptKind::Classic);
            assert!(r.is_ok(), "{:?}", r.err());
        }
    });
}

#[test]
fn a_steady_heap_is_not_collected_every_turn() {
    limited(false, None, |js| {
        assert!(run(js, "globalThis.keep = []", ScriptKind::Classic).is_ok());
        while js.heap_used() < LIMIT / 10 * 6 {
            let r = run(js, "for (let i = 0; i < 50; i++) keep.push(new Array(1000).fill(0))", ScriptKind::Classic);
            assert!(r.is_ok(), "{:?}", r.err());
        }
        let before = collections(js);
        for _ in 0..50 {
            let r = run(js, "globalThis.n = new Array(100).fill(0).length", ScriptKind::Classic);
            assert!(r.is_ok(), "{:?}", r.err());
        }
        let collected = collections(js) - before;
        assert!(collected < 10, "{collected} collections in 50 small turns");
    });
}

#[test]
fn garbage_made_outside_js_does_not_stop_the_next_task() {
    limited(false, None, |js| {
        js.enter(|a| {
            while a.mc.gc_scope().used() <= LIMIT {
                let _ = a.array((0..1000).map(|_| Value::Number(0.0)).collect());
            }
        });
        assert!(run(js, "globalThis.after = 1", ScriptKind::Classic).is_ok(), "the next task runs");
    });
}

#[test]
fn the_callback_raises_the_limit_and_it_goes_on() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&calls);
    let callback: HeapLimitCallback<'static> = Box::new(move |current, initial| {
        let mut calls = seen.lock().unwrap();
        calls.push((current, initial));
        if calls.len() == 1 { current * 2 } else { current }
    });
    limited(false, Some(callback), |js| {
        let r = run(js, "globalThis.keep = []; for (let i = 0; i < 65536; i++) keep.push(new Array(1000).fill(0))", ScriptKind::Module);
        assert!(matches!(r, Err(Error::Terminated)), "{:?}", r.as_ref().err());
        assert!(js.heap_used() > LIMIT * 2, "it ran on past the first limit: {}", js.heap_used());
    });
    assert_eq!(*calls.lock().unwrap(), vec![(LIMIT, LIMIT), (LIMIT * 2, LIMIT)]);
}
