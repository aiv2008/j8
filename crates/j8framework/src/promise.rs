use crate::ValueKind;
use alloc::vec::Vec;

use crate::ScopeLocal;

use crate::{Scope, Capability, Error, ErrorKind, HostReaction, Job, JsObject, ObjectData, ObjectKind, Reaction, Request, Resume, Settled, Static, Step, Value};

impl<'scope> Scope<'scope, '_> {
    pub fn new_promise(&self) -> JsObject<'scope> {
        ObjectData { kind: ObjectKind::Promise { state: Settled::Pending, reactions: Vec::new(), handled: false }, proto: Some(self.context.promise_proto), ..ObjectData::default() }.alloc(self.mc)
    }

    pub fn is_promise(v: Value<'scope>) -> bool {
        matches!(v.kind(), ValueKind::Object(o) if matches!(o.borrow().kind, ObjectKind::Promise { .. }))
    }

    pub fn promise_state(v: Value<'scope>) -> Option<Settled<'scope>> {
        match &v.as_object()?.borrow().kind {
            ObjectKind::Promise { state, .. } => Some(*state),
            _ => None,
        }
    }

    pub fn promise_resolvers(&self, promise: JsObject<'scope>) -> (JsObject<'scope>, JsObject<'scope>) {
        let done = self.mc.alloc(core::sync::atomic::AtomicBool::new(false));
        let make = |reject| {
            let f = ObjectData { kind: ObjectKind::Resolver { promise, reject, done }, proto: Some(self.context.function_proto), ..ObjectData::default() }.alloc(self.mc);
            self.context.name_function(self.mc, f, "", Some(1));
            f
        };
        (make(false), make(true))
    }

    pub fn run_resolver(&mut self, f: JsObject<'scope>, value: Value<'scope>) -> Result<Value<'scope>, Error> {
        let ObjectKind::Resolver { promise, reject, done } = f.borrow().kind else { unreachable!("a resolver function") };
        if !done.swap(true, core::sync::atomic::Ordering::AcqRel) {
            if reject {
                self.reject_promise(promise, value);
            } else {
                self.resolve_promise(promise, value)?;
            }
        }
        Ok(Value::Undefined)
    }

    pub fn enqueue_job(&self, job: Job<'scope>) {
        self.context.jobs.push(self.mc, job);
    }

    fn settle(&mut self, promise: JsObject<'scope>, value: Value<'scope>, rejected: bool) {
        let (reactions, handled) = {
            let mut d = promise.borrow_mut(self.mc);
            let ObjectKind::Promise { state, reactions, handled } = &mut d.kind else { return };
            if !matches!(state, Settled::Pending) {
                return;
            }
            *state = if rejected { Settled::Rejected(value) } else { Settled::Fulfilled(value) };
            (core::mem::take(reactions), *handled)
        };
        if rejected && !handled {
            self.context.rejections.push(self.mc, promise);
        }
        for reaction in reactions {
            self.enqueue_job(Job::Reaction { reaction, value, rejected });
        }
    }

    pub fn set_promise_handled(&self, promise: JsObject<'scope>, handled: bool) {
        if let ObjectKind::Promise { handled: h, .. } = &mut promise.borrow_mut(self.mc).kind {
            *h = handled;
        }
    }

    pub fn reject_promise(&mut self, promise: JsObject<'scope>, reason: Value<'scope>) {
        self.settle(promise, reason, true);
    }

    pub fn resolve_promise(&mut self, promise: JsObject<'scope>, value: Value<'scope>) -> Result<(), Error> {
        if let Some(o) = value.as_object() {
            if ScopeLocal::ptr_eq(o, promise) {
                let e = self.error(ErrorKind::Type, "Chaining cycle detected for promise");
                self.reject_promise(promise, e.into());
                return Ok(());
            }
            let then = match self.get_named(value, "then") {
                Ok(t) => t,
                Err(Error::Exception) => {
                    let reason = self.take_exception();
                    self.reject_promise(promise, reason);
                    return Ok(());
                }
                Err(e) => return Err(e),
            };
            if self.is_callable(then) {
                self.enqueue_job(Job::Thenable { promise, thenable: value, then });
                return Ok(());
            }
        }
        self.settle(promise, value, false);
        Ok(())
    }

    pub fn perform_then(&mut self, promise: JsObject<'scope>, reaction: Reaction<'scope>) {
        let settled = {
            let mut d = promise.borrow_mut(self.mc);
            let ObjectKind::Promise { state, reactions, handled } = &mut d.kind else { return };
            *handled = true;
            match *state {
                Settled::Pending => {
                    reactions.push(reaction);
                    None
                }
                Settled::Fulfilled(v) => Some((v, false)),
                Settled::Rejected(v) => Some((v, true)),
            }
        };
        if let Some((value, rejected)) = settled {
            self.enqueue_job(Job::Reaction { reaction, value, rejected });
        }
    }

    pub fn on_settled(&mut self, v: Value<'scope>, reaction: HostReaction) -> Result<(), Error> {
        let p = self.promise_resolve(v)?;
        let hook = self.mc.alloc(Static(j8lockfree::AtomicRefCell::new(Some(reaction))));
        self.perform_then(p, Reaction::Host { hook });
        Ok(())
    }

    pub fn promise_resolve(&mut self, v: Value<'scope>) -> Result<JsObject<'scope>, Error> {
        if let Some(o) = v.as_object()
            && Self::is_promise(v)
            && matches!((self.get_named(v, "constructor")?).kind(), ValueKind::Object(c) if ScopeLocal::ptr_eq(c, self.context.promise_ctor))
        {
            return Ok(o);
        }
        let p = self.new_promise();
        self.resolve_promise(p, v)?;
        Ok(p)
    }

    pub fn drive(&mut self, coroutine: JsObject<'scope>, mode: Resume, value: Value<'scope>) -> Result<(), Error> {
        let (promise, async_generator) = match &coroutine.borrow().kind {
            ObjectKind::Coroutine(c) => (c.promise, c.requests.is_some()),
            _ => (None, false),
        };
        if async_generator {
            return self.drive_async_generator(coroutine, mode, value);
        }
        let step = self.resume(coroutine, mode, value);
        self.driven(promise, step)
    }

    pub fn driven(&mut self, promise: Option<JsObject<'scope>>, step: Result<Step<'scope>, Error>) -> Result<(), Error> {
        match step {
            Ok(_) => Ok(()),
            Err(Error::Exception) => {
                let reason = self.take_exception();
                if let Some(p) = promise {
                    self.reject_promise(p, reason);
                }
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    pub fn async_generator_enqueue(&mut self, coroutine: JsObject<'scope>, mode: Resume, value: Value<'scope>) -> Result<JsObject<'scope>, Error> {
        let promise = self.new_promise();
        let idle = {
            let mut d = coroutine.borrow_mut(self.mc);
            let ObjectKind::Coroutine(c) = &mut d.kind else { unreachable!("an async generator") };
            let Some(q) = &mut c.requests else { unreachable!("an async generator") };
            q.push_back(Request { mode, value, promise });
            q.len() == 1
        };
        if idle {
            self.drive_async_generator(coroutine, mode, value)?;
        }
        Ok(promise)
    }

    fn settle_request(&mut self, coroutine: JsObject<'scope>, outcome: Result<(Value<'scope>, bool), Value<'scope>>) -> Result<(), Error> {
        let (done_req, next) = {
            let mut d = coroutine.borrow_mut(self.mc);
            let ObjectKind::Coroutine(c) = &mut d.kind else { unreachable!("an async generator") };
            let Some(q) = &mut c.requests else { unreachable!("an async generator") };
            (q.pop_front(), q.front().copied())
        };
        if let Some(req) = done_req {
            match outcome {
                Ok((v, done)) => {
                    let r = self.iter_result(v, done);
                    self.resolve_promise(req.promise, r.into())?;
                }
                Err(reason) => self.reject_promise(req.promise, reason),
            }
        }
        match next {
            Some(n) => self.drive_async_generator(coroutine, n.mode, n.value),
            None => Ok(()),
        }
    }

    fn drive_async_generator(&mut self, coroutine: JsObject<'scope>, mode: Resume, value: Value<'scope>) -> Result<(), Error> {
        match self.resume(coroutine, mode, value) {
            Ok(Step::Await(v)) => match self.promise_resolve(v) {
                Ok(p) => {
                    self.perform_then(p, Reaction::Await { coroutine });
                    Ok(())
                }
                Err(Error::Exception) => {
                    let reason = self.take_exception();
                    self.drive_async_generator(coroutine, Resume::Throw, reason)
                }
                Err(e) => Err(e),
            },
            Ok(Step::Yield(v)) => self.settle_request(coroutine, Ok((v, false))),
            Ok(Step::Done(v)) => self.settle_request(coroutine, Ok((v, true))),
            Err(Error::Exception) => {
                let reason = self.take_exception();
                self.settle_request(coroutine, Err(reason))
            }
            Err(e) => Err(e),
        }
    }

    pub fn run_job(&mut self, job: Job<'scope>) -> Result<(), Error> {
        match job {
            Job::Reaction { reaction: Reaction::Await { coroutine }, value, rejected } => self.drive(coroutine, if rejected { Resume::Throw } else { Resume::Next }, value),
            Job::Reaction { reaction: Reaction::Host { hook }, value, rejected } => {
                match hook.0.try_borrow_mut().and_then(|mut h| h.take()) {
                    Some(reaction) => (reaction.0)(self, value, rejected),
                    None => Ok(()),
                }
            }
            Job::Reaction { reaction: Reaction::Then { on_fulfilled, on_rejected, capability }, value, rejected } => {
                let handler = if rejected { on_rejected } else { on_fulfilled };
                let outcome = if self.is_callable(handler) {
                    self.call(handler, Value::Undefined, &[value])
                } else if rejected {
                    Err(self.throw_value(value))
                } else {
                    Ok(value)
                };
                self.reacted(capability, outcome)
            }
            Job::Import { module, promise } => {
                let held = self.global(promise.into());
                let evaluation = self.evaluate_module(module)?;
                let Some(promise) = self.local(&held).as_object() else { unreachable!("an import promise") };
                self.import_evaluated(evaluation, module, promise);
                Ok(())
            }
            Job::Thenable { promise, thenable, then } => {
                let (resolve, reject) = self.promise_resolvers(promise);
                match self.call(then, thenable, &[resolve.into(), reject.into()]) {
                    Ok(_) => Ok(()),
                    Err(Error::Exception) => {
                        let reason = self.take_exception();
                        self.run_resolver(reject, reason).map(drop)
                    }
                    Err(e) => Err(e),
                }
            }
        }
    }

    pub fn reacted(&mut self, capability: Option<Capability<'scope>>, outcome: Result<Value<'scope>, Error>) -> Result<(), Error> {
        let outcome = match outcome {
            Ok(r) => Ok(r),
            Err(Error::Exception) => Err(self.take_exception()),
            Err(e) => return Err(e),
        };
        let Some(cap) = capability else { return Ok(()) };
        let r = match outcome {
            Ok(r) => self.call(cap.resolve, Value::Undefined, &[r]),
            Err(reason) => self.call(cap.reject, Value::Undefined, &[reason]),
        };
        match r {
            Ok(_) => Ok(()),
            Err(Error::Exception) => {
                self.take_exception();
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    pub fn next_job(&self) -> Option<Job<'scope>> {
        self.context.jobs.pop(self.mc)
    }

    pub fn run_jobs(&mut self) -> Result<(), Error> {
        loop {
            match self.next_job() {
                Some(j) => self.run_job(j)?,
                None if self.report_rejections() => {}
                None => return Ok(()),
            }
        }
    }

    fn report_rejections(&mut self) -> bool {
        let mut reported = false;
        let platform = self.platform;
        while let Some(promise) = self.context.rejections.pop(self.mc) {
            let ObjectKind::Promise { state: Settled::Rejected(reason), handled: false, .. } = promise.borrow().kind else { continue };
            platform.unhandled_rejection(self, promise, reason);
            reported = true;
        }
        reported
    }
}
