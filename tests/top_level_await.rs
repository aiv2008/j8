use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use j8::frame::{JsObject, Platform, Pool, Scope, ScopeGlobal, Settled, Task, Value, ValueBrand};
use j8executor::Executor;
use j8platform::DefaultPlatform;

struct Later;

#[j8::object("later")]
impl Later {
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl, reason = "the binding settles on the event loop, after the module has suspended")]
    async fn echo(x: f64) -> f64 {
        x
    }
}

j8::runtime! {
    Waiting {
        globals: [Later],
    }
}

#[derive(Default)]
struct Sources(DefaultPlatform, AtomicUsize);

impl Sources {
    const DEPENDENCY: &str = "globalThis.order = ['dependency starts']; export const value = await later.echo(20); order.push('dependency done');";
    const FAILING: &str = "await later.echo(0); throw new Error('dependency failed');";
}

impl Pool for Sources {
    fn threads(&self) -> usize {
        self.0.threads()
    }

    fn run(&self, jobs: Vec<Task<'_>>) {
        self.0.run(jobs);
    }
}

impl Platform for Sources {
    fn now(&self) -> Duration {
        self.0.now()
    }

    fn load_module(&self, _: &str, specifier: &str) -> Option<(String, String)> {
        match specifier {
            "./dependency.mjs" => Some((specifier.to_owned(), Self::DEPENDENCY.to_owned())),
            "./failing.mjs" => Some((specifier.to_owned(), Self::FAILING.to_owned())),
            _ => None,
        }
    }

    fn print(&self, line: &str) {
        self.0.print(line);
    }

    fn unhandled_rejection<'scope>(&self, _: &mut Scope<'scope, '_>, _: JsObject<'scope>, _: Value<'scope>) {
        self.1.fetch_add(1, Ordering::SeqCst);
    }
}

fn state(rt: &j8::Runtime<'_>, evaluation: &ScopeGlobal<'_, ValueBrand>) -> String {
    rt.enter(|a| {
        match Scope::promise_state(a.local(evaluation)) {
            None => String::from("not a promise"),
            Some(Settled::Pending) => String::from("pending"),
            Some(Settled::Fulfilled(v)) => format!("fulfilled {}", text(a, v)),
            Some(Settled::Rejected(v)) => format!("rejected {}", text(a, v)),
        }
    })
}

fn text<'scope>(a: &mut Scope<'scope, '_>, v: j8::frame::Value<'scope>) -> String {
    a.to_string(v).map_or_else(|_| String::from("<ToString threw>"), |s| String::from(&**s))
}

#[test]
fn module_evaluation_waits_for_top_level_await() {
    let platform = Waiting::platform(Sources::default());
    j8::EventLoopSet::scope(&platform, std::num::NonZeroUsize::MIN, |set| {
        let rt = Waiting::runtime(set, Executor::default()).expect("runtime");
        let src = "import { value } from './dependency.mjs'; order.push('main starts'); globalThis.result = value + await later.echo(22); order.push('main done');";
        let evaluation = rt.eval_module(src, "main.mjs").expect("module starts");
        rt.run_until_idle().expect("loop runs");
        assert_eq!(state(&rt, &evaluation), "fulfilled undefined");
        let r = rt.eval("order.join('|') + '|' + result", "check.js").expect("script runs");
        assert_eq!(rt.text(&r), "dependency starts|dependency done|main starts|main done|42");
    });
}

#[test]
fn module_evaluation_rejects_when_a_dependency_throws_after_await() {
    let platform = Waiting::platform(Sources::default());
    j8::EventLoopSet::scope(&platform, std::num::NonZeroUsize::MIN, |set| {
        let rt = Waiting::runtime(set, Executor::default()).expect("runtime");
        let evaluation = rt.eval_module("import './failing.mjs'; globalThis.reached = true;", "main.mjs").expect("module starts");
        rt.run_until_idle().expect("loop runs");
        assert_eq!(state(&rt, &evaluation), "rejected Error: dependency failed");
        let r = rt.eval("typeof reached", "check.js").expect("script runs");
        assert_eq!(rt.text(&r), "undefined");
    });
}

#[test]
fn a_module_that_throws_is_reported_once_as_the_evaluation_error() {
    let platform = Waiting::platform(Sources::default());
    j8::EventLoopSet::scope(&platform, std::num::NonZeroUsize::MIN, |set| {
        let rt = Waiting::runtime(set, Executor::default()).expect("runtime");
        assert!(rt.eval_module("throw new Error('at the top');", "main.mjs").is_err(), "evaluation fails");
        assert!(rt.eval_module("import './failing.mjs';", "sync-wait.mjs").is_ok(), "an awaiting module starts");
        rt.run_until_idle().expect("loop runs");
    });
    assert_eq!(platform.inner().1.load(Ordering::SeqCst), 1, "only the rejection after await reaches the platform, once");
}
