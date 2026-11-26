use std::time::Duration;

use j8builtins::Builtins;
use j8executor::Executor;
use std::num::NonZeroUsize;

use j8executor::EventLoopSet;
use j8framework::{CreateParams, Error, Isolate, ScriptKind, Source};
use j8platform::DefaultPlatform;

const SPIN: Duration = Duration::from_millis(50);

#[test]
fn terminate_from_another_thread() {
    let platform = DefaultPlatform::default();
    let mut isolate = Isolate::new(&platform, CreateParams::default());
    let executor = Executor::default().with_tier(j8jit::Jit::from(0));
    let cx = isolate.create_context(&Builtins);
    let handle = isolate.handle();
    let text = "function spin() { let n = 0; for (;;) n++; } try { spin(); } catch (e) { globalThis.caught = 1; } finally { globalThis.fin = 1; }";
    let r = std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(SPIN);
            handle.terminate();
        });
        executor.eval(&mut isolate, &cx, &Source { text, file: "spin.js" })
    });
    assert!(matches!(r, Err(Error::Terminated)), "{:?}", r.as_ref().err());
    assert!(!handle.is_terminating(), "reset when leaving the outermost entry");
    let after = executor.eval(&mut isolate, &cx, &Source { text: "`${typeof caught} ${typeof fin} ${1 + 1}`", file: "after.js" }).expect("the isolate stays usable");
    let text = isolate.enter(&cx, &executor, |a| {
        let v = a.local(&after);
        String::from(&**a.to_string(v).expect("ToString"))
    });
    assert_eq!(text, "undefined undefined 2");
}

const SPINS: [(&str, &str); 4] = [
    ("top level", "for (;;) {}"),
    ("in a function", "function spin() { for (;;) {} } spin()"),
    ("after an await", "async function spin() { await null; for (;;) {} } spin()"),
    ("in a promise callback", "Promise.resolve().then(() => { for (;;) {} })"),
];

fn terminate_on_an_event_loop(jit: bool) {
    let platform = DefaultPlatform::default();
    for (what, text) in SPINS {
        EventLoopSet::scope(&platform, NonZeroUsize::MIN, |set| {
            let executor = if jit { Executor::default().with_tier(j8jit::Jit::from(0)) } else { Executor::default() };
            let js = set.spawn(executor, &Builtins);
            let handle = js.handle();
            let r = std::thread::scope(|s| {
                s.spawn(|| {
                    std::thread::sleep(SPIN);
                    handle.terminate();
                });
                js.eval(&Source { text, file: "spin.js" }, ScriptKind::Module).and_then(|_| js.run())
            });
            assert!(matches!(r, Err(Error::Terminated)), "{what}: {:?}", r.as_ref().err());
            assert!(!handle.is_terminating(), "{what}: reset when the loop gives up the turn");
            let after = js.eval(&Source { text: "1 + 1", file: "after.js" }, ScriptKind::Classic);
            assert!(after.is_ok(), "{what}: the loop stays usable");
        });
    }
}

#[test]
fn terminate_on_an_event_loop_interpreted() {
    terminate_on_an_event_loop(false);
}

#[test]
fn terminate_on_an_event_loop_compiled() {
    terminate_on_an_event_loop(true);
}
