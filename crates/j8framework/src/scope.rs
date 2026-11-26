use crate::ScopeLocal;
use crate::ValueKind;
use alloc::vec::Vec;

use crate::{Collect, Mutation};

use crate::{Context, Error, JsObject, Platform, Profiler, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CodeKind {
    Eval,
    StrictEval,
    Script,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Collect)]
#[collect(require_static)]
pub enum Resume {
    Next,
    Throw,
    Return,
}

impl Resume {
    pub const ALL: [Resume; 3] = [Resume::Next, Resume::Throw, Resume::Return];
}

impl From<Resume> for f64 {
    fn from(m: Resume) -> f64 {
        match m {
            Resume::Next => 0.0,
            Resume::Return => 1.0,
            Resume::Throw => 2.0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Step<'scope> {
    Yield(Value<'scope>),
    Await(Value<'scope>),
    Done(Value<'scope>),
}

pub trait Invoke<'scope> {
    fn call(&self, agent: &mut Scope<'scope, '_>, callee: Value<'scope>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error>;

    fn construct(&self, agent: &mut Scope<'scope, '_>, callee: Value<'scope>, args: &[Value<'scope>], new_target: Value<'scope>) -> Result<Value<'scope>, Error>;

    fn eval_code(&self, agent: &mut Scope<'scope, '_>, src: &str, kind: CodeKind, hidden: bool) -> Result<Value<'scope>, Error>;
    fn constructs(&self, closure: &crate::Closure<'scope>) -> bool;

    fn resume(&self, agent: &mut Scope<'scope, '_>, coroutine: crate::JsObject<'scope>, mode: Resume, value: Value<'scope>) -> Result<Step<'scope>, Error>;

    fn callback(&self, _agent: &mut Scope<'scope, '_>, callee: Value<'scope>, this: Value<'scope>) -> Callback<'scope> {
        Callback::from((callee, this))
    }

    fn capture(&self, _agent: &Scope<'scope, '_>, _limit: usize) -> Vec<crate::Captured> {
        Vec::new()
    }

    fn call_site(&self, _agent: &Scope<'scope, '_>, _frame: &crate::Captured) -> crate::CallSite {
        crate::CallSite::default()
    }
}

pub type CallbackRun = for<'scope, 'x> unsafe fn(&mut Scope<'scope, 'x>, &Callback<'scope>, &[Value<'scope>]) -> Option<Result<Value<'scope>, Error>>;

pub struct Callback<'scope> {
    pub callee: Value<'scope>,
    pub this: Value<'scope>,
    pub run: Option<CallbackRun>,
    pub code: usize,
    pub arity: usize,
    pub env: *mut (),
    pub release: Option<unsafe fn(*mut ())>,
    pub quiet: core::cell::Cell<bool>,
}

impl<'scope> From<(Value<'scope>, Value<'scope>)> for Callback<'scope> {
    fn from((callee, this): (Value<'scope>, Value<'scope>)) -> Self {
        Self { callee, this, run: None, code: 0, arity: 0, env: core::ptr::null_mut(), release: None, quiet: core::cell::Cell::new(false) }
    }
}

impl Drop for Callback<'_> {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            unsafe { release(self.env) };
        }
    }
}

impl<'scope> Callback<'scope> {
    #[inline]
    pub fn passed(&self) -> Option<usize> {
        self.run.map(|_| self.arity)
    }

    #[inline]
    pub fn call(&self, a: &mut Scope<'scope, '_>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        self.quiet.set(false);
        self.enter(a, args)
    }

    #[inline]
    pub fn again(&self, a: &mut Scope<'scope, '_>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        self.quiet.set(true);
        self.enter(a, args)
    }

    #[inline]
    fn enter(&self, a: &mut Scope<'scope, '_>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        if let Some(run) = self.run
            && let Some(r) = unsafe { run(a, self, args) }
        {
            return r;
        }
        let invoke = a.invoke;
        invoke.call(a, self.callee, self.this, args)
    }
}

