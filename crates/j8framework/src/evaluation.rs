use alloc::vec::Vec;

use crate::{Error, HostReaction, JsObject, ModuleStatus, NativeClosure, ObjectData, ObjectKind, Reaction, Scope, Value};

struct Import;

#[expect(clippy::unnecessary_wraps, reason = "reactions share the ClosureFn signature; Result can't be dropped just because one cannot fail")]
impl Import {
    const NAMESPACE: usize = 0;
    const PROMISE: usize = 1;

    fn slot(me: JsObject<'_>, i: usize) -> Value<'_> {
        match &me.borrow().kind {
            ObjectKind::NativeClosure(c) => c.slots.get(i).copied().unwrap_or_default(),
            _ => Value::Undefined,
        }
    }

    fn fulfilled<'scope>(a: &mut Scope<'scope, '_>, me: JsObject<'scope>, _: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        if let Some(promise) = Self::slot(me, Self::PROMISE).as_object() {
            a.resolve_promise(promise, Self::slot(me, Self::NAMESPACE))?;
        }
        Ok(Value::Undefined)
    }

    fn rejected<'scope>(a: &mut Scope<'scope, '_>, me: JsObject<'scope>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        if let Some(promise) = Self::slot(me, Self::PROMISE).as_object() {
            a.reject_promise(promise, args.first().copied().unwrap_or_default());
        }
        Ok(Value::Undefined)
    }
}

