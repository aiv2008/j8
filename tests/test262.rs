use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use j8builtins::Builtins;
use j8executor::Executor;
use j8framework::{CreateParams, Error, Isolate, Platform, Source, Task};
use serde::Deserialize;

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/test262");

const BASELINE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/test262.passing");

const CASE_ENV: &str = "J8_TEST262_CASE";
const UPDATE_ENV: &str = "J8_TEST262_UPDATE";
const VERBOSE_ENV: &str = "J8_TEST262_VERBOSE";

const JIT_ENV: &str = "J8_TEST262_JIT";

const TIMEOUT: Duration = Duration::from_secs(10);

const POLL: Duration = Duration::from_millis(2);

const ASYNC_DONE: &str = "Test262:AsyncTestComplete";

const GROUP_DEPTH: usize = 2;

#[derive(Deserialize, Default)]
struct Meta {
    #[serde(default)]
    includes: Vec<String>,
    #[serde(default)]
    flags: Vec<Flag>,
    negative: Option<Negative>,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
enum Flag {
    #[serde(rename = "onlyStrict")]
    OnlyStrict,
    #[serde(rename = "noStrict")]
    NoStrict,
    #[serde(rename = "module")]
    Module,
    #[serde(rename = "raw")]
    Raw,
    #[serde(rename = "async")]
    Async,
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct Negative {
    phase: Phase,
    #[serde(rename = "type")]
    kind: ErrorType,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Phase {
    Parse,
    Resolution,
    Runtime,
}

impl Phase {
    const fn name(self) -> &'static str {
        match self {
            Self::Parse => "parse",
            Self::Resolution => "resolution",
            Self::Runtime => "runtime",
        }
    }
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
enum ErrorType {
    SyntaxError,
    TypeError,
    ReferenceError,
    RangeError,
    EvalError,
    URIError,
    Test262Error,
    #[serde(other)]
    Other,
}

impl ErrorType {
    const fn name(self) -> &'static str {
        match self {
            Self::SyntaxError => "SyntaxError",
            Self::TypeError => "TypeError",
            Self::ReferenceError => "ReferenceError",
            Self::RangeError => "RangeError",
            Self::EvalError => "EvalError",
            Self::URIError => "URIError",
            Self::Test262Error => "Test262Error",
            Self::Other => "?",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
enum Outcome {
    Pass = 0,
    Fail = 1,
    Unsupported = 2,
    Skip = 3,
    Timeout = 4,
    Crash = 5,
}

impl Outcome {
    const ALL: [Self; 6] = [Self::Pass, Self::Fail, Self::Unsupported, Self::Skip, Self::Timeout, Self::Crash];

    const fn name(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Unsupported => "unsupported",
            Self::Skip => "skip",
            Self::Timeout => "timeout",
            Self::Crash => "crash",
        }
    }
}

impl From<Outcome> for ExitCode {
    fn from(o: Outcome) -> Self {
        ExitCode::from(o as u8)
    }
}

impl TryFrom<i32> for Outcome {
    type Error = ();

    fn try_from(code: i32) -> Result<Self, ()> {
        Self::ALL.into_iter().find(|o| i32::from(*o as u8) == code).ok_or(())
    }
}

struct Capture {
    start: Instant,
    lines: Mutex<Vec<String>>,
}

impl j8framework::Pool for Capture {
    fn threads(&self) -> usize {
        1
    }

    fn run(&self, tasks: Vec<Task<'_>>) {
        for t in tasks {
            t();
        }
    }
}

impl Platform for Capture {
    fn wall_clock(&self) -> Duration {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default()
    }

    fn load_module(&self, referrer: &str, specifier: &str) -> Option<(String, String)> {
        let referrer = Suite::test_dir().join(referrer);
        let base = referrer.parent()?;
        let path = std::fs::canonicalize(base.join(specifier)).ok()?;
        let text = std::fs::read_to_string(&path).ok()?;
        Some((path.display().to_string(), text))
    }

    fn now(&self) -> Duration {
        self.start.elapsed()
    }

    fn print(&self, line: &str) {
        self.lines.lock().expect("an unpoisoned log").push(line.into());
    }
}

struct Case {
    path: PathBuf,
    text: String,
    meta: Meta,
}

impl Case {
    fn load(path: PathBuf) -> Result<Self, String> {
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let meta = match (text.find("/*---"), text.find("---*/")) {
            (Some(a), Some(b)) if a < b => serde_yaml::from_str(&text[a + 5..b]).map_err(|e| format!("metadata: {e}"))?,
            _ => Meta::default(),
        };
        Ok(Self { path, text, meta })
    }

    fn has(&self, flag: Flag) -> bool {
        self.meta.flags.contains(&flag)
    }

    fn source(&self) -> Result<String, String> {
        if self.has(Flag::Raw) {
            return Ok(self.text.clone());
        }
        let mut src = String::new();
        if self.has(Flag::OnlyStrict) {
            src.push_str("\"use strict\";\n");
        }
        src.push_str(HOST_262);
        let mut harness = vec!["assert.js", "sta.js"];
        if self.has(Flag::Async) {
            src.push_str("function print(m) { console.log(m); }\n");
            harness.push("doneprintHandle.js");
        }
        for h in harness.into_iter().chain(self.meta.includes.iter().map(String::as_str)) {
            let p = Path::new(ROOT).join("harness").join(h);
            src.push_str(&std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?);
            src.push('\n');
        }
        if !self.has(Flag::Module) {
            src.push_str(&self.text);
        }
        Ok(src)
    }

    fn run(&self) -> (Outcome, String) {
        let src = match self.source() {
            Ok(s) => s,
            Err(e) => return (Outcome::Fail, e),
        };
        let platform = Capture { start: Instant::now(), lines: Mutex::default() };
        let mut isolate = Isolate::new(&platform, CreateParams::default());
        let executor = if std::env::var_os(JIT_ENV).is_some() { Executor::default().with_tier(j8jit::Jit::from(0)) } else { Executor::default() };
        let file = self.path.strip_prefix(Suite::test_dir()).unwrap_or(&self.path).display().to_string();
        let expected = self.meta.negative.as_ref();
        let cx = isolate.create_context(&Builtins);
        let r = if self.has(Flag::Module) {
            let module = std::fs::canonicalize(&self.path).map_or(file, |p| p.display().to_string());
            match executor.eval(&mut isolate, &cx, &Source { text: &src, file: "[harness]" }) {
                Ok(_) => executor.eval_module(&mut isolate, &cx, &Source { text: &self.text, file: &module }),
                e => e,
            }
        } else {
            executor.eval(&mut isolate, &cx, &Source { text: &src, file: &file })
        };
        let (outcome, why) = isolate.enter(&cx, &executor, |agent| {
            match (r, expected) {
                (Err(Error::Unsupported(m)), _) => (Outcome::Unsupported, m),
                (Err(Error::Syntax(_)), Some(n)) if n.phase != Phase::Runtime && n.kind == ErrorType::SyntaxError => (Outcome::Pass, String::new()),
                (Err(Error::Syntax(m)), _) => (Outcome::Fail, format!("syntax error: {m}")),
                (Err(Error::Terminated), _) => (Outcome::Fail, String::from("execution terminated")),
                (Err(Error::Parked(_)), _) => (Outcome::Fail, String::from("a parked exception reached the script")),
                (Err(Error::Exception), n) => {
                    let v = agent.take_exception();
                    let name = agent.get_named(v, "constructor").and_then(|c| agent.get_named(c, "name")).and_then(|n| agent.to_string(n));
                    let name = name.map_or_else(|_| String::new(), |s| String::from(&**s));
                    match n {
                        Some(n) if n.phase == Phase::Runtime && name == n.kind.name() => (Outcome::Pass, String::new()),
                        _ => {
                            let text = agent.to_string(v).map_or_else(|_| String::from("?"), |s| String::from(&**s));
                            (Outcome::Fail, format!("threw {text}"))
                        }
                    }
                }
                (Ok(_), Some(n)) => (Outcome::Fail, format!("expected {} ({} phase)", n.kind.name(), n.phase.name())),
                (Ok(_), None) => (Outcome::Pass, String::new()),
            }
        });
        if outcome == Outcome::Pass && self.has(Flag::Async) {
            let lines = std::mem::take(&mut *platform.lines.lock().expect("an unpoisoned log"));
            if !lines.iter().any(|l| l == ASYNC_DONE) {
                return (Outcome::Fail, format!("async: {}", lines.join(" | ")));
            }
        }
        (outcome, why)
    }
}

const HOST_262: &str = "var $262 = (function (transfer) { return { global: globalThis, gc: function () {}, evalScript: function (s) { return (0, eval)(s); }, \
createRealm: function () { throw new TypeError('createRealm is not supported'); }, \
detachArrayBuffer: function (b) { transfer.call(b); return null; }, agent: {} }; })(ArrayBuffer.prototype.transfer);\n";

struct Ran {
    rel: String,
    outcome: Outcome,
    why: String,
}

struct Suite {
    cases: Vec<String>,
    exe: PathBuf,
}

impl Suite {
    fn test_dir() -> PathBuf {
        Path::new(ROOT).join("test")
    }

    fn baseline() -> BTreeSet<String> {
        std::fs::read_to_string(BASELINE).unwrap_or_default().lines().filter(|l| !l.is_empty()).map(String::from).collect()
    }

    fn walk(dir: &Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                Self::walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "js") && !p.to_string_lossy().contains("_FIXTURE") {
                let rel = p.strip_prefix(Self::test_dir()).unwrap_or(&p);
                out.push(rel.to_string_lossy().into_owned());
            }
        }
    }

    fn new(filters: &[String]) -> Result<Self, String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut cases = Vec::new();
        if filters.is_empty() {
            cases.extend(Self::baseline());
        }
        for f in filters {
            let p = Self::test_dir().join(f);
            if p.is_file() {
                cases.push(f.clone());
            } else if p.is_dir() {
                Self::walk(&p, &mut cases);
            } else {
                return Err(format!("{}: no such test file or directory", p.display()));
            }
        }
        cases.sort();
        cases.dedup();
        Ok(Self { cases, exe })
    }

