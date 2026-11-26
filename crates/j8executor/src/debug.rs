use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};

use j8bytecode::{CatchPrediction, Module, Register};
use j8compiler::{Compile, DebugInfo};
use j8framework::{CallSite, Closure, ContextKind, Env, Error, ErrorKind, FuncId, ObjectKind, Scope, Value, ValueKind};

use crate::{threaded, Executor, Frame, Loaded, State};

pub trait Debugger {
    fn paused(&self, p: &mut Paused<'_, '_, '_, '_>) -> Action;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Continue,
    StepInto,
    StepOver,
    StepOut,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PauseOnExceptions {
    #[default]
    None,
    Uncaught,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BreakpointId(pub u32);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Breakpoint {
    pub id: BreakpointId,
    pub location: Option<CallSite>,
}

#[derive(Clone, Debug)]
pub enum Reason<'scope> {
    Breakpoint(Vec<BreakpointId>),
    Step,
    Pause,
    Statement,
    Exception { value: Value<'scope>, uncaught: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeKind {
    Local,
    Closure,
    Block,
    Catch,
    With,
    Script,
    Eval,
    Module,
    Global,
}

#[derive(Clone, Debug)]
pub struct ScopeView<'scope> {
    pub kind: ScopeKind,
    pub variables: Vec<(Box<str>, Value<'scope>)>,
    pub object: Option<Value<'scope>>,
}

type Site = (FuncId, usize);

struct Request {
    id: BreakpointId,
    script: Box<str>,
    line: u32,
    column: u32,
    site: Option<Site>,
}

#[derive(Clone, Copy)]
struct Stepping {
    action: Action,
    level: usize,
}

pub(crate) struct Debug<'p> {
    hook: &'p (dyn Debugger + Sync),
    requests: RefCell<Vec<Request>>,
    armed: RefCell<Vec<Site>>,
    seen: Cell<u32>,
    stepping: Cell<Option<Stepping>>,
    exceptions: Cell<PauseOnExceptions>,
    next: Cell<u32>,
    paused: Cell<bool>,
    requested: Cell<bool>,
}

impl From<ContextKind> for ScopeKind {
    fn from(kind: ContextKind) -> Self {
        match kind {
            ContextKind::Function => Self::Closure,
            ContextKind::Block => Self::Block,
            ContextKind::Catch => Self::Catch,
            ContextKind::With => Self::With,
            ContextKind::Script => Self::Script,
            ContextKind::Eval => Self::Eval,
            ContextKind::Module => Self::Module,
        }
    }
}

impl<'p> Debug<'p> {
    fn new(hook: &'p (dyn Debugger + Sync)) -> Self {
        Self {
            hook,
            requests: RefCell::new(Vec::new()),
            armed: RefCell::new(Vec::new()),
            seen: Cell::new(0),
            stepping: Cell::new(None),
            exceptions: Cell::new(PauseOnExceptions::None),
            next: Cell::new(0),
            paused: Cell::new(false),
            requested: Cell::new(false),
        }
    }

    fn has_breakpoint(&self, site: Site) -> bool {
        self.requests.borrow().iter().any(|r| r.site == Some(site))
    }
}

pub struct Paused<'r, 'scope, 'x, 'p> {
    exe: &'r Executor<'p>,
    pub scope: &'r mut Scope<'scope, 'x>,
    reason: Reason<'scope>,
}

impl<'scope, 'p> Paused<'_, 'scope, '_, 'p> {
    pub fn reason(&self) -> &Reason<'scope> {
        &self.reason
    }

