use std::sync::Mutex;
use std::time::Duration;

use j8::frame::{Error, JsObject, Platform, Pool, Scope, Task, Value};
use j8executor::Executor;
use j8platform::DefaultPlatform;

struct Probe;

#[j8::object("probe")]
impl Probe {
    fn sites(a: &mut Scope<'_, '_>) -> Vec<String> {
        a.stack_trace(10).iter().map(|s| format!("{}|{}|{}|{}|{}", s.function, s.script, s.line, s.column, s.constructor)).collect()
    }
}

j8::runtime! {
    Stacks {
        globals: [Probe],
    }
}

#[derive(Default)]
struct Recorder {
    inner: DefaultPlatform,
    rejections: Mutex<Vec<String>>,
}

impl Pool for Recorder {
    fn threads(&self) -> usize {
        self.inner.threads()
    }

    fn run(&self, jobs: Vec<Task<'_>>) {
        self.inner.run(jobs);
    }
}

impl Platform for Recorder {
    fn now(&self) -> Duration {
        self.inner.now()
    }

    fn print(&self, line: &str) {
        self.inner.print(line);
    }

    fn unhandled_rejection<'scope>(&self, scope: &mut Scope<'scope, '_>, _: JsObject<'scope>, reason: Value<'scope>) {
        let text = scope.stack_text(reason).map_or_else(|_| String::from("<threw>"), |s| String::from(&**s));
        self.rejections.lock().expect("not poisoned").push(text);
    }
}

fn eval(text: &str, file: &str) -> Result<String, String> {
    let platform = Stacks::platform(DefaultPlatform::default());
    j8::EventLoopSet::scope(&platform, std::num::NonZeroUsize::MIN, |set| {
        let rt = Stacks::runtime(set, Executor::default()).expect("runtime");
        match rt.eval(text, file) {
            Ok(g) => Ok(rt.text(&g)),
            Err(Error::Exception) => Err(rt.exception_text()),
            Err(e) => panic!("not a completion: {e}"),
        }
    })
}

fn frames(text: &str) -> String {
    let thrown = eval(text, "stack.js").expect_err("the script throws");
    thrown.lines().skip(1).collect::<Vec<_>>().join("\n")
}

#[test]
fn error_stack_names_functions_scripts_lines_and_columns() {
    let got = eval("function inner() {\n  return new Error(\"boom\");\n}\nclass Box {\n  constructor() {\n    this.e = inner();\n  }\n}\nnew Box().e.stack", "stack.js");
    assert_eq!(got.as_deref(), Ok("Error: boom\n    at inner (stack.js:2:10)\n    at new Box (stack.js:6:14)\n    at stack.js:9:1"));
}

#[test]
fn engine_errors_point_at_the_failing_expression() {
    assert_eq!(frames("const o = {};\nfunction call() {\n  o.missing();\n}\ncall();"), "    at call (stack.js:3:5)\n    at stack.js:5:1");
    assert_eq!(frames("const o = {};\nfunction read() {\n  return o.a.b;\n}\nread();"), "    at read (stack.js:3:14)\n    at stack.js:5:1");
    assert_eq!(frames("function f() { return notDefined; }\nf();"), "    at f (stack.js:1:16)\n    at stack.js:2:1");
    assert_eq!(frames("throw new RangeError(\"r\");"), "    at stack.js:1:7");
}

#[test]
fn stack_text_follows_v8() {
    let script = "Error.stackTraceLimit = 1;
const one = new Error('one').stack.split('\\n').length;
Error.stackTraceLimit = 0;
const zero = new Error('zero').stack;
Error.stackTraceLimit = 10;
class V extends Error { constructor(m) { super(m); this.name = 'V'; } }
const sub = new V('m').stack.split('\\n')[0];
const e = new Error('a'); e.message = 'b'; const late = e.stack.split('\\n')[0];
e.stack = 'replaced'; const set = e.stack;
const o = {}; Error.captureStackTrace(o); const captured = o.stack.split('\\n')[0];
[one, zero, sub, late, set, captured].join('|')";
    assert_eq!(eval(script, "stack.js").as_deref(), Ok("2|Error: zero|V: m|Error: b|replaced|Error"));
}

#[test]
fn host_reads_structured_call_sites() {
    let got = eval("function outer() { return probe.sites(); }\nouter().join(';')", "sites.js");
    assert_eq!(got.as_deref(), Ok("outer|sites.js|1|33|false;|sites.js|2|1|false"));
}

#[test]
fn uncaught_exception_text_carries_the_stack() {
    let got = eval("function f() {\n  return null.x;\n}\nf();", "main.js").expect_err("the script throws");
    assert!(got.ends_with("\n    at f (main.js:2:15)\n    at main.js:4:1"), "{got}");
}

#[test]
fn unhandled_rejections_reach_the_platform() {
    let platform = Stacks::platform(Recorder::default());
    j8::EventLoopSet::scope(&platform, std::num::NonZeroUsize::MIN, |set| {
        let rt = Stacks::runtime(set, Executor::default()).expect("runtime");
        let script = "Promise.reject(new Error('lost'));
Promise.reject(1).catch(() => {});
async function work() {
  await null;
  throw new TypeError('async');
}
work();
const late = Promise.reject(2);
late.then(null, () => {});";
        rt.eval(script, "async.js").expect("script runs");
        rt.run_until_idle().expect("loop runs");
    });
    let seen = platform.inner().rejections.lock().expect("not poisoned").clone();
    assert_eq!(seen, ["Error: lost\n    at async.js:1:16", "TypeError: async\n    at work (async.js:5:9)"]);
}

#[test]
fn the_limit_is_read_from_error_itself() {
    let script = "function a() { return b(); }\nfunction b() { return new Error('x').stack.split('\\n').length - 1; }\n\
                  Error.prototype.constructor = { stackTraceLimit: 0 };\nconst all = a();\nError.stackTraceLimit = 1;\n`${all} ${a()}`";
    assert_eq!(eval(script, "limit.js"), Ok(String::from("3 1")));
}
