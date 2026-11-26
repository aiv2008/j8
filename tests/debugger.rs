use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::time::Duration;
use std::sync::Mutex;

use j8::frame::{CreateParams, Isolate, ScriptKind, Source, Value};
use j8builtins::Builtins;
use j8executor::{Action, Breakpoint, Debugger, EventLoop, EventLoopSet, Executor, PauseOnExceptions, Paused, Reason, ScopeKind};
use j8platform::DefaultPlatform;

const SCRIPT: &str = "function add(a, b) {
  const sum = a + b;
  return sum;
}
function main() {
  let x = 1;
  let y = add(x, 2);
  y = y * 10;
  return y;
}
main();
";

#[derive(Default)]
struct Log {
    actions: VecDeque<Action>,
    evaluate: VecDeque<&'static str>,
    pauses: Vec<String>,
}

#[derive(Default)]
struct Recorder(Mutex<Log>);

impl Recorder {
    fn with(actions: &[Action], evaluate: &[&'static str]) -> Self {
        let log = Log { actions: actions.iter().copied().collect(), evaluate: evaluate.iter().copied().collect(), pauses: Vec::new() };
        Self(Mutex::new(log))
    }

    fn pauses(&self) -> Vec<String> {
        self.0.lock().expect("not poisoned").pauses.clone()
    }
}

fn text<'s>(p: &mut Paused<'_, 's, '_, '_>, v: Value<'s>) -> String {
    p.scope.to_string(v).map_or_else(|_| String::from("?"), |s| String::from(&**s))
}

impl Debugger for Recorder {
    fn paused(&self, p: &mut Paused<'_, '_, '_, '_>) -> Action {
        let (action, expr) = {
            let mut log = self.0.lock().expect("not poisoned");
            (log.actions.pop_front().unwrap_or(Action::Continue), log.evaluate.pop_front())
        };
        let reason = match p.reason().clone() {
            Reason::Breakpoint(ids) => format!("breakpoint {}", ids.len()),
            Reason::Step => String::from("step"),
            Reason::Pause => String::from("pause"),
            Reason::Statement => String::from("debugger"),
            Reason::Exception { value, uncaught } => format!("exception {} uncaught={uncaught}", text(p, value)),
        };
        let frames = p.frames();
        let top = &frames[0];
        let locals = p.scopes(0).into_iter().find(|s| s.kind == ScopeKind::Local).map(|s| {
            let vars: Vec<String> = s.variables.into_iter().map(|(n, v)| format!("{n}={}", text(p, v))).collect();
            vars.join(",")
        });
        let evaluated = expr.map(|e| match p.evaluate(0, e) {
            Ok(v) => text(p, v),
            Err(_) => String::from("threw"),
        });
        let line = format!("{reason} | {}:{} | {} | {} | {}", top.function, top.line, frames.len(), locals.unwrap_or_default(), evaluated.unwrap_or_default());
        self.0.lock().expect("not poisoned").pauses.push(line);
        action
    }
}

struct Session<'p> {
    isolate: Isolate<'p>,
    cx: j8::frame::ScopeGlobal<'p, j8::frame::ContextBrand>,
}

impl<'p> Session<'p> {
    fn new(platform: &'p DefaultPlatform) -> Self {
        let mut isolate = Isolate::new(platform, CreateParams::default());
        let cx = isolate.create_context(&Builtins);
        Self { isolate, cx }
    }

    fn breakpoint(&mut self, executor: &Executor<'p>, script: &str, line: u32, column: u32) -> Option<Breakpoint> {
        self.isolate.enter(&self.cx, executor, |a| executor.set_breakpoint(a, script, line, column))
    }

    fn run(&mut self, executor: &Executor<'p>, script: &str, file: &str) -> Result<String, String> {
        let g = executor.eval(&mut self.isolate, &self.cx, &Source { text: script, file }).map_err(|e| e.to_string())?;
        Ok(self.isolate.enter(&self.cx, executor, |a| {
            let v = a.local(&g);
            a.to_string(v).map_or_else(|_| String::new(), |s| String::from(&**s))
        }))
    }
}