    pub fn executor(&self) -> &Executor<'p> {
        self.exe
    }

    pub fn frames(&self) -> Vec<CallSite> {
        self.scope.stack_trace(usize::MAX)
    }

    fn frame(&self, index: usize) -> Option<(&State<'scope>, &Frame)> {
        self.exe.frames().nth(index)
    }

    pub fn this(&self, frame: usize) -> Option<Value<'scope>> {
        let (st, f) = self.frame(frame)?;
        let loaded = unsafe { f.loaded.get() };
        let k = Loaded::register(i64::from(i32::from(Register::receiver())), loaded.func.bytecode.parameter_count);
        Some(Executor::reg(&st.regs, f, k))
    }

    pub fn scopes(&self, frame: usize) -> Vec<ScopeView<'scope>> {
        let Some((st, f)) = self.frame(frame) else { return Vec::new() };
        let loaded = unsafe { f.loaded.get() };
        let mut views = alloc::vec![ScopeView { kind: ScopeKind::Local, variables: Executor::locals(st, f, loaded), object: None }];
        let mut env = f.env(&st.regs);
        while let Some(e) = env {
            let shape = e.shape.map(|s| self.exe.context_shape(s));
            let variables: Vec<(Box<str>, Value<'scope>)> = shape.map(|s| s.names.iter().enumerate().map(|(i, n)| (Box::from(&*n.name), e.load(i))).collect()).unwrap_or_default();
            if e.shape.is_some() && e.shape == loaded.scope && shape.is_some_and(|s| s.kind == ContextKind::Function) {
                views[0].variables.extend(variables);
            } else if !variables.is_empty() || e.extension.is_some() {
                views.push(ScopeView { kind: shape.map_or(ScopeKind::Block, |s| s.kind.into()), variables, object: e.extension.map(Value::Object) });
            }
            env = e.parent;
        }
        views.push(ScopeView { kind: ScopeKind::Global, variables: Vec::new(), object: Some(Value::Object(self.scope.context.global)) });
        views
    }

    pub fn evaluate(&mut self, frame: usize, source: &str) -> Result<Value<'scope>, Error> {
        let this = self.this(frame).unwrap_or(Value::Undefined);
        let Some((locals, parent, strict)) = self.frame(frame).map(|(st, f)| {
            let loaded = unsafe { f.loaded.get() };
            (Executor::locals(st, f, loaded), f.env(&st.regs), loaded.func.strict)
        }) else {
            return Err(self.scope.throw(ErrorKind::Range, "no such frame"));
        };
        let holder = self.scope.object();
        for (name, v) in &locals {
            self.scope.set_named(Value::Object(holder), name, *v)?;
        }
        let env = Env::context(0, Value::Undefined, parent, None, Some(holder)).alloc(self.scope.mc);
        let module = Module::compile_direct(&j8framework::Source { text: source, file: crate::module::EVAL_FILE }, strict, false, DebugInfo::Record).map_err(|e| match e {
            Error::Syntax(m) => self.scope.throw(ErrorKind::Syntax, &m),
            e => e,
        })?;
        let main = self.exe.load(self.scope.mc, module);
        let callee = Value::Object(self.exe.function_object(self.scope, Closure::new(main, Some(env))));
        self.scope.call(callee, this, &[])
    }
}

impl<'p> Executor<'p> {
    #[must_use]
    pub fn with_debugger(mut self, hook: &'p (dyn Debugger + Sync)) -> Self {
        self.tier = None;
        self.debug = Some(Box::new(Debug::new(hook)));
        self
    }
}