impl<'scope> Scope<'scope, '_> {
    pub fn evaluate_module(&mut self, m: u32) -> Result<JsObject<'scope>, Error> {
        let module = match self.modules()[m as usize].status {
            ModuleStatus::EvaluatingAsync | ModuleStatus::Evaluated => self.modules()[m as usize].evaluation.cycle_root.unwrap_or(m),
            _ => m,
        };
        if let Some(promise) = self.modules()[module as usize].capability {
            return Ok(promise);
        }
        let promise = self.new_promise();
        self.update_module(module, |r| r.capability = Some(promise));
        let mut stack = Vec::new();
        let evaluated = self.inner_module_evaluation(module, &mut stack, 0);
        let Some(promise) = self.modules()[module as usize].capability else { unreachable!("the capability set above") };
        match evaluated {
            Ok(_) => {
                if self.modules()[module as usize].evaluation.order.is_none() {
                    self.resolve_promise(promise, Value::Undefined)?;
                }
            }
            Err(Error::Exception) => {
                let error = self.take_exception();
                for s in stack {
                    self.update_module(s, |r| {
                        r.status = ModuleStatus::Evaluated;
                        r.error = Some(error);
                    });
                }
                self.reject_promise(promise, error);
            }
            Err(e) => return Err(e),
        }
        Ok(promise)
    }

    pub fn import_evaluated(&mut self, evaluation: JsObject<'scope>, m: u32, promise: JsObject<'scope>) {
        let slots = alloc::vec![self.module_namespace(m).into(), promise.into()];
        let on_fulfilled = self.import_reaction(Import::fulfilled, slots.clone());
        let on_rejected = self.import_reaction(Import::rejected, slots);
        self.perform_then(evaluation, Reaction::Then { on_fulfilled: on_fulfilled.into(), on_rejected: on_rejected.into(), capability: None });
    }

    fn import_reaction(&self, f: crate::ClosureFn, slots: Vec<Value<'scope>>) -> JsObject<'scope> {
        ObjectData { kind: ObjectKind::NativeClosure(NativeClosure { f, slots, constructor: false }), proto: Some(self.context.function_proto), ..ObjectData::default() }.alloc(self.mc)
    }

    fn inner_module_evaluation(&mut self, m: u32, stack: &mut Vec<u32>, index: u32) -> Result<u32, Error> {
        let record = &self.modules()[m as usize];
        match record.status {
            ModuleStatus::EvaluatingAsync | ModuleStatus::Evaluated => return record.error.map_or(Ok(index), |e| Err(self.throw_value(e))),
            ModuleStatus::Evaluating => return Ok(index),
            ModuleStatus::Linked => {}
        }
        let requested = record.requested.clone();
        self.update_module(m, |r| {
            r.status = ModuleStatus::Evaluating;
            r.evaluation.dfs_index = index;
            r.evaluation.dfs_ancestor = index;
            r.evaluation.pending = 0;
        });
        let mut index = index + 1;
        stack.push(m);
        for required in requested {
            index = self.inner_module_evaluation(required, stack, index)?;
            let r = &self.modules()[required as usize];
            let dependency = if r.status == ModuleStatus::Evaluating {
                let ancestor = r.evaluation.dfs_ancestor;
                self.update_module(m, |r| r.evaluation.dfs_ancestor = r.evaluation.dfs_ancestor.min(ancestor));
                required
            } else {
                let root = r.evaluation.cycle_root.unwrap_or(required);
                if let Some(e) = self.modules()[root as usize].error {
                    return Err(self.throw_value(e));
                }
                root
            };
            if self.modules()[dependency as usize].evaluation.order.is_some() {
                self.update_module(m, |r| r.evaluation.pending += 1);
                self.update_module(dependency, |r| r.async_parents.push(m));
            }
        }
        let record = &self.modules()[m as usize];
        if record.evaluation.pending > 0 || record.info.top_level_await {
            let order = self.context.async_evaluation_order.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let pending = record.evaluation.pending;
            self.update_module(m, |r| r.evaluation.order = Some(order));
            if pending == 0 {
                self.execute_async_module(m)?;
            }
        } else {
            self.execute_module(m)?;
        }
        let record = &self.modules()[m as usize];
        if record.evaluation.dfs_ancestor == record.evaluation.dfs_index {
            while let Some(s) = stack.pop() {
                let status = if self.modules()[s as usize].evaluation.order.is_some() { ModuleStatus::EvaluatingAsync } else { ModuleStatus::Evaluated };
                self.update_module(s, |r| {
                    r.status = status;
                    r.evaluation.cycle_root = Some(m);
                });
                if s == m {
                    break;
                }
            }
        }
        Ok(index)
    }

    fn execute_module(&mut self, m: u32) -> Result<(), Error> {
        let function = self.modules()[m as usize].function;
        self.call_safe(function.into(), Value::Undefined, &[]).map(drop)
    }

    fn execute_async_module(&mut self, m: u32) -> Result<(), Error> {
        let function = self.modules()[m as usize].function;
        let done = self.call_safe(function.into(), Value::Undefined, &[])?;
        self.on_settled(
            done,
            HostReaction::new(move |a, value, rejected| {
                if rejected {
                    a.async_module_rejected(m, value);
                    return Ok(());
                }
                a.async_module_fulfilled(m)
            }),
        )
    }

    fn gather_available_ancestors(&self, m: u32, list: &mut Vec<u32>) {
        for parent in self.modules()[m as usize].async_parents.clone() {
            let record = &self.modules()[parent as usize];
            let root = record.evaluation.cycle_root.unwrap_or(parent);
            if list.contains(&parent) || self.modules()[root as usize].error.is_some() {
                continue;
            }
            let pending = record.evaluation.pending.saturating_sub(1);
            let top_level_await = record.info.top_level_await;
            self.update_module(parent, |r| r.evaluation.pending = pending);
            if pending == 0 {
                list.push(parent);
                if !top_level_await {
                    self.gather_available_ancestors(parent, list);
                }
            }
        }
    }

    fn async_module_fulfilled(&mut self, m: u32) -> Result<(), Error> {
        if self.modules()[m as usize].status == ModuleStatus::Evaluated {
            return Ok(());
        }
        self.module_evaluated(m)?;
        let mut list = Vec::new();
        self.gather_available_ancestors(m, &mut list);
        list.sort_by_key(|&x| self.modules()[x as usize].evaluation.order);
        for x in list {
            let record = &self.modules()[x as usize];
            if record.status == ModuleStatus::Evaluated {
                continue;
            }
            let executed = if record.info.top_level_await { self.execute_async_module(x) } else { self.execute_module(x).and_then(|()| self.module_evaluated(x)) };
            match executed {
                Ok(()) => {}
                Err(Error::Exception) => {
                    let error = self.take_exception();
                    self.async_module_rejected(x, error);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn module_evaluated(&mut self, m: u32) -> Result<(), Error> {
        self.update_module(m, |r| {
            r.evaluation.order = None;
            r.status = ModuleStatus::Evaluated;
        });
        match self.modules()[m as usize].capability {
            Some(promise) => self.resolve_promise(promise, Value::Undefined),
            None => Ok(()),
        }
    }

    fn async_module_rejected(&mut self, m: u32, error: Value<'scope>) {
        if self.modules()[m as usize].status == ModuleStatus::Evaluated {
            return;
        }
        self.update_module(m, |r| {
            r.error = Some(error);
            r.status = ModuleStatus::Evaluated;
        });
        if let Some(promise) = self.modules()[m as usize].capability {
            self.reject_promise(promise, error);
        }
        for parent in self.modules()[m as usize].async_parents.clone() {
            self.async_module_rejected(parent, error);
        }
    }
}