#[test]
fn breakpoint_steps_scopes_and_evaluation() {
    let platform = DefaultPlatform::default();
    let recorder = Recorder::with(&[Action::StepInto, Action::StepOver, Action::StepOut, Action::Continue], &["x + 41", "a * b", "sum", "y"]);
    let mut session = Session::new(&platform);
    let executor = Executor::default().with_debugger(&recorder);
    let bp = session.breakpoint(&executor, "main.js", 7, 0).expect("a debugger is attached");
    assert_eq!(bp.location, None, "the script is not loaded yet");
    assert_eq!(session.run(&executor, SCRIPT, "main.js"), Ok(String::from("30")));
    assert_eq!(recorder.pauses(), [
        "breakpoint 1 | main:7 | 2 | x=1,y=undefined | 42",
        "step | add:2 | 3 | a=1,b=2,sum=undefined | 2",
        "step | add:3 | 3 | a=1,b=2,sum=3 | 3",
        "step | main:8 | 2 | x=1,y=3 | 3",
    ]);
}

#[test]
fn cleared_breakpoints_and_debugger_statements() {
    let platform = DefaultPlatform::default();
    let recorder = Recorder::default();
    let mut session = Session::new(&platform);
    let executor = Executor::default().with_debugger(&recorder);
    let bp = session.breakpoint(&executor, "cleared.js", 2, 0).expect("a debugger is attached");
    executor.clear_breakpoint(bp.id);
    assert_eq!(session.run(&executor, "let n = 1;\nn += 1;\ndebugger;\nn;\n", "cleared.js"), Ok(String::from("2")));
    assert_eq!(recorder.pauses(), ["debugger | :3 | 1 |  | "]);
}

#[test]
fn breakpoints_resolve_in_loaded_scripts() {
    let platform = DefaultPlatform::default();
    let recorder = Recorder::with(&[], &["a + b"]);
    let mut session = Session::new(&platform);
    let executor = Executor::default().with_debugger(&recorder);
    assert_eq!(session.run(&executor, "function f(a, b) {\n  return a + b;\n}\n", "lib.js"), Ok(String::from("undefined")));
    let bp = session.breakpoint(&executor, "lib.js", 2, 0).expect("a debugger is attached");
    assert_eq!(bp.location.map(|l| (l.line, l.column)), Some((2, 3)));
    assert_eq!(session.run(&executor, "f(20, 22);\n", "call.js"), Ok(String::from("42")));
    assert_eq!(recorder.pauses(), ["breakpoint 1 | f:2 | 2 | a=20,b=22 | 42"]);
}

#[test]
fn captured_variables_read_from_contexts() {
    let platform = DefaultPlatform::default();
    let recorder = Recorder::with(&[], &["count + step"]);
    let mut session = Session::new(&platform);
    let executor = Executor::default().with_debugger(&recorder);
    let script = "function counter(step) {\n  let count = 10;\n  return () => {\n    count += step;\n    return count;\n  };\n}\ncounter(5)();\n";
    session.breakpoint(&executor, "closure.js", 4, 0);
    assert_eq!(session.run(&executor, script, "closure.js"), Ok(String::from("15")));
    assert_eq!(recorder.pauses(), ["breakpoint 1 | :4 | 2 |  | 15"]);
}

#[test]
fn exceptions_pause_by_mode() {
    let platform = DefaultPlatform::default();
    let recorder = Recorder::default();
    let mut session = Session::new(&platform);
    let executor = Executor::default().with_debugger(&recorder);
    let script = "function boom(m) { throw new Error(m); }\ntry { boom('caught'); } catch (e) {}\ntry { boom('finally'); } finally {}\n";
    executor.pause_on_exceptions(PauseOnExceptions::Uncaught);
    assert!(session.run(&executor, script, "uncaught.js").is_err());
    executor.pause_on_exceptions(PauseOnExceptions::All);
    assert!(session.run(&executor, script, "all.js").is_err());
    assert_eq!(recorder.pauses(), [
        "exception Error: finally uncaught=true | boom:1 | 2 | m=finally | ",
        "exception Error: caught uncaught=false | boom:1 | 2 | m=caught | ",
        "exception Error: finally uncaught=true | boom:1 | 2 | m=finally | ",
    ]);
}