impl Executor<'_> {
    pub(crate) fn debug_info(&self) -> DebugInfo {
        if self.debug.is_some() { DebugInfo::Record } else { DebugInfo::Omit }
    }

    pub fn pause(&self) {
        let Some(d) = &self.debug else { return };
        if d.paused.get() || d.requested.replace(true) {
            return;
        }
        self.start_stepping(d, Stepping { action: Action::StepInto, level: 0 });
    }

    pub fn pause_on_exceptions(&self, mode: PauseOnExceptions) {
        if let Some(d) = &self.debug {
            d.exceptions.set(mode);
        }
    }

    pub fn set_breakpoint(&self, a: &Scope<'_, '_>, script: &str, line: u32, column: u32) -> Option<Breakpoint> {
        let d = self.debug.as_ref()?;
        let id = BreakpointId(d.next.get());
        d.next.set(id.0 + 1);
        let mut r = Request { id, script: script.into(), line, column, site: None };
        let location = self.place(a, &mut r, 0);
        d.requests.borrow_mut().push(r);
        Some(Breakpoint { id, location })
    }

    pub fn clear_breakpoint(&self, id: BreakpointId) {
        let Some(d) = &self.debug else { return };
        let removed = {
            let mut requests = d.requests.borrow_mut();
            let at = requests.iter().position(|r| r.id == id);
            at.map(|i| requests.remove(i))
        };
        if let Some(site) = removed.and_then(|r| r.site) {
            self.disarm(d, site);
        }
    }

    fn place(&self, a: &Scope<'_, '_>, r: &mut Request, from: u32) -> Option<CallSite> {
        let to = u32::try_from(self.code.len()).unwrap_or(u32::MAX);
        let mut script: Option<(u32, bool)> = None;
        let mut in_script = |l: &Loaded| match script {
            Some((base, yes)) if base == l.base => yes,
            _ => {
                let yes = *self.referrer(a, l.base) == *r.script;
                script = Some((l.base, yes));
                yes
            }
        };
        let functions = (from..to).filter_map(|f| Some((FuncId(f), self.live_function(FuncId(f))?))).filter(|(_, l)| in_script(l));
        let (line, column, f, i) = functions.flat_map(|(f, l)| {
            l.func.bytecode.statements().iter().filter(|p| (p.line, p.column) >= (r.line, r.column)).map(move |p| (p.line, p.column, f, l.index_of(p.offset as usize)))
        }).min_by_key(|&(line, column, f, i)| (line, column, f.0, i))?;
        r.site = Some((f, i));
        self.arm((f, i));
        let function = self.live_function(f).map(|l| l.func.name.clone()).unwrap_or_default();
        Some(CallSite { function, script: r.script.clone(), line, column, constructor: false })
    }

    pub(crate) fn debug_new_code(&self, a: &Scope<'_, '_>) {
        let Some(d) = &self.debug else { return };
        let (from, to) = (d.seen.get(), u32::try_from(self.code.len()).unwrap_or(u32::MAX));
        if from == to {
            return;
        }
        d.seen.set(to);
        for r in d.requests.borrow_mut().iter_mut().filter(|r| r.site.is_none()) {
            self.place(a, r, from);
        }
        if d.stepping.get().is_some_and(|s| s.action == Action::StepInto) {
            self.arm_entries(d, from..to);
        }
    }

    fn arm(&self, (f, i): Site) {
        if let Some(d) = self.live_function(f).and_then(|l| l.insns.get(i)) {
            d.handler.set(threaded::brk);
        }
    }

    fn disarm(&self, d: &Debug<'_>, site: Site) {
        if d.has_breakpoint(site) {
            return;
        }
        if let Some(insn) = self.live_function(site.0).and_then(|l| l.insns.get(site.1)) {
            insn.handler.set(crate::Insn::handler(insn.bytecode));
        }
    }

    fn arm_for_step(&self, d: &Debug<'_>, site: Site) {
        self.arm(site);
        d.armed.borrow_mut().push(site);
    }

    fn arm_entries(&self, d: &Debug<'_>, functions: core::ops::Range<u32>) {
        for f in functions.map(FuncId) {
            let Some(l) = self.live_function(f) else { continue };
            if let Some(p) = l.func.bytecode.statements().first() {
                self.arm_for_step(d, (f, l.index_of(p.offset as usize)));
            }
        }
    }

    fn start_stepping(&self, d: &Debug<'_>, stepping: Stepping) {
        d.stepping.set(Some(stepping));
        let mut on_stack = BTreeSet::new();
        for (_, frame) in self.frames() {
            if !on_stack.insert(frame.func.0) {
                continue;
            }
            let loaded = unsafe { frame.loaded.get() };
            for p in loaded.func.bytecode.statements() {
                self.arm_for_step(d, (frame.func, loaded.index_of(p.offset as usize)));
            }
        }
        if stepping.action == Action::StepInto {
            self.arm_entries(d, 0..u32::try_from(self.code.len()).unwrap_or(u32::MAX));
        }
    }

    fn stop_stepping(&self, d: &Debug<'_>) {
        if d.stepping.take().is_none() {
            return;
        }
        for site in d.armed.take() {
            self.disarm(d, site);
        }
    }

    fn locals<'scope>(st: &State<'scope>, f: &Frame, loaded: &Loaded) -> Vec<(Box<str>, Value<'scope>)> {
        let at = loaded.insns.get(f.insn).map_or(0, |d| d.start);
        let count = loaded.func.bytecode.parameter_count;
        loaded.func.bytecode.locals_at(at).map(|l| {
            let v = Executor::reg(&st.regs, f, Loaded::register(i64::from(i32::from(Register(l.register))), count));
            (l.name.clone(), if matches!(v.kind(), ValueKind::Uninit) { Value::Undefined } else { v })
        }).collect()
    }

    pub(crate) fn debug_break<'scope>(&self, a: &mut Scope<'scope, '_>, st: &State<'scope>, level: usize, statement: bool) {
        let Some(d) = &self.debug else { return };
        if d.paused.get() {
            return;
        }
        let f = *st.f();
        let site = (f.func, f.insn);
        let ids: Vec<BreakpointId> = d.requests.borrow().iter().filter(|r| r.site == Some(site)).map(|r| r.id).collect();
        let reason = if statement {
            Reason::Statement
        } else if !ids.is_empty() {
            Reason::Breakpoint(ids)
        } else if d.requested.get() {
            Reason::Pause
        } else {
            match d.stepping.get() {
                Some(Stepping { action: Action::StepInto, .. }) => Reason::Step,
                Some(Stepping { action: Action::StepOver, level: at }) if level <= at => Reason::Step,
                Some(Stepping { action: Action::StepOut, level: at }) if level < at => Reason::Step,
                _ => return,
            }
        };
        self.enter_pause(a, reason, level);
    }

    pub(crate) fn debug_throw<'scope>(&self, a: &mut Scope<'scope, '_>, v: Value<'scope>, level: usize) {
        let Some(d) = &self.debug else { return };
        let mode = d.exceptions.get();
        if d.paused.get() || mode == PauseOnExceptions::None {
            return;
        }
        let uncaught = !self.caught();
        if mode == PauseOnExceptions::Uncaught && !uncaught {
            return;
        }
        self.enter_pause(a, Reason::Exception { value: v, uncaught }, level);
    }

    fn caught(&self) -> bool {
        for (st, f) in self.frames() {
            let loaded = unsafe { f.loaded.get() };
            let at = loaded.insns.get(f.insn).map_or(u32::MAX, |d| d.start);
            let mut handlers: Vec<_> = loaded.func.bytecode.handlers.iter().filter(|h| h.start <= at && at < h.end).collect();
            handlers.sort_by_key(|h| h.end - h.start);
            for h in handlers {
                match h.prediction {
                    CatchPrediction::Caught | CatchPrediction::Promise => return true,
                    CatchPrediction::Uncaught => {}
                    CatchPrediction::AsyncAwait | CatchPrediction::UncaughtAsyncAwait => {
                        if Self::awaited(st, f, loaded) {
                            return true;
                        }
                        break;
                    }
                }
            }
        }
        false
    }

    fn awaited(st: &State<'_>, f: &Frame, loaded: &Loaded) -> bool {
        let Some(r) = loaded.func.bytecode.new_target_or_generator else { return false };
        let Some(co) = st.regs.get(f.base + usize::try_from(r).unwrap_or(usize::MAX)).and_then(|v| v.as_object()) else { return false };
        let promise = match &co.borrow().kind {
            ObjectKind::Coroutine(c) => c.promise,
            _ => None,
        };
        promise.is_some_and(|p| matches!(p.borrow().kind, ObjectKind::Promise { handled: true, .. }))
    }

    fn enter_pause<'scope>(&self, a: &mut Scope<'scope, '_>, reason: Reason<'scope>, level: usize) {
        let Some(d) = &self.debug else { return };
        d.requested.set(false);
        self.stop_stepping(d);
        d.paused.set(true);
        let action = d.hook.paused(&mut Paused { exe: self, scope: a, reason });
        d.paused.set(false);
        if action != Action::Continue {
            self.start_stepping(d, Stepping { action, level });
        }
    }
}