pub struct Scope<'scope, 'x> {
    pub mc: &'x Mutation<'scope>,
    pub platform: &'x dyn Platform,
    pub profiler: &'x mut Profiler,
    pub context: ScopeLocal<'scope, Context<'scope>>,
    pub new_target: Value<'scope>,
    pub invoke: &'x dyn Invoke<'scope>,
    pub spawned: Option<&'x j8lockfree::Fifo<crate::AsyncJob<'x>>>,
    pub timers: Option<&'x crate::Timers<'x>>,
    pub stop: &'x crate::Termination,
    pub parked: &'x core::cell::RefCell<crate::Parked<'x>>,
    pub spare_regs: Vec<Vec<Value<'scope>>>,
    pub(crate) heap_limit: Option<&'x crate::HeapLimit<'x>>,
    pub(crate) stack_limit: usize,
    pub(crate) joining: Visiting<'scope>,
    pub gc_safe: core::cell::Cell<bool>,
    pub handles: Vec<Value<'scope>>,
    #[cfg(all(target_os = "linux", feature = "crossheap"))]
    pub shares: &'x mut crate::Shares<'x>,
}


#[derive(Default)]
pub struct Visiting<'scope>(Vec<JsObject<'scope>>);

impl<'scope> Visiting<'scope> {
    pub fn enter(&mut self, o: JsObject<'scope>) -> bool {
        if self.0.iter().any(|&v| ScopeLocal::ptr_eq(v, o)) {
            return false;
        }
        self.0.push(o);
        true
    }

    pub fn leave(&mut self) {
        self.0.pop();
    }
}

impl<'scope> Scope<'scope, '_> {
    pub fn check_heap_limit(&self) {
        let Some(limit) = self.heap_limit else { return };
        let gc = self.mc.gc_scope();
        if gc.used() > limit.current() {
            limit.pending().set(true);
            self.stop.request_collect();
        } else {
            gc.set_limit(Some(limit.current()));
        }
    }

    pub fn heap_limit_pending(&self) -> bool {
        self.heap_limit.is_some_and(|l| l.pending().get())
    }

    pub fn recheck_heap_limit(&self) {
        let Some(limit) = self.heap_limit else { return };
        if !limit.pending().replace(false) {
            return;
        }
        let gc = self.mc.gc_scope();
        if limit.exceeded(gc.used()) {
            self.stop.request();
        } else {
            gc.set_limit(Some(limit.current()));
        }
    }

    const SPARE_REGS: usize = 8;

    pub fn after(&self, delay: core::time::Duration, keep_alive: bool) -> Result<(crate::JsObject<'scope>, u64), Error> {
        let Some(timers) = self.timers else { return Err(self.throw(crate::ErrorKind::Type, "timers need a runtime with an event loop (j8::Runtime::run_until_idle)")) };
        let promise = self.new_promise();
        let (resolve, _) = self.promise_resolvers(promise);
        Ok((promise, timers.arm(self.platform.now().saturating_add(delay), keep_alive, self.global(resolve.into()))))
    }

    pub fn cancel_after(&self, id: u64) {
        if let Some(timers) = self.timers {
            timers.cancel(id);
        }
    }

    #[inline]
    fn stack_address() -> usize {
        let marker = 0u8;
        core::hint::black_box(core::ptr::addr_of!(marker)) as usize
    }

    pub(crate) fn stack_limit_here(size: usize) -> usize {
        Self::stack_address().saturating_sub(size)
    }

    #[inline]
    pub fn check_stack(&self) -> Result<(), Error> {
        if Self::stack_address() < self.stack_limit {
            return Err(self.throw(crate::ErrorKind::Range, "Maximum call stack size exceeded"));
        }
        Ok(())
    }

    pub fn joining(&mut self) -> &mut Visiting<'scope> {
        &mut self.joining
    }

    pub fn spawn<'i>(&self, job: crate::AsyncJob<'i>) -> Result<(), crate::AsyncJob<'i>> {
        match self.spawned {
            Some(q) => {
                q.push(unsafe { crate::rebrand(job) });
                Ok(())
            }
            None => Err(job),
        }
    }

    pub fn park(&self) -> crate::Error {
        let v = self.take_exception();
        let root = self.global(v);
        crate::Error::Parked(self.parked.borrow_mut().park(root))
    }

    pub fn unpark(&self, ticket: u64) -> crate::Error {
        let v = self.parked.borrow_mut().unpark(ticket).map_or(Value::Undefined, |root| self.local(&root));
        self.mc.raise(v)
    }

    pub fn take_regs(&mut self) -> Vec<Value<'scope>> {
        self.spare_regs.pop().unwrap_or_default()
    }

    pub fn give_regs(&mut self, mut regs: Vec<Value<'scope>>) {
        if self.spare_regs.len() < Self::SPARE_REGS {
            regs.clear();
            self.spare_regs.push(regs);
        }
    }
}

