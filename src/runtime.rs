use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::time::Duration;

use j8executor::{EventLoop, EventLoopSet, Executor};
use j8framework::{Scope, Error, ErrorKind, HeapLimitCallback, Platform, Pool, ScopeGlobal, ScriptKind, Source, Task, Value, ValueBrand};

pub trait Host {
    fn install(a: &mut Scope<'_, '_>);

    fn module_source(specifier: &str) -> Option<String>;

    fn preludes(out: &mut Vec<(&'static str, &'static str)>);
}

pub struct HostPlatform<H, P> {
    inner: P,
    _host: PhantomData<fn() -> H>,
}

impl<H: Host, P: Platform> HostPlatform<H, P> {
    pub fn new(inner: P) -> Self {
        Self { inner, _host: PhantomData }
    }

    pub fn inner(&self) -> &P {
        &self.inner
    }
}

impl<H, P: Pool> Pool for HostPlatform<H, P> {
    fn threads(&self) -> usize {
        self.inner.threads()
    }

    fn run(&self, jobs: Vec<Task<'_>>) {
        self.inner.run(jobs);
    }
}

impl<H: Host, P: Platform> Platform for HostPlatform<H, P> {
    fn now(&self) -> Duration {
        self.inner.now()
    }

    fn wall_clock(&self) -> Duration {
        self.inner.wall_clock()
    }

    fn load_module(&self, referrer: &str, specifier: &str) -> Option<(String, String)> {
        match H::module_source(specifier) {
            Some(text) => Some((specifier.to_owned(), text)),
            None => self.inner.load_module(referrer, specifier),
        }
    }

    fn locale(&self) -> Option<String> {
        self.inner.locale()
    }

    fn time_zone(&self) -> Option<String> {
        self.inner.time_zone()
    }

    fn print(&self, line: &str) {
        self.inner.print(line);
    }

    fn unhandled_rejection<'scope>(&self, scope: &mut Scope<'scope, '_>, promise: j8framework::JsObject<'scope>, reason: Value<'scope>) {
        self.inner.unhandled_rejection(scope, promise, reason);
    }
}

pub struct Runtime<'p> {
    js: EventLoop<'p>,
}

impl<'p> Runtime<'p> {
    pub fn on<H: Host>(set: &EventLoopSet<'_, 'p>, executor: Executor<'p>) -> Result<Self, Error> {
        let rt = Self { js: set.spawn(executor, &j8builtins::Builtins) };
        rt.enter(|a| H::install(a));
        let mut preludes = Vec::new();
        H::preludes(&mut preludes);
        for (name, text) in preludes {
            rt.js.eval(&Source { text, file: name }, ScriptKind::Shared)?;
        }
        Ok(rt)
    }

    pub fn eval(&self, text: &str, file: &str) -> Result<ScopeGlobal<'p, ValueBrand>, Error> {
        self.js.eval(&Source { text, file }, ScriptKind::Classic)
    }

    pub fn eval_module(&self, text: &str, file: &str) -> Result<ScopeGlobal<'p, ValueBrand>, Error> {
        self.js.eval(&Source { text, file }, ScriptKind::Module)
    }

    pub fn enter<R: Send>(&self, f: impl for<'scope> FnOnce(&mut Scope<'scope, '_>) -> R + Send) -> R {
        self.js.enter(f)
    }

    pub fn text(&self, g: &ScopeGlobal<'p, ValueBrand>) -> String {
        self.enter(|a| {
            let v = a.local(g);
            a.to_string(v).map_or_else(|_| String::from("<ToString threw>"), |s| String::from(&**s))
        })
    }

    pub fn exception_text(&self) -> String {
        self.enter(|a| {
            let v = a.take_exception();
            let text = a.stack_text(v).map(|s| String::from(&**s));
            text.unwrap_or_else(|_| {
                let _ = a.take_exception();
                String::from("<ToString threw>")
            })
        })
    }

    pub fn set<V: for<'g> crate::IntoJs<'g> + Send>(&self, path: &str, value: V) -> Result<(), Error> {
        self.enter(|a| {
            let (holder, key) = path.rsplit_once('.').unwrap_or(("", path));
            let mut target: Value<'_> = a.context.global.into();
            for part in holder.split('.').filter(|p| !p.is_empty()) {
                target = a.get_named(target, part)?;
            }
            let Some(o) = target.as_object() else { return Err(a.throw(ErrorKind::Type, &format!("{holder} is not an object"))) };
            let v = value.into_js(a)?;
            o.borrow_mut(a.mc).define(key, j8framework::Property::data(v));
            Ok(())
        })
    }

    pub fn run_until_idle(&self) -> Result<(), Error> {
        self.js.run()
    }

    pub fn interrupter(&self) -> j8executor::Interrupter<'p> {
        self.js.interrupter()
    }

    pub fn set_heap_limit(&self, limit: Option<NonZeroUsize>, callback: Option<HeapLimitCallback<'p>>) {
        self.js.set_heap_limit(limit, callback);
    }

    pub fn heap_used(&self) -> usize {
        self.js.heap_used()
    }
}

#[macro_export]
macro_rules! runtime {
    (
        $(#[$meta:meta])* $vis:vis $name:ident $(: $parent:ty)? {
            $(globals: [$($global:ty),* $(,)?],)?
            $(modules: [$($module:ty),* $(,)?],)?
            $(natives: [$($native:ty),* $(,)?],)?
            $(prelude: $prelude:expr,)?
        }
    ) => {
        $(#[$meta])*
        $vis struct $name;

        impl $crate::Host for $name {
            fn install(a: &mut $crate::frame::Scope<'_, '_>) {
                $(<$parent as $crate::Host>::install(a);)?
                let global = a.context.global;
                $($(<$global as $crate::Install>::install_into(a, global);)*)?
                $($($crate::Glue::register_module::<$module>(a);)*)?
                $(
                    let natives = $crate::Glue::natives(a);
                    $(<$native as $crate::Install>::install_into(a, natives);)*
                )?
                let _ = global;
            }

            fn module_source(specifier: &str) -> ::core::option::Option<::std::string::String> {
                $($(
                    if let ::core::option::Option::Some(s) = $crate::Glue::module_source::<$module>(specifier) {
                        return ::core::option::Option::Some(s);
                    }
                )*)?
                $(
                    if let ::core::option::Option::Some(s) = <$parent as $crate::Host>::module_source(specifier) {
                        return ::core::option::Option::Some(s);
                    }
                )?
                let _ = specifier;
                ::core::option::Option::None
            }

            fn preludes(out: &mut ::std::vec::Vec<(&'static str, &'static str)>) {
                $(<$parent as $crate::Host>::preludes(out);)?
                $(out.push((::core::concat!("[", ::core::stringify!($name), " prelude]"), $prelude));)?
                let _ = out;
            }
        }

        impl $name {
            pub fn platform<P: $crate::frame::Platform>(inner: P) -> $crate::HostPlatform<$name, P> {
                $crate::HostPlatform::new(inner)
            }

            pub fn runtime<'p>(set: &$crate::EventLoopSet<'_, 'p>, executor: $crate::executor::Executor<'p>) -> ::core::result::Result<$crate::Runtime<'p>, $crate::frame::Error> {
                $crate::Runtime::on::<$name>(set, executor)
            }
        }
    };
}
