use j8framework::ValueKind;
use alloc::boxed::Box;
use alloc::vec::Vec;

use j8framework::{Scope, Bound, Descriptor, Error, ErrorKind, Integrity, IterKind, JsObject, Key, ObjectData, ObjectKind, Property, PropertyKey, Slot, Value};

use crate::{Args, Installer};

pub(crate) struct Objects;

impl Objects {
    fn is_object_ctor(v: Value<'_>) -> bool {
        matches!(v.kind(), ValueKind::Object(f) if matches!(f.borrow().kind, ObjectKind::Native(n) if core::ptr::fn_addr_eq(n.f, Self::object as j8framework::NativeFn)))
    }

    fn object<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let new_target = a.new_target;
        if new_target != Value::Undefined && !Self::is_object_ctor(new_target) {
            return Ok(a.create_from_ctor(new_target, a.context.object_proto, ObjectKind::Ordinary)?.into());
        }
        Ok(match args.at(0).kind() {
            ValueKind::Undefined | ValueKind::Null | ValueKind::Uninit => a.object().into(),
            v => a.to_object(Value::from(v))?.into(),
        })
    }

    fn require_object<'scope>(a: &mut Scope<'scope, '_>, v: Value<'scope>, method: &str) -> Result<JsObject<'scope>, Error> {
        match v.kind() {
            ValueKind::Object(o) => Ok(o),
            _ => Err(a.throw(ErrorKind::Type, &alloc::format!("Object.{method} called on non-object"))),
        }
    }

    fn enumerable_own<'scope>(a: &mut Scope<'scope, '_>, v: Value<'scope>, kind: IterKind) -> Result<Value<'scope>, Error> {
        let o = a.to_object(v)?;
        let mut out = Vec::new();
        for k in a.own_property_keys(o)? {
            let PropertyKey::String(s) = k else { continue };
            if !a.get_own_property(o, Key::Str(&s))?.is_some_and(|p| p.enumerable) {
                continue;
            }
            out.push(match kind {
                IterKind::Keys => s.into(),
                IterKind::Values => a.get_named(o.into(), &s)?,
                IterKind::Entries => {
                    let v = a.get_named(o.into(), &s)?;
                    a.array(alloc::vec![s.into(), v]).into()
                }
            });
        }
        Ok(a.array(out).into())
    }

    fn keys<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::enumerable_own(a, args.at(0), IterKind::Keys)
    }

    fn values<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::enumerable_own(a, args.at(0), IterKind::Values)
    }

    fn entries<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::enumerable_own(a, args.at(0), IterKind::Entries)
    }

    fn assign<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let to = a.to_object(args.at(0))?;
        for &src in args.iter().skip(1).filter(|v| !v.is_nullish()) {
            let from = a.to_object(src)?;
            for k in a.own_property_keys(from)? {
                let key = k.key();
                if a.get_own_property(from, key)?.is_some_and(|p| p.enumerable) {
                    let v = a.get_key(from.into(), key, from.into())?;
                    if !a.set_key(to.into(), key, v, to.into())? {
                        let t = a.key_text(key);
                        return Err(a.throw(ErrorKind::Type, &alloc::format!("Cannot assign to read only property '{t}' of object")));
                    }
                }
            }
        }
        Ok(to.into())
    }

    fn get_prototype_of<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let o = a.to_object(args.at(0))?;
        Ok(a.get_prototype_of(o)?.map_or(Value::Null, Value::Object))
    }

    fn proto_arg<'scope>(a: &mut Scope<'scope, '_>, v: Value<'scope>) -> Result<Option<JsObject<'scope>>, Error> {
        match v.kind() {
            ValueKind::Object(o) => Ok(Some(o)),
            ValueKind::Null => Ok(None),
            other => {
                let t = a.describe(Value::from(other));
                Err(a.throw(ErrorKind::Type, &alloc::format!("Object prototype may only be an Object or null: {t}")))
            }
        }
    }

    fn set_prototype_of<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let target = args.at(0);
        if target.is_nullish() {
            return Err(a.throw(ErrorKind::Type, "Object.setPrototypeOf called on null or undefined"));
        }
        let proto = Self::proto_arg(a, args.at(1))?;
        let Some(o) = target.as_object() else { return Ok(target) };
        if !a.set_prototype_of(o, proto)? {
            return Err(a.throw(ErrorKind::Type, "Cannot set prototype: object is not extensible or the chain would be cyclic"));
        }
        Ok(target)
    }

    fn define_properties_on<'scope>(a: &mut Scope<'scope, '_>, o: JsObject<'scope>, props: Value<'scope>) -> Result<(), Error> {
        let props = a.to_object(props)?;
        let mut descs = Vec::new();
        for k in a.own_property_keys(props)? {
            if a.get_own_property(props, k.key())?.is_some_and(|p| p.enumerable) {
                let d = a.get_key(props.into(), k.key(), props.into())?;
                descs.push((k, a.to_property_descriptor(d)?));
            }
        }
        for (k, d) in descs {
            a.define_property_or_throw(o, k.key(), d)?;
        }
        Ok(())
    }

    fn create<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let proto = Self::proto_arg(a, args.at(0))?;
        let o = ObjectData { proto, ..ObjectData::default() }.alloc(a.mc);
        if args.at(1) != Value::Undefined {
            Self::define_properties_on(a, o, args.at(1))?;
        }
        Ok(o.into())
    }

    fn define_properties<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let o = Self::require_object(a, args.at(0), "defineProperties")?;
        Self::define_properties_on(a, o, args.at(1))?;
        Ok(o.into())
    }

    fn define_property<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let o = Self::require_object(a, args.at(0), "defineProperty")?;
        let key = a.to_property_key(args.at(1))?;
        let desc = a.to_property_descriptor(args.at(2))?;
        a.define_property_or_throw(o, key.key(), desc)?;
        Ok(o.into())
    }

    fn get_own_property_descriptor<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let o = a.to_object(args.at(0))?;
        let key = a.to_property_key(args.at(1))?;
        let p = a.get_own_property(o, key.key())?;
        Ok(a.from_property_descriptor(p))
    }

    fn get_own_property_descriptors<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let o = a.to_object(args.at(0))?;
        let out = a.object();
        for k in a.own_property_keys(o)? {
            let p = a.get_own_property(o, k.key())?;
            let d = a.from_property_descriptor(p);
            if d != Value::Undefined {
                a.create_data_property_or_throw(out, k.key(), d)?;
            }
        }
        Ok(out.into())
    }

    fn own_keys_of<'scope>(a: &mut Scope<'scope, '_>, v: Value<'scope>, symbols: bool) -> Result<Value<'scope>, Error> {
        let o = a.to_object(v)?;
        let keys = a.own_property_keys(o)?.into_iter().filter(|k| matches!(k, PropertyKey::Symbol(_)) == symbols).map(Value::from).collect();
        Ok(a.array(keys).into())
    }

    fn get_own_property_names<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::own_keys_of(a, args.at(0), false)
    }

    fn get_own_property_symbols<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::own_keys_of(a, args.at(0), true)
    }

    fn freeze<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::integrity(a, args.at(0), Integrity::Frozen)
    }

    fn seal<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::integrity(a, args.at(0), Integrity::Sealed)
    }

    fn integrity<'scope>(a: &mut Scope<'scope, '_>, v: Value<'scope>, level: Integrity) -> Result<Value<'scope>, Error> {
        if let Some(o) = v.as_object()
            && !a.set_integrity_level(o, level)?
        {
            return Err(a.throw(ErrorKind::Type, "Cannot freeze or seal the object"));
        }
        Ok(v)
    }

    fn prevent_extensions<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        if let Some(o) = args.at(0).as_object()
            && !a.prevent_extensions(o)?
        {
            return Err(a.throw(ErrorKind::Type, "Cannot prevent extensions"));
        }
        Ok(args.at(0))
    }

    fn is_extensible<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Ok(match args.at(0).kind() {
            ValueKind::Object(o) => a.is_extensible(o)?,
            _ => false,
        }
        .into())
    }

    fn is_frozen<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Ok(match args.at(0).kind() {
            ValueKind::Object(o) => a.test_integrity_level(o, Integrity::Frozen)?,
            _ => true,
        }
        .into())
    }

    fn is_sealed<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Ok(match args.at(0).kind() {
            ValueKind::Object(o) => a.test_integrity_level(o, Integrity::Sealed)?,
            _ => true,
        }
        .into())
    }

    fn is<'scope>(_: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Ok(args.at(0).same_value(args.at(1)).into())
    }

    fn has_own<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let o = a.to_object(args.at(0))?;
        let key = a.to_property_key(args.at(1))?;
        Ok(a.has_own_property(o, key.key())?.into())
    }

    fn from_entries<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let iterable = args.at(0);
        if iterable.is_nullish() {
            return Err(a.throw(ErrorKind::Type, "Object.fromEntries requires an iterable"));
        }
        let o = a.object();
        let it = a.get_iterator(iterable)?;
        while let Some(item) = a.iterator_step(it)? {
            let r = (|| {
                if item.as_object().is_none() {
                    let t = a.describe(item);
                    return Err(a.throw(ErrorKind::Type, &alloc::format!("Iterator value {t} is not an entry object")));
                }
                let k = a.get_index(item, 0.0)?;
                let v = a.get_index(item, 1.0)?;
                let key = a.to_property_key(k)?;
                a.create_data_property_or_throw(o, key.key(), v)
            })();
            if let Err(e) = r {
                return Err(a.iterator_close_abrupt(it.iterator, e));
            }
        }
        Ok(o.into())
    }

    fn group_by<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let groups = Self::group(a, args.at(0), args.at(1), true)?;
        let o = ObjectData::default().alloc(a.mc);
        for (k, items) in groups {
            let arr = a.array(items);
            let key = a.to_property_key(k)?;
            a.create_data_property_or_throw(o, key.key(), arr.into())?;
        }
        Ok(o.into())
    }

    pub(crate) fn group<'scope>(a: &mut Scope<'scope, '_>, items: Value<'scope>, callback: Value<'scope>, property: bool) -> Result<Vec<(Value<'scope>, Vec<Value<'scope>>)>, Error> {
        if items.is_nullish() {
            return Err(a.throw(ErrorKind::Type, "groupBy called on null or undefined"));
        }
        if !a.is_callable(callback) {
            return Err(a.throw(ErrorKind::Type, "groupBy callback is not a function"));
        }
        let mut groups: Vec<(Value<'scope>, Vec<Value<'scope>>)> = Vec::new();
        let it = a.get_iterator(items)?;
        let mut k = 0.0;
        while let Some(v) = a.iterator_step(it)? {
            let key = a.call(callback, Value::Undefined, &[v, k.into()]).and_then(|key| {
                if property {
                    a.to_property_key(key).map(Value::from)
                } else {
                    Ok(match key.kind() {
                        ValueKind::Number(0.0) => Value::Number(0.0),
                        other => Value::from(other),
                    })
                }
            });
            let key = match key {
                Ok(key) => key,
                Err(e) => {
                    return Err(a.iterator_close_abrupt(it.iterator, e));
                }
            };
            match groups.iter_mut().find(|(g, _)| g.same_value(key)) {
                Some((_, list)) => list.push(v),
                None => groups.push((key, alloc::vec![v])),
            }
            k += 1.0;
        }
        Ok(groups)
    }

    fn to_string<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let builtin = match this.kind() {
            ValueKind::Undefined | ValueKind::Uninit => return Ok(a.string("[object Undefined]").into()),
            ValueKind::Null => return Ok(a.string("[object Null]").into()),
            _ => {
                let o = a.to_object(this)?;
                let is_array = a.is_array(o.into())?;
                let tag = match &o.borrow().kind {
                    _ if is_array => "Array",
                    ObjectKind::Arguments(_) => "Arguments",
                    k if k.is_callable() => "Function",
                    ObjectKind::Error(_) => "Error",
                    ObjectKind::Primitive(v) if matches!(v.kind(), ValueKind::Bool(_)) => "Boolean",
                    ObjectKind::Primitive(v) if v.is_number() => "Number",
                    ObjectKind::Primitive(v) if matches!(v.kind(), ValueKind::String(_)) => "String",
                    ObjectKind::RegExp(_) => "RegExp",
                    ObjectKind::Date(_) => "Date",
                    _ => "Object",
                };
                let t = a.get_symbol(o.into(), a.context.symbols.to_string_tag, o.into())?;
                if let ValueKind::String(s) = t.kind() {
                    return Ok(a.string(&alloc::format!("[object {}]", &**s)).into());
                }
                tag
            }
        };
        Ok(a.string(&alloc::format!("[object {builtin}]")).into())
    }

    fn to_locale_string<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let f = a.get_named(this, "toString")?;
        a.call(f, this, &[])
    }

    fn has_own_property<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let key = a.to_property_key(args.at(0))?;
        let o = a.to_object(this)?;
        Ok(a.has_own_property(o, key.key())?.into())
    }

    fn is_prototype_of<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let Some(v) = args.at(0).as_object() else { return Ok(false.into()) };
        let o = a.to_object(this)?;
        let mut cur = a.get_prototype_of(v)?;
        while let Some(c) = cur {
            if j8framework::ScopeLocal::ptr_eq(c, o) {
                return Ok(true.into());
            }
            cur = a.get_prototype_of(c)?;
        }
        Ok(false.into())
    }

    fn property_is_enumerable<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let key = a.to_property_key(args.at(0))?;
        let o = a.to_object(this)?;
        Ok(a.get_own_property(o, key.key())?.is_some_and(|p| p.enumerable).into())
    }

    fn proto_getter<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let o = a.to_object(this)?;
        Ok(a.get_prototype_of(o)?.map_or(Value::Null, Value::Object))
    }

    fn proto_setter<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        if this.is_nullish() {
            return Err(a.throw(ErrorKind::Type, "Object.prototype.__proto__ called on null or undefined"));
        }
        let proto = match args.at(0).kind() {
            ValueKind::Object(p) => Some(p),
            ValueKind::Null => None,
            _ => return Ok(Value::Undefined),
        };
        let Some(o) = this.as_object() else { return Ok(Value::Undefined) };
        if !a.set_prototype_of(o, proto)? {
            return Err(a.throw(ErrorKind::Type, "Cyclic __proto__ value or object is not extensible"));
        }
        Ok(Value::Undefined)
    }

    fn define_half<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>], setter: bool) -> Result<Value<'scope>, Error> {
        let o = a.to_object(this)?;
        let f = match args.at(1).kind() {
            ValueKind::Object(f) if f.borrow().kind.is_callable() => f,
            _ => return Err(a.throw(ErrorKind::Type, "Object.prototype.__defineGetter__: Expecting function")),
        };
        let desc = if setter { Descriptor { set: Some(Some(f)), ..Descriptor::default() } } else { Descriptor { get: Some(Some(f)), ..Descriptor::default() } };
        let desc = Descriptor { enumerable: Some(true), configurable: Some(true), ..desc };
        let key = a.to_property_key(args.at(0))?;
        a.define_property_or_throw(o, key.key(), desc)?;
        Ok(Value::Undefined)
    }

    fn define_getter<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::define_half(a, this, args, false)
    }

    fn define_setter<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::define_half(a, this, args, true)
    }

    fn lookup_half<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>], setter: bool) -> Result<Value<'scope>, Error> {
        let o = a.to_object(this)?;
        let key = a.to_property_key(args.at(0))?;
        let mut cur = Some(o);
        while let Some(c) = cur {
            if let Some(p) = a.get_own_property(c, key.key())? {
                return Ok(match p.slot {
                    Slot::Accessor { get, set } => (if setter { set } else { get }).map_or(Value::Undefined, Value::Object),
                    Slot::Data(_) => Value::Undefined,
                });
            }
            cur = a.get_prototype_of(c)?;
        }
        Ok(Value::Undefined)
    }

    fn lookup_getter<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::lookup_half(a, this, args, false)
    }

    fn lookup_setter<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::lookup_half(a, this, args, true)
    }

    fn value_of<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Ok(a.to_object(this)?.into())
    }

    fn call<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        if !a.is_callable(this) {
            return Err(a.throw(ErrorKind::Type, "Function.prototype.call called on a non-function"));
        }
        a.call(this, args.at(0), args.get(1..).unwrap_or(&[]))
    }

    fn apply<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        if !a.is_callable(this) {
            return Err(a.throw(ErrorKind::Type, "Function.prototype.apply was called on a non-function"));
        }
        let list = match args.at(1).kind() {
            ValueKind::Undefined | ValueKind::Null => Vec::new(),
            v => a.list_from_array_like(Value::from(v))?,
        };
        a.call(this, args.at(0), &list)
    }

    fn bind<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let Some(target) = this.as_object() else {
            return Err(a.throw(ErrorKind::Type, "Bind must be called on a function"));
        };
        if !a.is_callable(this) {
            return Err(a.throw(ErrorKind::Type, "Bind must be called on a function"));
        }
        let pre = args.get(1..).unwrap_or(&[]).to_vec();
        let proto = a.get_prototype_of(target)?;
        let bound = Bound { target: this, this: args.at(0), args: pre.clone() };
        let f = ObjectData { kind: ObjectKind::Bound(Box::new(bound)), proto, ..ObjectData::default() }.alloc(a.mc);
        let mut length = 0.0;
        if a.has_own_property(target, Key::Str("length"))?
            && let ValueKind::Number(n) = (a.get_named(this, "length")?).kind()
        {
            {
                let n = if n == f64::INFINITY { n } else if n == f64::NEG_INFINITY { 0.0 } else { a.to_integer_or_infinity(n.into())? };
                #[expect(clippy::cast_precision_loss, reason = "argument count is far below 2^53")]
                let k = pre.len() as f64;
                length = (n - k).max(0.0);
            }
        }
        let name = match (a.get_named(this, "name")?).kind() {
            ValueKind::String(s) => alloc::format!("bound {}", &**s),
            _ => alloc::string::String::from("bound "),
        };
        let prop = |v: Value<'scope>| Property { slot: Slot::Data(v), enumerable: false, writable: false, configurable: true };
        let n = a.string(&name);
        let d = f.borrow_mut(a.mc);
        d.define("length", prop(length.into()));
        d.define("name", prop(n.into()));
        drop(d);
        Ok(f.into())
    }

    fn function_to_string<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        if !a.is_callable(this) {
            return Err(a.throw(ErrorKind::Type, "Function.prototype.toString requires that 'this' be a Function"));
        }
        let name = match this.kind() {
            ValueKind::Object(o) => match o.borrow().get_own(a.mc, "name").and_then(|p| p.value()).map(Value::kind) {
                Some(ValueKind::String(s)) if Self::native_name(&s) => alloc::string::String::from(&**s),
                _ => alloc::string::String::new(),
            },
            _ => alloc::string::String::new(),
        };
        Ok(a.string(&alloc::format!("function {name}() {{ [native code] }}")).into())
    }

    fn native_name(s: &str) -> bool {
        let bare = s.strip_prefix("get ").or_else(|| s.strip_prefix("set ")).unwrap_or(s);
        let ident = bare.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$') && bare.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$');
        ident || (bare.starts_with('[') && bare.ends_with(']'))
    }

    fn noop<'scope>(_: &mut Scope<'scope, '_>, _: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Ok(Value::Undefined)
    }

    fn has_instance<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Ok(a.ordinary_has_instance(this, args.at(0))?.into())
    }

    fn boolean<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let new_target = a.new_target;
        let b = a.to_boolean(args.at(0));
        a.wrap_primitive(new_target, b.into(), a.context.boolean_proto)
    }

    fn this_boolean<'scope>(a: &Scope<'scope, '_>, this: Value<'scope>, method: &str) -> Result<bool, Error> {
        match this.unwrapped().kind() {
            ValueKind::Bool(b) => Ok(b),
            _ => Err(a.throw(ErrorKind::Type, &alloc::format!("Boolean.prototype.{method} requires that 'this' be a Boolean"))),
        }
    }

    fn boolean_to_string<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let b = Self::this_boolean(a, this, "toString")?;
        Ok(a.string(if b { "true" } else { "false" }).into())
    }

    fn boolean_value_of<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Ok(Self::this_boolean(a, this, "valueOf")?.into())
    }
}