impl<'scope> Scope<'scope, '_> {
    pub fn call(&mut self, callee: Value<'scope>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let invoke = self.invoke;
        invoke.call(self, callee, this, args)
    }

    pub fn holding<R>(&mut self, held: &mut [Value<'scope>], f: impl FnOnce(&mut Self) -> R) -> (R, bool) {
        let mark = self.handles.len();
        let before = self.mc.generation();
        self.handles.extend_from_slice(held);
        self.gc_safe.set(true);
        let r = f(self);
        self.gc_safe.set(false);
        held.copy_from_slice(&self.handles[mark..mark + held.len()]);
        self.handles.truncate(mark);
        (r, self.mc.generation() != before)
    }

    pub fn call_safe(&mut self, callee: Value<'scope>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        self.gc_safe.set(true);
        let invoke = self.invoke;
        invoke.call(self, callee, this, args)
    }

    pub fn callback(&mut self, callee: Value<'scope>, this: Value<'scope>) -> Callback<'scope> {
        let invoke = self.invoke;
        invoke.callback(self, callee, this)
    }

    pub fn is_constructor(&self, v: Value<'scope>) -> bool {
        let Some(o) = v.as_object() else { return false };
        match &o.borrow().kind {
            crate::ObjectKind::Function(c) => self.invoke.constructs(c),
            crate::ObjectKind::Native(n) => n.constructor,
            crate::ObjectKind::NativeClosure(c) => c.constructor,
            crate::ObjectKind::Bound(b) => self.is_constructor(b.target),
            crate::ObjectKind::Proxy(p) => p.constructor,
            _ => false,
        }
    }

    pub fn proto_from_ctor(&mut self, new_target: Value<'scope>, fallback: JsObject<'scope>) -> Result<JsObject<'scope>, Error> {
        if new_target == Value::Undefined {
            return Ok(fallback);
        }
        match (self.get_named(new_target, "prototype")?).kind() {
            ValueKind::Object(p) => Ok(p),
            _ => Ok(fallback),
        }
    }

    pub fn create_from_ctor(&mut self, new_target: Value<'scope>, fallback: JsObject<'scope>, kind: crate::ObjectKind<'scope>) -> Result<JsObject<'scope>, Error> {
        let proto = self.proto_from_ctor(new_target, fallback)?;
        Ok(crate::ObjectData { kind, proto: Some(proto), ..crate::ObjectData::default() }.alloc(self.mc))
    }

    pub fn wrap_primitive(&mut self, new_target: Value<'scope>, v: Value<'scope>, fallback: JsObject<'scope>) -> Result<Value<'scope>, Error> {
        if new_target == Value::Undefined {
            return Ok(v);
        }
        Ok(self.create_from_ctor(new_target, fallback, crate::ObjectKind::Primitive(v))?.into())
    }

    pub fn eval_code(&mut self, src: &str, kind: CodeKind) -> Result<Value<'scope>, Error> {
        self.eval_text(src, kind, false)
    }

    pub fn eval_text(&mut self, src: &str, kind: CodeKind, hidden: bool) -> Result<Value<'scope>, Error> {
        let invoke = self.invoke;
        invoke.eval_code(self, src, kind, hidden)
    }

    pub fn resume(&mut self, coroutine: crate::JsObject<'scope>, mode: Resume, value: Value<'scope>) -> Result<Step<'scope>, Error> {
        let invoke = self.invoke;
        invoke.resume(self, coroutine, mode, value)
    }

    pub fn construct(&mut self, callee: Value<'scope>, args: &[Value<'scope>], new_target: Value<'scope>) -> Result<Value<'scope>, Error> {
        let invoke = self.invoke;
        invoke.construct(self, callee, args, new_target)
    }
}