    fn run_one(&self, rel: &str) -> Ran {
        let spawned = Command::new(&self.exe)
            .env(CASE_ENV, Self::test_dir().join(rel))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => return Ran { rel: rel.into(), outcome: Outcome::Crash, why: e.to_string() },
        };
        let start = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break Some(s),
                Ok(None) if start.elapsed() < TIMEOUT => std::thread::sleep(POLL),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        let mut why = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_string(&mut why);
        }
        let outcome = match status.map(|s| s.code()) {
            None => Outcome::Timeout,
            Some(code) => code.and_then(|c| Outcome::try_from(c).ok()).unwrap_or(Outcome::Crash),
        };
        Ran { rel: rel.into(), outcome, why: why.trim().into() }
    }

    fn run(&self) -> Vec<Ran> {
        let jobs = std::thread::available_parallelism().map_or(1, |n| (n.get() / 2).max(1));
        let next = AtomicUsize::new(0);
        let done = Mutex::new(Vec::with_capacity(self.cases.len()));
        std::thread::scope(|s| {
            for _ in 0..jobs {
                s.spawn(|| {
                    while let Some(rel) = self.cases.get(next.fetch_add(1, Ordering::Relaxed)) {
                        let r = self.run_one(rel);
                        done.lock().expect("an unpoisoned result list").push(r);
                    }
                });
            }
        });
        let mut done = done.into_inner().expect("an unpoisoned result list");
        done.sort_by(|a, b| a.rel.cmp(&b.rel));
        done
    }
}

