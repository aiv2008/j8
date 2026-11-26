use j8framework::ValueKind;
use alloc::boxed::Box;
use alloc::vec::Vec;

use j8framework::{Scope, Error, ErrorKind, HelperKind, HelperState, IterRecord, IteratorHelper, JsObject, Key, ObjectData, ObjectKind, Property, Slot, Value};

use crate::{Args, Installer};

pub(crate) struct Helpers;

const ITERATOR_PROTO: &str = "Cannot assign to this property of Iterator.prototype";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Probe {
    Some,
    Every,
    Find,
}

impl Helpers {
    fn is_iterator_ctor(v: Value<'_>) -> bool {
        matches!(v.kind(), ValueKind::Object(f) if matches!(f.borrow().kind, ObjectKind::Native(n) if core::ptr::fn_addr_eq(n.f, Self::iterator as j8framework::NativeFn)))
    }

    fn iterator<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let new_target = a.new_target;
        if new_target == Value::Undefined || Self::is_iterator_ctor(new_target) {
            return Err(a.throw(ErrorKind::Type, "Abstract class Iterator not directly constructable"));
        }
        Ok(a.create_from_ctor(new_target, a.context.iterator_proto, ObjectKind::Ordinary)?.into())
    }

    fn direct<'scope>(a: &mut Scope<'scope, '_>, o: Value<'scope>) -> Result<IterRecord<'scope>, Error> {
        let next = a.get_named(o, "next")?;
        Ok(IterRecord { iterator: o, next })
    }

    fn this_object<'scope>(a: &Scope<'scope, '_>, this: Value<'scope>) -> Result<Value<'scope>, Error> {
        match this.kind() {
            ValueKind::Object(_) => Ok(this),
            _ => Err(a.throw(ErrorKind::Type, "Iterator.prototype method called on a non-object")),
        }
    }

    fn reject<'scope>(a: &mut Scope<'scope, '_>, iterator: Value<'scope>, kind: ErrorKind, message: &str) -> Error {
        let e = a.throw(kind, message);
        a.iterator_close_abrupt(iterator, e)
    }

    fn callable<'scope>(a: &mut Scope<'scope, '_>, iterator: Value<'scope>, f: Value<'scope>) -> Result<(), Error> {
        if a.is_callable(f) {
            Ok(())
        } else {
            let t = a.describe(f);
            Err(Self::reject(a, iterator, ErrorKind::Type, &alloc::format!("{t} is not a function")))
        }
    }

    fn close_on<'scope, T>(a: &mut Scope<'scope, '_>, iterator: Value<'scope>, r: Result<T, Error>) -> Result<T, Error> {
        r.map_err(|e| a.iterator_close_abrupt(iterator, e))
    }

    fn make<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, kind: HelperKind, f: Value<'scope>, remaining: f64) -> Result<Value<'scope>, Error> {
        let underlying = Self::direct(a, this)?;
        let h = IteratorHelper { kind, state: HelperState::Start, underlying, f, remaining, counter: 0.0, inner: None };
        Ok(ObjectData { kind: ObjectKind::IteratorHelper(Box::new(h)), proto: Some(a.context.iterator_helper_proto), ..ObjectData::default() }.alloc(a.mc).into())
    }

    fn with_callback<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>], kind: HelperKind) -> Result<Value<'scope>, Error> {
        let this = Self::this_object(a, this)?;
        Self::callable(a, this, args.at(0))?;
        Self::make(a, this, kind, args.at(0), 0.0)
    }

    fn map<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::with_callback(a, this, args, HelperKind::Map)
    }

    fn filter<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::with_callback(a, this, args, HelperKind::Filter)
    }

    fn flat_map<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::with_callback(a, this, args, HelperKind::FlatMap)
    }

    fn limited<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>], kind: HelperKind) -> Result<Value<'scope>, Error> {
        let this = Self::this_object(a, this)?;
        let n = a.to_number(args.at(0));
        let n = Self::close_on(a, this, n)?;
        if n.is_nan() {
            return Err(Self::reject(a, this, ErrorKind::Range, "limit must be a number"));
        }
        let n = a.to_integer_or_infinity(n.into())?;
        if n < 0.0 {
            return Err(Self::reject(a, this, ErrorKind::Range, "limit must not be negative"));
        }
        Self::make(a, this, kind, Value::Undefined, n)
    }

    fn take<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::limited(a, this, args, HelperKind::Take)
    }

    fn drop<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::limited(a, this, args, HelperKind::Drop)
    }

    fn flattenable<'scope>(a: &mut Scope<'scope, '_>, v: Value<'scope>, strings: bool) -> Result<IterRecord<'scope>, Error> {
        match v.kind() {
            ValueKind::Object(_) => {}
            ValueKind::String(_) if strings => {}
            _ => {
                let t = a.describe(v);
                return Err(a.throw(ErrorKind::Type, &alloc::format!("{t} is not an object")));
            }
        }
        let method = a.get_symbol(v, a.context.symbols.iterator, v)?;
        let iterator = if method.is_nullish() { v } else { a.call(method, v, &[])? };
        if iterator.as_object().is_none() {
            return Err(a.throw(ErrorKind::Type, "the iterator is not an object"));
        }
        Self::direct(a, iterator)
    }

    fn step<'scope>(a: &mut Scope<'scope, '_>, h: &mut IteratorHelper<'scope>) -> Result<Option<Value<'scope>>, Error> {
        let under = h.underlying;
        loop {
            match h.kind {
                HelperKind::Take if h.remaining == 0.0 => {
                    a.iterator_close(under.iterator)?;
                    return Ok(None);
                }
                HelperKind::Drop => {
                    while h.remaining > 0.0 {
                        if h.remaining != f64::INFINITY {
                            h.remaining -= 1.0;
                        }
                        if a.iterator_step(under)?.is_none() {
                            return Ok(None);
                        }
                    }
                }
                HelperKind::FlatMap => {
                    if let Some(inner) = h.inner {
                        let r = a.iterator_step(inner);
                        match Self::close_on(a, under.iterator, r)? {
                            Some(v) => return Ok(Some(v)),
                            None => {
                                h.inner = None;
                                continue;
                            }
                        }
                    }
                }
                _ => {}
            }
            if h.kind == HelperKind::Take && h.remaining != f64::INFINITY {
                h.remaining -= 1.0;
            }
            let Some(v) = a.iterator_step(under)? else { return Ok(None) };
            let counter = h.counter;
            match h.kind {
                HelperKind::Take | HelperKind::Drop => return Ok(Some(v)),
                HelperKind::Map => {
                    let r = a.call(h.f, Value::Undefined, &[v, counter.into()]);
                    let mapped = Self::close_on(a, under.iterator, r)?;
                    h.counter += 1.0;
                    return Ok(Some(mapped));
                }
                HelperKind::Filter => {
                    let r = a.call(h.f, Value::Undefined, &[v, counter.into()]);
                    let selected = Self::close_on(a, under.iterator, r)?;
                    h.counter += 1.0;
                    if a.to_boolean(selected) {
                        return Ok(Some(v));
                    }
                }
                HelperKind::FlatMap => {
                    let r = a.call(h.f, Value::Undefined, &[v, counter.into()]).and_then(|m| Self::flattenable(a, m, false));
                    h.inner = Some(Self::close_on(a, under.iterator, r)?);
                    h.counter += 1.0;
                }
            }
        }
    }

    fn helper<'scope>(a: &Scope<'scope, '_>, this: Value<'scope>) -> Result<(JsObject<'scope>, IteratorHelper<'scope>), Error> {
        match this.kind() {
            ValueKind::Object(o) => match &o.borrow().kind {
                ObjectKind::IteratorHelper(h) => Ok((o, **h)),
                _ => Err(a.throw(ErrorKind::Type, "Iterator Helper method called on an incompatible receiver")),
            },
            _ => Err(a.throw(ErrorKind::Type, "Iterator Helper method called on an incompatible receiver")),
        }
    }

    fn store<'scope>(a: &Scope<'scope, '_>, o: JsObject<'scope>, h: IteratorHelper<'scope>) {
        if let ObjectKind::IteratorHelper(x) = &mut o.borrow_mut(a.mc).kind {
            **x = h;
        }
    }

    fn helper_next<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, mut h) = Self::helper(a, this)?;
        match h.state {
            HelperState::Running => return Err(a.throw(ErrorKind::Type, "Generator is already running")),
            HelperState::Done => return Ok(a.iter_result(Value::Undefined, true).into()),
            HelperState::Start | HelperState::Suspended => {}
        }
        Self::store(a, o, IteratorHelper { state: HelperState::Running, ..h });
        let r = Self::step(a, &mut h);
        h.state = if matches!(r, Ok(Some(_))) { HelperState::Suspended } else { HelperState::Done };
        Self::store(a, o, h);
        match r? {
            Some(v) => Ok(a.iter_result(v, false).into()),
            None => Ok(a.iter_result(Value::Undefined, true).into()),
        }
    }

    fn helper_return<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, h) = Self::helper(a, this)?;
        match h.state {
            HelperState::Running => return Err(a.throw(ErrorKind::Type, "Generator is already running")),
            HelperState::Done => return Ok(a.iter_result(Value::Undefined, true).into()),
            HelperState::Start | HelperState::Suspended => {}
        }
        Self::store(a, o, IteratorHelper { state: HelperState::Done, ..h });
        if let (HelperState::Suspended, Some(inner)) = (h.state, h.inner) {
            let r = a.iterator_close(inner.iterator);
            Self::close_on(a, h.underlying.iterator, r)?;
        }
        a.iterator_close(h.underlying.iterator)?;
        Ok(a.iter_result(Value::Undefined, true).into())
    }

    fn consumer<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, f: Value<'scope>) -> Result<IterRecord<'scope>, Error> {
        let this = Self::this_object(a, this)?;
        Self::callable(a, this, f)?;
        Self::direct(a, this)
    }

    fn reduce<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let f = args.at(0);
        let it = Self::consumer(a, this, f)?;
        let (mut acc, mut counter) = if args.len() >= 2 {
            (args[1], 0.0)
        } else {
            match a.iterator_step(it)? {
                Some(v) => (v, 1.0),
                None => return Err(a.throw(ErrorKind::Type, "Reduce of empty iterator with no initial value")),
            }
        };
        while let Some(v) = a.iterator_step(it)? {
            let r = a.call(f, Value::Undefined, &[acc, v, counter.into()]);
            acc = Self::close_on(a, it.iterator, r)?;
            counter += 1.0;
        }
        Ok(acc)
    }

    fn to_array<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let this = Self::this_object(a, this)?;
        let it = Self::direct(a, this)?;
        let mut out = Vec::new();
        while let Some(v) = a.iterator_step(it)? {
            out.push(v);
        }
        Ok(a.array(out).into())
    }

    fn for_each<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let f = args.at(0);
        let it = Self::consumer(a, this, f)?;
        let mut counter = 0.0;
        while let Some(v) = a.iterator_step(it)? {
            let r = a.call(f, Value::Undefined, &[v, Value::from(counter)]);
            Self::close_on(a, it.iterator, r)?;
            counter += 1.0;
        }
        Ok(Value::Undefined)
    }

    fn probe<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>], how: Probe) -> Result<Value<'scope>, Error> {
        let f = args.at(0);
        let it = Self::consumer(a, this, f)?;
        let mut counter = 0.0;
        while let Some(v) = a.iterator_step(it)? {
            let r = a.call(f, Value::Undefined, &[v, Value::from(counter)]);
            let hit = Self::close_on(a, it.iterator, r)?;
            let hit = a.to_boolean(hit);
            let out = match (how, hit) {
                (Probe::Some, true) => Some(Value::Bool(true)),
                (Probe::Every, false) => Some(Value::Bool(false)),
                (Probe::Find, true) => Some(v),
                _ => None,
            };
            if let Some(out) = out {
                a.iterator_close(it.iterator)?;
                return Ok(out);
            }
            counter += 1.0;
        }
        Ok(match how {
            Probe::Some => false.into(),
            Probe::Every => true.into(),
            Probe::Find => Value::Undefined,
        })
    }

    fn some<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::probe(a, this, args, Probe::Some)
    }

    fn every<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::probe(a, this, args, Probe::Every)
    }

    fn find<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::probe(a, this, args, Probe::Find)
    }

    fn from<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let rec = Self::flattenable(a, args.at(0), true)?;
        let inherits = match rec.iterator.kind() {
            ValueKind::Object(it) => {
                let mut cur = a.get_prototype_of(it)?;
                let mut found = false;
                while let Some(p) = cur {
                    if j8framework::ScopeLocal::ptr_eq(p, a.context.iterator_proto) {
                        found = true;
                        break;
                    }
                    cur = a.get_prototype_of(p)?;
                }
                found
            }
            _ => false,
        };
        if inherits {
            return Ok(rec.iterator);
        }
        Ok(ObjectData { kind: ObjectKind::WrappedIterator(rec), proto: Some(a.context.wrap_iterator_proto), ..ObjectData::default() }.alloc(a.mc).into())
    }

    fn wrapped<'scope>(a: &Scope<'scope, '_>, this: Value<'scope>) -> Result<IterRecord<'scope>, Error> {
        match this.kind() {
            ValueKind::Object(o) => match o.borrow().kind {
                ObjectKind::WrappedIterator(r) => Ok(r),
                _ => Err(a.throw(ErrorKind::Type, "method called on an incompatible receiver")),
            },
            _ => Err(a.throw(ErrorKind::Type, "method called on an incompatible receiver")),
        }
    }

    fn wrapped_next<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let r = Self::wrapped(a, this)?;
        a.call(r.next, r.iterator, &[])
    }

    fn wrapped_return<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let r = Self::wrapped(a, this)?;
        let ret = a.get_named(r.iterator, "return")?;
        if ret.is_nullish() {
            return Ok(a.iter_result(Value::Undefined, true).into());
        }
        a.call(ret, r.iterator, &[])
    }

    fn tag_get<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Ok(a.string("Iterator").into())
    }

    fn ctor_get<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Ok(a.context.global.borrow().own(a.mc, "Iterator").unwrap_or_default())
    }

    pub(crate) fn ignoring_setter<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, home: (JsObject<'scope>, &str), key: Key<'_, 'scope>, v: Value<'scope>) -> Result<Value<'scope>, Error> {
        let Some(o) = this.as_object() else {
            return Err(a.throw(ErrorKind::Type, "setter called on a non-object"));
        };
        if j8framework::ScopeLocal::ptr_eq(o, home.0) {
            return Err(a.throw(ErrorKind::Type, home.1));
        }
        if a.get_own_property(o, key)?.is_none() {
            a.create_data_property_or_throw(o, key, v)?;
        } else if !a.set_key(this, key, v, this)? {
            let k = a.key_text(key);
            return Err(a.throw(ErrorKind::Type, &alloc::format!("Cannot assign to read only property '{k}'")));
        }
        Ok(Value::Undefined)
    }

    fn tag_set<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::ignoring_setter(a, this, (a.context.iterator_proto, ITERATOR_PROTO), Key::Sym(a.context.symbols.to_string_tag), args.at(0))
    }

    fn ctor_set<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::ignoring_setter(a, this, (a.context.iterator_proto, ITERATOR_PROTO), Key::Str("constructor"), args.at(0))
    }
}

