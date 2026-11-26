#![expect(clippy::unnecessary_wraps, reason = "host functions all share the NativeFn signature; Result can't be dropped just because one cannot fail")]
extern crate alloc;

macro_rules! locale_method {
    ($name:ident, $intl:path, $plain:path) => {
        #[cfg(feature = "intl")]
        fn $name<'scope>(a: &mut j8framework::Scope<'scope, '_>, this: j8framework::Value<'scope>, args: &[j8framework::Value<'scope>]) -> Result<j8framework::Value<'scope>, j8framework::Error> {
            $intl(a, this, args)
        }

        #[cfg(not(feature = "intl"))]
        fn $name<'scope>(a: &mut j8framework::Scope<'scope, '_>, this: j8framework::Value<'scope>, args: &[j8framework::Value<'scope>]) -> Result<j8framework::Value<'scope>, j8framework::Error> {
            $plain(a, this, args)
        }
    };
}

mod array;
mod asyncgen;
mod bigint;
mod buffer;
mod collections;
mod date;
mod dfa;
mod error;
mod function;
mod global;
mod helpers;
#[cfg(feature = "intl")]
mod intl;
mod iter;
mod json;
mod math;
mod object;
mod promise;
mod reflect;
mod regexp;
mod string;
mod text;
mod typed;
mod weak;

use j8framework::{Mutation};
use j8framework::{Error, Intrinsics, JsObject, Context, Scope, Value};

#[cfg(feature = "intl")]
const LOCALE_ARGS: usize = 2;

#[cfg(not(feature = "intl"))]
const LOCALE_ARGS: usize = 0;

#[derive(Debug, Default, Clone, Copy)]
pub struct Builtins;

impl Intrinsics for Builtins {
    fn install<'scope>(&self, mc: &Mutation<'scope>, context: &Context<'scope>) {
        let mut b = Installer { mc, context };
        b.global();
        b.object();
        b.functions();
        b.function();
        b.array();
        b.string();
        b.number();
        b.math();
        b.errors();
        b.iterators();
        b.helpers();
        b.promise();
        b.bigint();
        b.regexp();
        b.collections();
        b.async_generators();
        b.json();
        b.buffers();
        b.date();
        b.texts();
        b.weaks();
        b.typed_arrays();
        b.reflect();
        #[cfg(feature = "intl")]
        b.intl();
        b.tags();
    }
}

struct Installer<'r, 'scope> {
    mc: &'r Mutation<'scope>,
    context: &'r Context<'scope>,
}

impl<'scope> Installer<'_, 'scope> {
    fn set(&self, target: JsObject<'scope>, name: &str, v: Value<'scope>) {
        target.borrow_mut(self.mc).define(name, j8framework::Property::hidden(v));
    }

    fn method(&self, target: JsObject<'scope>, name: &str, f: j8framework::NativeFn, n: u32) {
        self.context.method(self.mc, target, name, f, n);
    }

    fn direct(&self, f: j8framework::NativeFn, call: j8framework::FastCall, out: j8framework::FastOut) {
        self.context.add_fast(self.mc, f, call, out);
    }

    fn direct_push(&self, f: j8framework::NativeFn, call: j8framework::FastCall, out: j8framework::FastOut) {
        self.context.add_fast_inline(self.mc, f, call, out);
    }

    fn constructor(&self, name: &str, f: j8framework::NativeFn, proto: JsObject<'scope>, n: u32) -> JsObject<'scope> {
        let ctor = self.context.named_native(self.mc, f, name, n);
        if let j8framework::ObjectKind::Native(native) = &mut ctor.borrow_mut(self.mc).kind {
            native.constructor = true;
        }
        self.install_constructor(ctor, name, proto)
    }

    fn adopt_constructor(&self, ctor: JsObject<'scope>, name: &str, f: j8framework::NativeFn, proto: JsObject<'scope>, n: u32) -> JsObject<'scope> {
        ctor.borrow_mut(self.mc).kind = j8framework::ObjectKind::Native(j8framework::Native::constructor(f));
        self.context.name_function(self.mc, ctor, name, Some(n));
        self.install_constructor(ctor, name, proto)
    }

    fn install_constructor(&self, ctor: JsObject<'scope>, name: &str, proto: JsObject<'scope>) -> JsObject<'scope> {
        ctor.borrow_mut(self.mc).define("prototype", j8framework::Property { slot: j8framework::Slot::Data(proto.into()), enumerable: false, writable: false, configurable: false });
        self.set(proto, "constructor", ctor.into());
        self.set(self.context.global, name, ctor.into());
        ctor
    }

    fn tags(&self) {
        let r = self.context;
        let global = |name: &str| r.global.borrow().own(self.mc, name).and_then(j8framework::Value::as_object);
        let mut tagged = alloc::vec![
            (r.symbol_proto, "Symbol"),
            (r.bigint_proto, "BigInt"),
            (r.map_proto, "Map"),
            (r.set_proto, "Set"),
            (r.weakmap_proto, "WeakMap"),
            (r.weakset_proto, "WeakSet"),
            (r.promise_proto, "Promise"),
            (r.generator_proto, "Generator"),
            (r.async_generator_proto, "AsyncGenerator"),
            (r.generator_function_proto, "GeneratorFunction"),
            (r.async_function_proto, "AsyncFunction"),
            (r.async_generator_function_proto, "AsyncGeneratorFunction"),
            (r.array_iterator_proto, "Array Iterator"),
            (r.collection_iterator_proto, "Map Iterator"),
            (r.set_iterator_proto, "Set Iterator"),
            (r.iterator_helper_proto, "Iterator Helper"),
            (r.regexp_string_iterator_proto, "RegExp String Iterator"),
            (r.string_iterator_proto, "String Iterator"),
        ];
        for name in ["Math", "JSON"] {
            if let Some(o) = global(name) {
                tagged.push((o, name));
            }
        }
        for (o, tag) in tagged {
            let v = self.text(tag);
            o.borrow_mut(self.mc).define_symbol(r.symbols.to_string_tag, j8framework::Property { slot: j8framework::Slot::Data(v), enumerable: false, writable: false, configurable: true });
        }
    }

    fn not_constructor(&self, f: JsObject<'scope>) {
        if let j8framework::ObjectKind::Native(native) = &mut f.borrow_mut(self.mc).kind {
            native.constructor = false;
        }
    }

    fn getter(&self, target: JsObject<'scope>, name: &str, f: j8framework::NativeFn) {
        let g = self.context.named_native(self.mc, f, &alloc::format!("get {name}"), 0);
        target.borrow_mut(self.mc).define(name, j8framework::Property { slot: j8framework::Slot::Accessor { get: Some(g), set: None }, enumerable: false, writable: false, configurable: true });
    }

    fn text(&self, s: &str) -> Value<'scope> {
        Value::String(j8framework::JsStr::alloc(self.mc, s))
    }

    fn plain(&self) -> JsObject<'scope> {
        j8framework::ObjectData { proto: Some(self.context.object_proto), ..j8framework::ObjectData::default() }.alloc(self.mc)
    }
}

struct LocaleJoin;

impl LocaleJoin {
    fn join<'scope>(a: &mut Scope<'scope, '_>, o: JsObject<'scope>, len: f64, args: &[Value<'scope>]) -> Result<alloc::string::String, Error> {
        let passed: alloc::vec::Vec<Value<'scope>> = (0..LOCALE_ARGS).map(|i| args.at(i)).collect();
        let mut out = alloc::string::String::new();
        let mut k = 0.0;
        while k < len {
            if k > 0.0 {
                out.push(',');
            }
            let v = a.get_index(o.into(), k)?;
            if !v.is_nullish() {
                let f = a.get_named(v, "toLocaleString")?;
                let s = a.call(f, v, &passed)?;
                out.push_str(&a.to_string(s)?);
            }
            k += 1.0;
        }
        Ok(out)
    }
}

trait Args<'scope> {
    fn at(&self, i: usize) -> Value<'scope>;
}

impl<'scope> Args<'scope> for [Value<'scope>] {
    fn at(&self, i: usize) -> Value<'scope> {
        self.get(i).copied().unwrap_or_default()
    }
}
