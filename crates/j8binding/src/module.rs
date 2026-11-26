use alloc::format;
use alloc::string::String;
use core::fmt::Write as _;

use j8framework::{Scope, JsObject, Property};

use crate::{Glue, Member, Object};

pub const NATIVE: &str = "__j8_native";

pub trait Install {
    fn install_into<'scope>(a: &mut Scope<'scope, '_>, target: JsObject<'scope>);
}

pub trait Module: Object {
    const SPECIFIERS: &'static [&'static str];
}

impl Glue {
    pub fn spawn<'scope>(a: &mut Scope<'scope, '_>, future: impl core::future::Future<Output = j8framework::Settle> + Send + 'static) -> Result<j8framework::Value<'scope>, j8framework::Error> {
        let promise = a.new_promise();
        let (resolve, reject) = a.promise_resolvers(promise);
        let job = j8framework::AsyncJob::new(a.global(resolve.into()), a.global(reject.into()), alloc::boxed::Box::pin(future));
        match a.spawn(job) {
            Ok(()) => Ok(promise.into()),
            Err(_) => Err(a.throw(j8framework::ErrorKind::Type, "async host functions need a runtime with an event loop (j8::Runtime::run_until_idle)")),
        }
    }

    pub fn settle<T: for<'g> crate::IntoJs<'g> + Send + 'static>(out: T) -> j8framework::Settle {
        alloc::boxed::Box::new(move |a: &mut Scope<'_, '_>| out.into_js(a))
    }

    #[doc(hidden)]
    pub fn natives<'scope>(a: &mut Scope<'scope, '_>) -> JsObject<'scope> {
        let global = a.context.global;
        if let Some(j8framework::Slot::Data(v)) = global.borrow().get_own(a.mc, NATIVE).map(|p| p.slot)
            && let Some(o) = v.as_object()
        {
            return o;
        }
        let o = a.object();
        global.borrow_mut(a.mc).define(NATIVE, Property::hidden(o.into()));
        o
    }

    #[doc(hidden)]
    pub fn register_module<M: Module>(a: &mut Scope<'_, '_>) {
        let registry = Self::natives(a);
        let m = <M as Object>::install(a, registry);
        for spec in M::SPECIFIERS {
            registry.borrow_mut(a.mc).define(spec, Property::hidden(m.into()));
        }
    }

    #[doc(hidden)]
    pub fn module_source<M: Module>(spec: &str) -> Option<String> {
        let canonical = M::SPECIFIERS.first()?;
        if !M::SPECIFIERS.contains(&spec) {
            return None;
        }
        let mut out = format!("const m = globalThis.{NATIVE}[\"{canonical}\"];\nexport default m;\n");
        for member in M::MEMBERS {
            let name = match member {
                Member::Method { name, .. } | Member::Static { name, .. } | Member::Accessor { name, .. } => name,
            };
            let _ = writeln!(out, "export const {name} = m.{name};");
        }
        Some(out)
    }
}