impl Installer<'_, '_> {
    pub(crate) fn object(&mut self) {
        let proto = self.context.object_proto;
        let ctor = self.constructor("Object", Objects::object, proto, 1);
        let statics: [(&str, j8framework::NativeFn, u32); 23] = [
            ("assign", Objects::assign, 2),
            ("create", Objects::create, 2),
            ("defineProperties", Objects::define_properties, 2),
            ("defineProperty", Objects::define_property, 3),
            ("entries", Objects::entries, 1),
            ("freeze", Objects::freeze, 1),
            ("fromEntries", Objects::from_entries, 1),
            ("getOwnPropertyDescriptor", Objects::get_own_property_descriptor, 2),
            ("getOwnPropertyDescriptors", Objects::get_own_property_descriptors, 1),
            ("getOwnPropertyNames", Objects::get_own_property_names, 1),
            ("getOwnPropertySymbols", Objects::get_own_property_symbols, 1),
            ("getPrototypeOf", Objects::get_prototype_of, 1),
            ("groupBy", Objects::group_by, 2),
            ("hasOwn", Objects::has_own, 2),
            ("is", Objects::is, 2),
            ("isExtensible", Objects::is_extensible, 1),
            ("isFrozen", Objects::is_frozen, 1),
            ("isSealed", Objects::is_sealed, 1),
            ("keys", Objects::keys, 1),
            ("preventExtensions", Objects::prevent_extensions, 1),
            ("seal", Objects::seal, 1),
            ("setPrototypeOf", Objects::set_prototype_of, 2),
            ("values", Objects::values, 1),
        ];
        for (name, f, n) in statics {
            self.method(ctor, name, f, n);
        }
        let methods: [(&str, j8framework::NativeFn, u32); 10] = [
            ("hasOwnProperty", Objects::has_own_property, 1),
            ("isPrototypeOf", Objects::is_prototype_of, 1),
            ("propertyIsEnumerable", Objects::property_is_enumerable, 1),
            ("toLocaleString", Objects::to_locale_string, 0),
            ("toString", Objects::to_string, 0),
            ("valueOf", Objects::value_of, 0),
            ("__defineGetter__", Objects::define_getter, 2),
            ("__defineSetter__", Objects::define_setter, 2),
            ("__lookupGetter__", Objects::lookup_getter, 1),
            ("__lookupSetter__", Objects::lookup_setter, 1),
        ];
        for (name, f, n) in methods {
            self.method(proto, name, f, n);
        }
        let get = self.context.named_native(self.mc, Objects::proto_getter, "get __proto__", 0);
        let set = self.context.named_native(self.mc, Objects::proto_setter, "set __proto__", 1);
        proto.borrow_mut(self.mc).define("__proto__", Property { slot: Slot::Accessor { get: Some(get), set: Some(set) }, enumerable: false, writable: false, configurable: true });
    }