impl Installer<'_, '_> {
    pub(crate) fn helpers(&mut self) {
        let r = self.context;
        let proto = r.iterator_proto;
        let ctor = r.named_native(self.mc, Helpers::iterator, "Iterator", 0);
        if let ObjectKind::Native(n) = &mut ctor.borrow_mut(self.mc).kind {
            n.constructor = true;
        }
        ctor.borrow_mut(self.mc).define("prototype", Property { slot: Slot::Data(proto.into()), enumerable: false, writable: false, configurable: false });
        self.set(r.global, "Iterator", ctor.into());
        self.method(ctor, "from", Helpers::from, 1);
        let methods: [(&str, j8framework::NativeFn, u32); 11] = [
            ("map", Helpers::map, 1),
            ("filter", Helpers::filter, 1),
            ("take", Helpers::take, 1),
            ("drop", Helpers::drop, 1),
            ("flatMap", Helpers::flat_map, 1),
            ("reduce", Helpers::reduce, 1),
            ("toArray", Helpers::to_array, 0),
            ("forEach", Helpers::for_each, 1),
            ("some", Helpers::some, 1),
            ("every", Helpers::every, 1),
            ("find", Helpers::find, 1),
        ];
        for (name, f, n) in methods {
            self.method(proto, name, f, n);
        }
        let accessor = |get: j8framework::NativeFn, set: j8framework::NativeFn, name: &str| {
            let g = r.named_native(self.mc, get, &alloc::format!("get {name}"), 0);
            let s = r.named_native(self.mc, set, &alloc::format!("set {name}"), 1);
            Property { slot: Slot::Accessor { get: Some(g), set: Some(s) }, enumerable: false, writable: false, configurable: true }
        };
        let ctor_prop = accessor(Helpers::ctor_get, Helpers::ctor_set, "constructor");
        proto.borrow_mut(self.mc).define("constructor", ctor_prop);
        let tag_prop = accessor(Helpers::tag_get, Helpers::tag_set, "[Symbol.toStringTag]");
        proto.borrow_mut(self.mc).define_symbol(r.symbols.to_string_tag, tag_prop);
        self.method(r.iterator_helper_proto, "next", Helpers::helper_next, 0);
        self.method(r.iterator_helper_proto, "return", Helpers::helper_return, 0);
        self.method(r.wrap_iterator_proto, "next", Helpers::wrapped_next, 0);
        self.method(r.wrap_iterator_proto, "return", Helpers::wrapped_return, 0);
    }
}