struct Report<'r> {
    ran: &'r [Ran],
}

impl Report<'_> {
    fn counts<'a>(rs: impl Iterator<Item = &'a Ran>) -> [usize; Outcome::ALL.len()] {
        let mut c = [0; Outcome::ALL.len()];
        rs.for_each(|r| c[r.outcome as usize] += 1);
        c
    }

    fn row(label: &str, c: &[usize; Outcome::ALL.len()]) {
        let total: usize = c.iter().sum();
        let cols: Vec<String> = c.iter().map(|n| format!("{n:>12}")).collect();
        #[expect(clippy::cast_precision_loss, reason = "percentage")]
        let pct = if total == 0 { 0.0 } else { c[Outcome::Pass as usize] as f64 * 100.0 / total as f64 };
        println!("{label:<48}{}{total:>8}{pct:>7.1}%", cols.concat());
    }

    fn print(&self) {
        let heads: Vec<String> = Outcome::ALL.iter().map(|o| format!("{:>12}", o.name())).collect();
        println!("{:<48}{}{:>8}{:>8}", "test262", heads.concat(), "total", "pass%");
        let mut groups: Vec<&str> = self.ran.iter().map(|r| Self::group(&r.rel)).collect();
        groups.dedup();
        for g in groups {
            Self::row(g, &Self::counts(self.ran.iter().filter(|r| Self::group(&r.rel) == g)));
        }
        Self::row("total", &Self::counts(self.ran.iter()));
        if std::env::var_os(VERBOSE_ENV).is_some() {
            for r in self.ran.iter().filter(|r| r.outcome != Outcome::Pass) {
                println!("{:<11} {}  {}", r.outcome.name(), r.rel, r.why.lines().next().unwrap_or(""));
            }
        }
    }

    fn group(rel: &str) -> &str {
        rel.match_indices('/').nth(GROUP_DEPTH - 1).map_or(rel, |(i, _)| &rel[..i])
    }

    fn against_baseline(&self, update: bool) -> Result<Vec<&str>, String> {
        let base = Suite::baseline();
        let passing: BTreeSet<&str> = self.ran.iter().filter(|r| r.outcome == Outcome::Pass).map(|r| r.rel.as_str()).collect();
        let lost: Vec<&str> = self.ran.iter().map(|r| r.rel.as_str()).filter(|r| base.contains(*r) && !passing.contains(r)).collect();
        let gained = passing.iter().filter(|r| !base.contains(**r)).count();
        if update {
            let ran: BTreeSet<&str> = self.ran.iter().map(|r| r.rel.as_str()).collect();
            let merged: BTreeSet<&str> = base.iter().map(String::as_str).filter(|r| !ran.contains(r)).chain(passing.iter().copied()).collect();
            let text: String = merged.iter().flat_map(|r| [*r, "\n"]).collect();
            std::fs::write(BASELINE, text).map_err(|e| format!("{BASELINE}: {e}"))?;
            println!("baseline: {} passing (+{gained}, -{})", merged.len(), lost.len());
        } else if gained > 0 {
            println!("{gained} newly passing; rerun with {UPDATE_ENV}=1 to record them");
        }
        Ok(lost)
    }
}

fn main() -> ExitCode {
    if let Some(path) = std::env::var_os(CASE_ENV) {
        let (outcome, why) = match Case::load(PathBuf::from(path)) {
            Ok(case) => case.run(),
            Err(e) => (Outcome::Fail, e),
        };
        if !why.is_empty() {
            eprintln!("{why}");
        }
        return outcome.into();
    }
    let filters: Vec<String> = std::env::args().skip(1).filter(|a| !a.starts_with('-')).collect();
    let suite = match Suite::new(&filters) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("test262: {e}");
            return ExitCode::FAILURE;
        }
    };
    if suite.cases.is_empty() {
        println!("test262: nothing to run (empty baseline; pass directories, e.g. `-- language/expressions`)");
        return ExitCode::SUCCESS;
    }
    let ran = suite.run();
    let report = Report { ran: &ran };
    report.print();
    match report.against_baseline(std::env::var_os(UPDATE_ENV).is_some()) {
        Ok(lost) if lost.is_empty() => ExitCode::SUCCESS,
        Ok(lost) => {
            println!("{} regressions against {BASELINE}:", lost.len());
            for r in &lost {
                println!("  {r}");
            }
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("test262: {e}");
            ExitCode::FAILURE
        }
    }
}