    pub(crate) fn function(&mut self) {
        let proto = self.context.function_proto;
        proto.borrow_mut(self.mc).kind = ObjectKind::Native(j8framework::Native::function(Objects::noop));
        self.context.name_function(self.mc, proto, "", Some(0));
        self.method(proto, "call", Objects::call, 1);
        self.method(proto, "apply", Objects::apply, 2);
        for (name, forward) in [("call", j8framework::Forward::Call), ("apply", j8framework::Forward::Apply)] {
            let f = proto.borrow().own(self.mc, name);
            if let Some(ValueKind::Object(f)) = f.map(Value::kind)
                && let ObjectKind::Native(n) = &mut f.borrow_mut(self.mc).kind
            {
                n.forward = forward;
            }
        }
        self.method(proto, "bind", Objects::bind, 1);
        self.method(proto, "toString", Objects::function_to_string, 0);
        let has_instance = self.context.named_native(self.mc, Objects::has_instance, "[Symbol.hasInstance]", 1);
        proto.borrow_mut(self.mc).define_symbol(self.context.symbols.has_instance, Property { slot: Slot::Data(has_instance.into()), enumerable: false, writable: false, configurable: false });
        let b = self.context.boolean_proto;
        b.borrow_mut(self.mc).kind = ObjectKind::Primitive(false.into());
        self.constructor("Boolean", Objects::boolean, b, 1);
        self.method(b, "toString", Objects::boolean_to_string, 0);
        self.method(b, "valueOf", Objects::boolean_value_of, 0);
    }
}
