use std::num::NonZeroUsize;
use std::sync::mpsc;
use std::time::Duration;

use j8executor::{EventLoopSet, Executor};
use j8framework::{Scope, Value};
use j8platform::DefaultPlatform;

const WAIT: Duration = Duration::from_secs(10);
const SPIN: Duration = Duration::from_millis(100);
const CALLBACK: &str = "globalThis.stop = false;\nglobalThis.turns = 0;\nlater.now().then(() => { while (!globalThis.stop) globalThis.turns++; });\n";

struct Later;

#[j8::object("later")]
impl Later {
    #[allow(clippy::unused_async, reason = "the callback under test runs when this settles")]
    async fn now() {}
}

j8::runtime! {
    Host {
        globals: [Later],
    }
}

fn read(a: &mut Scope<'_, '_>, name: &str) -> Option<f64> {
    let global = Value::Object(a.context.global);
    a.get_named(global, name).ok()?.as_number()
}

#[test]
fn an_interrupt_runs_while_the_event_loop_is_idle() {
    let platform = DefaultPlatform::default();
    EventLoopSet::scope(&platform, NonZeroUsize::MIN, |set| {
        let rt = j8::Runtime::on::<Host>(set, Executor::default()).expect("runtime");
        rt.eval("globalThis.seen = 41;", "idle.js").expect("runs");
        rt.run_until_idle().expect("idle");
        let (tx, rx) = mpsc::channel();
        rt.interrupter().interrupt(move |_, a| _ = tx.send(read(a, "seen")));
        assert_eq!(rx.recv_timeout(WAIT), Ok(Some(41.0)), "ran on the idle loop without anything else to run");
    });
}

fn interrupts_a_busy_promise_callback(jit: bool) {
    let platform = DefaultPlatform::default();
    EventLoopSet::scope(&platform, NonZeroUsize::MIN, |set| {
        let executor = if jit { Executor::default().with_tier(j8jit::Jit::from(0)) } else { Executor::default() };
        let rt = j8::Runtime::on::<Host>(set, executor).expect("runtime");
        let interrupter = rt.interrupter();
        let (tx, rx) = mpsc::channel();
        std::thread::scope(|s| {
            s.spawn(move || {
                std::thread::sleep(SPIN);
                interrupter.interrupt(move |_, a| {
                    let turns = read(a, "turns");
                    let global = Value::Object(a.context.global);
                    a.set_named(global, "stop", true.into()).expect("stop is set");
                    _ = tx.send(turns);
                });
            });
            rt.eval(CALLBACK, "spin.js").expect("runs");
            rt.run_until_idle().expect("the callback ends");
        });
        let turns = rx.recv_timeout(WAIT).expect("the interrupt ran").expect("a count");
        assert!(turns > 0.0, "the callback was spinning when the interrupt ran");
        let after = rt.eval("turns >= 0 && stop", "after.js").expect("runs");
        assert_eq!(rt.text(&after), "true");
    });
}

#[test]
fn an_interrupt_runs_inside_a_busy_promise_callback() {
    interrupts_a_busy_promise_callback(false);
}

#[test]
fn an_interrupt_runs_inside_jit_code() {
    interrupts_a_busy_promise_callback(true);
}