#[test]
fn an_exception_through_a_native_pauses_once() {
    let platform = DefaultPlatform::default();
    let recorder = Recorder::default();
    let mut session = Session::new(&platform);
    let executor = Executor::default().with_debugger(&recorder);
    executor.pause_on_exceptions(PauseOnExceptions::All);
    let script = "try { [1].forEach(() => { throw new Error('inner'); }); } catch (e) {}\n[2].forEach(() => { throw new Error('outer'); });\n";
    assert!(session.run(&executor, script, "native.js").is_err());
    assert_eq!(recorder.pauses(), [
        "exception Error: inner uncaught=false | :1 | 2 |  | ",
        "exception Error: outer uncaught=true | :2 | 2 |  | ",
    ]);
}

#[test]
fn rejections_of_async_functions_are_predicted() {
    let platform = DefaultPlatform::default();
    let recorder = Recorder::default();
    EventLoopSet::scope(&platform, NonZeroUsize::MIN, |set| {
        let js = set.spawn(Executor::default().with_debugger(&recorder), &Builtins);
        js.interrupter().interrupt(|exe, _| exe.pause_on_exceptions(PauseOnExceptions::Uncaught));
        let script = "async function late(m) { await null; throw new Error(m); }\n(async () => { try { await late('awaited'); } catch (e) {} })();\nlate('lost');\n";
        js.eval(&Source { text: script, file: "async.js" }, ScriptKind::Classic).expect("runs");
        js.run().expect("idle");
    });
    assert_eq!(recorder.pauses(), ["exception Error: lost uncaught=true | late:1 | 1 | m=lost | "]);
}

fn answer(js: &EventLoop<'_>, g: &j8::frame::ScopeGlobal<'_, j8::frame::ValueBrand>) -> String {
    js.enter(|a| {
        let v = a.local(g);
        a.to_string(v).map_or_else(|_| String::new(), |s| String::from(&**s))
    })
}

#[test]
fn an_interrupt_pauses_a_busy_loop() {
    let platform = DefaultPlatform::default();
    let recorder = Recorder::with(&[], &["spin = false"]);
    let result = EventLoopSet::scope(&platform, NonZeroUsize::MIN, |set| {
        let js = set.spawn(Executor::default().with_debugger(&recorder), &Builtins);
        let script = "var spin = true;\nvar turns = 0;\nwhile (spin) {\n  turns++;\n}\nturns > 0;\n";
        let ticket = js.submit(&Source { text: script, file: "spin.js" }, ScriptKind::Classic);
        std::thread::sleep(Duration::from_millis(100));
        js.interrupter().interrupt(|exe, _| exe.pause());
        let g = js.wait(ticket).expect("runs");
        answer(&js, &g)
    });
    assert_eq!(result, "true");
    assert_eq!(recorder.pauses(), ["pause | :3 | 1 |  | false"]);
}

#[test]
fn an_interrupt_sets_a_breakpoint_while_the_loop_is_idle() {
    let platform = DefaultPlatform::default();
    let recorder = Recorder::with(&[], &["a + b"]);
    let result = EventLoopSet::scope(&platform, NonZeroUsize::MIN, |set| {
        let js = set.spawn(Executor::default().with_debugger(&recorder), &Builtins);
        js.eval(&Source { text: "function f(a, b) {\n  return a + b;\n}\n", file: "lib.js" }, ScriptKind::Classic).expect("runs");
        let (tx, rx) = std::sync::mpsc::channel();
        js.interrupter().interrupt(move |exe, a| _ = tx.send(exe.set_breakpoint(a, "lib.js", 2, 0).and_then(|bp| bp.location).map(|l| (l.line, l.column))));
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)), Ok(Some((2, 3))), "placed while nothing ran");
        let g = js.eval(&Source { text: "f(20, 22);\n", file: "call.js" }, ScriptKind::Classic).expect("runs");
        answer(&js, &g)
    });
    assert_eq!(result, "42");
    assert_eq!(recorder.pauses(), ["breakpoint 1 | f:2 | 2 | a=20,b=22 | 42"]);
}
