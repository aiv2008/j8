use j8framework::ValueKind;
use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

use j8framework::{Scope, Element, ElementKind, Error, ErrorKind, IterKind, JsObject, ObjectData, ObjectKind, Property, Slot, Value, ViewData};

use crate::buffer::Buffers;
use crate::{Args, Installer};

struct Typed;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Visit {
    ForEach,
    Map,
    Filter,
    Some,
    Every,
    Find,
    FindIndex,
    FindLast,
    FindLastIndex,
}

impl Typed {
    fn usize_value<'scope>(n: usize) -> Value<'scope> {
        #[expect(clippy::cast_precision_loss, reason = "length is far below 2^53")]
        let x = n as f64;
        x.into()
    }

    fn validate<'scope>(a: &Scope<'scope, '_>, v: Value<'scope>) -> Result<(JsObject<'scope>, ElementKind, ViewData<'scope>, usize), Error> {
        let Some(o) = v.as_object() else { return Err(a.throw(ErrorKind::Type, "this is not a typed array")) };
        let Some((k, view)) = Scope::typed_array(o) else { return Err(a.throw(ErrorKind::Type, "this is not a typed array")) };
        match Scope::view_length(&view, k.size()) {
            Some(len) => Ok((o, k, view, len)),
            None => Err(a.throw(ErrorKind::Type, "Cannot perform this operation on a detached or out-of-bounds TypedArray")),
        }
    }

    fn create<'scope>(a: &mut Scope<'scope, '_>, kind: ElementKind, proto: JsObject<'scope>, len: usize) -> Result<JsObject<'scope>, Error> {
        let Some(bytes) = len.checked_mul(kind.size()) else { return Err(a.throw(ErrorKind::Range, "Invalid typed array length")) };
        let buffer = Buffers::allocate(a, a.context.array_buffer_proto, bytes, None, false)?;
        Ok(ObjectData { kind: ObjectKind::TypedArray(kind, ViewData { buffer, offset: 0, length: Some(len) }), proto: Some(proto), ..ObjectData::default() }.alloc(a.mc))
    }

    fn index_of_kind(kind: ElementKind) -> usize {
        ElementKind::ALL.iter().position(|&k| k == kind).unwrap_or(0)
    }

    #[expect(clippy::too_many_lines, reason = "the spec initializes from four kinds of argument in one place")]
    fn construct<'scope, const K: usize>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let new_target = a.new_target;
        let kind = ElementKind::ALL[K];
        if new_target == Value::Undefined {
            return Err(a.throw(ErrorKind::Type, &alloc::format!("Constructor {} requires 'new'", kind.name())));
        }
        let first = args.at(0);
        let Some(src) = first.as_object() else {
            let len = a.to_index(first)?;
            let proto = a.proto_from_ctor(new_target, a.context.typed_protos[K])?;
            return Ok(Self::create(a, kind, proto, len)?.into());
        };
        if matches!(src.borrow().kind, ObjectKind::ArrayBuffer(_)) {
            let proto = a.proto_from_ctor(new_target, a.context.typed_protos[K])?;
            let size = kind.size();
            let offset = a.to_index(args.at(1))?;
            if offset % size != 0 {
                return Err(a.throw(ErrorKind::Range, &alloc::format!("start offset of {} should be a multiple of {size}", kind.name())));
            }
            let new_len = match args.at(2).kind() {
                ValueKind::Undefined => None,
                v => Some(a.to_index(Value::from(v))?),
            };
            let (len, max) = match &src.borrow().kind {
                ObjectKind::ArrayBuffer(b) => (b.bytes.as_deref().map(<[u8]>::len), b.max),
                _ => (None, None),
            };
            let Some(buf_len) = len else { return Err(a.throw(ErrorKind::Type, "Cannot construct a TypedArray on a detached ArrayBuffer")) };
            let length = match new_len {
                None if max.is_some() => {
                    if offset > buf_len {
                        return Err(a.throw(ErrorKind::Range, "Start offset is outside the bounds of the buffer"));
                    }
                    None
                }
                None => {
                    if buf_len % size != 0 {
                        return Err(a.throw(ErrorKind::Range, &alloc::format!("byte length of {} should be a multiple of {size}", kind.name())));
                    }
                    if offset > buf_len {
                        return Err(a.throw(ErrorKind::Range, "Start offset is outside the bounds of the buffer"));
                    }
                    Some((buf_len - offset) / size)
                }
                Some(n) => {
                    if offset + n * size > buf_len {
                        return Err(a.throw(ErrorKind::Range, "Invalid typed array length"));
                    }
                    Some(n)
                }
            };
            let view = ViewData { buffer: src, offset, length };
            return Ok(ObjectData { kind: ObjectKind::TypedArray(kind, view), proto: Some(proto), ..ObjectData::default() }.alloc(a.mc).into());
        }
        if let Some((sk, sview)) = Scope::typed_array(src) {
            let proto = a.proto_from_ctor(new_target, a.context.typed_protos[K])?;
            let Some(len) = Scope::view_length(&sview, sk.size()) else {
                return Err(a.throw(ErrorKind::Type, "source TypedArray is detached or out of bounds"));
            };
            if sk.is_bigint() != kind.is_bigint() {
                return Err(a.throw(ErrorKind::Type, "Cannot mix BigInt and other types"));
            }
            let out = Self::create(a, kind, proto, len)?;
            let (_, view) = Scope::typed_array(out).expect("just created");
            for i in 0..len {
                if let Some(e) = Scope::read_raw(sview.buffer, sview.offset + i * sk.size(), sk) {
                    a.write_raw(view.buffer, i * kind.size(), kind, &e);
                }
            }
            return Ok(out.into());
        }
        let proto = a.proto_from_ctor(new_target, a.context.typed_protos[K])?;
        let using = a.get_symbol(first, a.context.symbols.iterator, first)?;
        let values = if using.is_nullish() {
            None
        } else {
            if !a.is_callable(using) {
                return Err(a.throw(ErrorKind::Type, "Symbol.iterator is not a function"));
            }
            let iterator = a.call(using, first, &[])?;
            if iterator.as_object().is_none() {
                return Err(a.throw(ErrorKind::Type, "Result of the Symbol.iterator method is not an object"));
            }
            let next = a.get_named(iterator, "next")?;
            let it = j8framework::IterRecord { iterator, next };
            let mut out = Vec::new();
            while let Some(v) = a.iterator_step(it)? {
                out.push(v);
            }
            Some(out)
        };
        let out = match values {
            Some(values) => {
                let out = Self::create(a, kind, proto, values.len())?;
                for (i, v) in (0..).zip(values) {
                    a.typed_set(out, f64::from(i), v)?;
                }
                out
            }
            None => {
                let len = a.length_of_array_like(first)?;
                #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "result of ToLength")]
                let out = Self::create(a, kind, proto, len as usize)?;
                let mut k = 0.0;
                while k < len {
                    let v = a.get_index(first, k)?;
                    a.typed_set(out, k, v)?;
                    k += 1.0;
                }
                out
            }
        };
        Ok(out.into())
    }

    fn abstract_ctor<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Err(a.throw(ErrorKind::Type, "Abstract class TypedArray not directly constructable"))
    }

    fn create_from_ctor<'scope>(a: &mut Scope<'scope, '_>, ctor: Value<'scope>, args: &[Value<'scope>]) -> Result<(JsObject<'scope>, ElementKind, usize), Error> {
        let made = a.construct(ctor, args, ctor)?;
        let (o, k, _, len) = Self::validate(a, made)?;
        if let [v] = *args
            && let ValueKind::Number(n) = v.kind()
            && Self::f(len) < n
        {
            return Err(a.throw(ErrorKind::Type, "TypedArray species constructor returned a too short array"));
        }
        Ok((o, k, len))
    }

    fn species_create<'scope>(a: &mut Scope<'scope, '_>, exemplar: JsObject<'scope>, kind: ElementKind, args: &[Value<'scope>]) -> Result<JsObject<'scope>, Error> {
        let default = a.context.typed_ctors[Self::index_of_kind(kind)].into();
        let ctor = Buffers::species_constructor(a, exemplar.into(), default)?;
        let (o, k, _) = Self::create_from_ctor(a, ctor, args)?;
        if k.is_bigint() != kind.is_bigint() {
            return Err(a.throw(ErrorKind::Type, "TypedArray species constructor returned an array with a different content type"));
        }
        Ok(o)
    }

    fn species<'scope>(_: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Ok(this)
    }

    fn from<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        if !a.is_constructor(this) {
            return Err(a.throw(ErrorKind::Type, "TypedArray.from: this is not a constructor"));
        }
        let (source, mapper, this_arg) = (args.at(0), args.at(1), args.at(2));
        let mapping = mapper != Value::Undefined;
        if mapping && !a.is_callable(mapper) {
            return Err(a.throw(ErrorKind::Type, "TypedArray.from: mapFn is not a function"));
        }
        let using = if source.is_nullish() { Value::Undefined } else { a.get_symbol(source, a.context.symbols.iterator, source)? };
        let values = if using.is_nullish() {
            let like = a.to_object(source)?;
            let len = a.length_of_array_like(like.into())?;
            let mut k = 0.0;
            let (target, _, _) = Self::create_from_ctor(a, this, &[len.into()])?;
            while k < len {
                let v = a.get_index(like.into(), k)?;
                let v = if mapping { a.call(mapper, this_arg, &[v, k.into()])? } else { v };
                a.set_index(target.into(), k, v)?;
                k += 1.0;
            }
            return Ok(target.into());
        } else {
            if !a.is_callable(using) {
                return Err(a.throw(ErrorKind::Type, "Symbol.iterator is not a function"));
            }
            let iterator = a.call(using, source, &[])?;
            if iterator.as_object().is_none() {
                return Err(a.throw(ErrorKind::Type, "Result of the Symbol.iterator method is not an object"));
            }
            let next = a.get_named(iterator, "next")?;
            let it = j8framework::IterRecord { iterator, next };
            let mut out = Vec::new();
            while let Some(v) = a.iterator_step(it)? {
                out.push(v);
            }
            out
        };
        let len = values.len();
        let (target, _, _) = Self::create_from_ctor(a, this, &[Self::usize_value(len)])?;
        for (k, v) in (0..).zip(values) {
            let v = if mapping { a.call(mapper, this_arg, &[v, f64::from(k).into()])? } else { v };
            a.set_index(target.into(), f64::from(k), v)?;
        }
        Ok(target.into())
    }

    fn of<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        if !a.is_constructor(this) {
            return Err(a.throw(ErrorKind::Type, "TypedArray.of: this is not a constructor"));
        }
        let (target, _, _) = Self::create_from_ctor(a, this, &[Self::usize_value(args.len())])?;
        for (k, &v) in (0..).zip(args) {
            a.set_index(target.into(), f64::from(k), v)?;
        }
        Ok(target.into())
    }

    fn this_typed<'scope>(a: &Scope<'scope, '_>, this: Value<'scope>) -> Result<(ElementKind, ViewData<'scope>), Error> {
        match this.kind() {
            ValueKind::Object(o) => Scope::typed_array(o).ok_or_else(|| a.throw(ErrorKind::Type, "this is not a typed array")),
            _ => Err(a.throw(ErrorKind::Type, "this is not a typed array")),
        }
    }

    fn buffer<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Ok(Self::this_typed(a, this)?.1.buffer.into())
    }

    fn byte_length<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (k, v) = Self::this_typed(a, this)?;
        Ok(Self::usize_value(Scope::view_length(&v, k.size()).map_or(0, |n| n * k.size())))
    }

    fn byte_offset<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (k, v) = Self::this_typed(a, this)?;
        Ok(Self::usize_value(Scope::view_length(&v, k.size()).map_or(0, |_| v.offset)))
    }

    fn length<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (k, v) = Self::this_typed(a, this)?;
        Ok(Self::usize_value(Scope::view_length(&v, k.size()).unwrap_or(0)))
    }

    fn to_string_tag<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Ok(match this.kind() {
            ValueKind::Object(o) => Scope::typed_array(o).map_or(Value::Undefined, |(k, _)| a.string(k.name()).into()),
            _ => Value::Undefined,
        })
    }

    fn relative<'scope>(a: &mut Scope<'scope, '_>, v: Value<'scope>, len: f64, default: f64) -> Result<f64, Error> {
        if v == Value::Undefined {
            return Ok(default);
        }
        let x = a.to_integer_or_infinity(v)?;
        Ok(if x < 0.0 { (len + x).max(0.0) } else { x.min(len) })
    }

    fn at<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, _, _, len) = Self::validate(a, this)?;
        #[expect(clippy::cast_precision_loss, reason = "length is far below 2^53")]
        let len = len as f64;
        let rel = a.to_integer_or_infinity(args.at(0))?;
        let k = if rel >= 0.0 { rel } else { len + rel };
        if k < 0.0 || k >= len {
            return Ok(Value::Undefined);
        }
        a.get_index(o.into(), k)
    }

    fn visit<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>], how: Visit) -> Result<Value<'scope>, Error> {
        let (o, kind, _, len) = Self::validate(a, this)?;
        let (f, this_arg) = (args.at(0), args.at(1));
        if !a.is_callable(f) {
            let t = a.describe(f);
            return Err(a.throw(ErrorKind::Type, &alloc::format!("{t} is not a function")));
        }
        let mut kept = Vec::new();
        let mut mapped = Vec::new();
        let backward = matches!(how, Visit::FindLast | Visit::FindLastIndex);
        for i in 0..len {
            let k = if backward { len - 1 - i } else { i };
            #[expect(clippy::cast_precision_loss, reason = "length is far below 2^53")]
            let kf = k as f64;
            let v = a.get_index(o.into(), kf)?;
            let r = a.call(f, this_arg, &[v, kf.into(), o.into()])?;
            match how {
                Visit::Map => mapped.push(r),
                Visit::Filter if a.to_boolean(r) => kept.push(v),
                Visit::Some if a.to_boolean(r) => return Ok(true.into()),
                Visit::Every if !a.to_boolean(r) => return Ok(false.into()),
                Visit::Find | Visit::FindLast if a.to_boolean(r) => return Ok(v),
                Visit::FindIndex | Visit::FindLastIndex if a.to_boolean(r) => return Ok(kf.into()),
                _ => {}
            }
        }
        Ok(match how {
            Visit::ForEach | Visit::Find | Visit::FindLast => Value::Undefined,
            Visit::FindIndex | Visit::FindLastIndex => (-1.0).into(),
            Visit::Some => false.into(),
            Visit::Every => true.into(),
            Visit::Map => {
                let out = Self::species_create(a, o, kind, &[Self::usize_value(len)])?;
                for (k, v) in (0..).zip(mapped) {
                    a.set_index(out.into(), f64::from(k), v)?;
                }
                out.into()
            }
            Visit::Filter => {
                let out = Self::species_create(a, o, kind, &[Self::usize_value(kept.len())])?;
                for (k, v) in (0..).zip(kept) {
                    a.set_index(out.into(), f64::from(k), v)?;
                }
                out.into()
            }
        })
    }
}

macro_rules! visits {
    ($($name:ident => $how:ident),* $(,)?) => {
        impl Typed {
            $(fn $name<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
                Self::visit(a, this, args, Visit::$how)
            })*
        }
    };
}

visits! {
    for_each => ForEach,
    map => Map,
    filter => Filter,
    some => Some,
    every => Every,
    find => Find,
    find_index => FindIndex,
    find_last => FindLast,
    find_last_index => FindLastIndex,
}

impl Typed {
    fn fold<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>], right: bool) -> Result<Value<'scope>, Error> {
        let (o, _, _, len) = Self::validate(a, this)?;
        let f = args.at(0);
        if !a.is_callable(f) {
            let t = a.describe(f);
            return Err(a.throw(ErrorKind::Type, &alloc::format!("{t} is not a function")));
        }
        let index = |i: usize| if right { len - 1 - i } else { i };
        let mut i = 0;
        let mut acc = if args.len() >= 2 {
            args[1]
        } else {
            if len == 0 {
                return Err(a.throw(ErrorKind::Type, "Reduce of empty array with no initial value"));
            }
            i = 1;
            a.get_index(o.into(), Self::f(index(0)))?
        };
        while i < len {
            let k = Self::f(index(i));
            let v = a.get_index(o.into(), k)?;
            acc = a.call(f, Value::Undefined, &[acc, v, k.into(), o.into()])?;
            i += 1;
        }
        Ok(acc)
    }

    fn f(n: usize) -> f64 {
        #[expect(clippy::cast_precision_loss, reason = "length is far below 2^53")]
        let x = n as f64;
        x
    }

    fn reduce<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::fold(a, this, args, false)
    }

    fn reduce_right<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::fold(a, this, args, true)
    }

    fn fill<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, kind, _, len) = Self::validate(a, this)?;
        let lenf = Self::f(len);
        let e = a.to_element(kind, args.at(0))?;
        let start = Self::relative(a, args.at(1), lenf, 0.0)?;
        let end = Self::relative(a, args.at(2), lenf, lenf)?;
        let (_, _, view, len) = Self::validate(a, o.into())?;
        let end = end.min(Self::f(len));
        let pattern = kind.encode(&e);
        let size = kind.size();
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "clamped to [0, len]")]
        let (from, to) = (view.offset + start as usize * size, view.offset + end.max(start) as usize * size);
        let par = a.par();
        if let ObjectKind::ArrayBuffer(b) = &mut view.buffer.borrow_mut(a.mc).kind
            && let Some(dst) = b.bytes.as_mut().and_then(|x| x.get_mut(from..to))
        {
            par.each_mut(dst, size, &|c: &mut [u8]| c.chunks_exact_mut(size).for_each(|e| e.copy_from_slice(&pattern)));
        }
        Ok(o.into())
    }

    fn search<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>], last: bool, zero: bool) -> Result<Value<'scope>, Error> {
        let (o, _, _, len) = Self::validate(a, this)?;
        let not_found: Value<'scope> = if zero { false.into() } else { (-1.0).into() };
        if len == 0 {
            return Ok(not_found);
        }
        let lenf = Self::f(len);
        let want = args.at(0);
        if last {
            let n = if args.len() > 1 { a.to_integer_or_infinity(args[1])? } else { lenf - 1.0 };
            if n == f64::NEG_INFINITY {
                return Ok(not_found);
            }
            let mut k = if n >= 0.0 { n.min(lenf - 1.0) } else { lenf + n };
            while k >= 0.0 {
                if let Some(v) = a.typed_get(o, k)
                    && v == want
                {
                    return Ok(k.into());
                }
                k -= 1.0;
            }
            return Ok(not_found);
        }
        let n = a.to_integer_or_infinity(args.at(1))?;
        if n == f64::INFINITY {
            return Ok(not_found);
        }
        let mut k = if n >= 0.0 { n } else { (lenf + n).max(0.0) };
        if let Some(w) = want.as_number()
            && let Some((kind, view)) = Scope::typed_array(o)
            && !kind.is_bigint()
        {
            #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "clamped to [0, len]")]
            let from = k as usize;
            let end = Scope::view_length(&view, kind.size()).unwrap_or(0).min(len);
            let buffer = view.buffer.borrow();
            let ObjectKind::ArrayBuffer(b) = &buffer.kind else { return Ok(not_found) };
            let bytes = b.bytes.as_deref().unwrap_or(&[]);
            let size = kind.size();
            let hit = |x: f64| x == w || (zero && x.is_nan() && w.is_nan());
            let found = a.par().find_first(end.saturating_sub(from), &|r: core::ops::Range<usize>| {
                r.into_iter().find(|&i| {
                    let at = view.offset + (from + i) * size;
                    bytes.get(at..at + size).is_some_and(|e| matches!(kind.decode(e), j8framework::Element::Number(x) if hit(x)))
                })
            });
            return Ok(match found {
                Some(_) if zero => true.into(),
                Some(i) => Self::f(from + i).into(),
                None => not_found,
            });
        }
        while k < lenf {
            let v = a.typed_get(o, k);
            let hit = match v {
                Some(v) if zero => v == want || matches!((v.kind(), want.kind()), (ValueKind::Number(x), ValueKind::Number(y)) if x.is_nan() && y.is_nan()),
                Some(v) => v == want,
                None => zero && want == Value::Undefined,
            };
            if hit {
                return Ok(if zero { true.into() } else { k.into() });
            }
            k += 1.0;
        }
        Ok(not_found)
    }

    fn index_of<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::search(a, this, args, false, false)
    }

    fn last_index_of<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::search(a, this, args, true, false)
    }

    fn includes<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::search(a, this, args, false, true)
    }

    fn join<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, _, _, len) = Self::validate(a, this)?;
        let sep = match args.at(0).kind() {
            ValueKind::Undefined => a.string(","),
            v => a.to_string(Value::from(v))?,
        };
        let mut out = String::new();
        for k in 0..len {
            if k > 0 {
                out.push_str(&sep);
            }
            let v = a.get_index(o.into(), Self::f(k))?;
            if v != Value::Undefined {
                out.push_str(&a.to_string(v)?);
            }
        }
        Ok(a.string(&out).into())
    }

    fn to_locale_string<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, _, _, len) = Self::validate(a, this)?;
        let joined = crate::LocaleJoin::join(a, o, Self::f(len), args)?;
        Ok(a.string(&joined).into())
    }

    fn elements<'scope>(a: &Scope<'scope, '_>, kind: ElementKind, view: &ViewData<'scope>, len: usize) -> Vec<Value<'scope>> {
        (0..len).filter_map(|i| Scope::read_raw(view.buffer, view.offset + i * kind.size(), kind)).map(|e| a.element_value(e)).collect()
    }

    fn write_all<'scope>(a: &Scope<'scope, '_>, kind: ElementKind, view: &ViewData<'scope>, values: &[Value<'scope>]) {
        for (i, v) in values.iter().enumerate() {
            let e = match (*v).kind() {
                ValueKind::Number(x) => Element::Number(x),
                ValueKind::BigInt(b) => Element::BigInt(b.0.clone()),
                _ => continue,
            };
            a.write_raw(view.buffer, view.offset + i * kind.size(), kind, &e);
        }
    }

    fn reverse<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, kind, view, len) = Self::validate(a, this)?;
        let mut values = Self::elements(a, kind, &view, len);
        values.reverse();
        Self::write_all(a, kind, &view, &values);
        Ok(o.into())
    }

    fn to_reversed<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (_, kind, view, len) = Self::validate(a, this)?;
        let mut values = Self::elements(a, kind, &view, len);
        values.reverse();
        let out = Self::create(a, kind, a.context.typed_protos[Self::index_of_kind(kind)], len)?;
        let (_, ov) = Scope::typed_array(out).expect("just created");
        Self::write_all(a, kind, &ov, &values);
        Ok(out.into())
    }

    fn order<'scope>(a: &mut Scope<'scope, '_>, f: Value<'scope>, x: Value<'scope>, y: Value<'scope>) -> Result<Ordering, Error> {
        if f != Value::Undefined {
            let r = a.call(f, Value::Undefined, &[x, y])?;
            return Ok(a.to_number(r)?.partial_cmp(&0.0).unwrap_or(Ordering::Equal));
        }
        Ok(match (x.kind(), y.kind()) {
            (ValueKind::Number(p), ValueKind::Number(q)) => match (p.is_nan(), q.is_nan()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                _ if p == 0.0 && q == 0.0 => p.is_sign_positive().cmp(&q.is_sign_positive()),
                _ => p.partial_cmp(&q).unwrap_or(Ordering::Equal),
            },
            (ValueKind::BigInt(p), ValueKind::BigInt(q)) => p.0.cmp(&q.0),
            _ => Ordering::Equal,
        })
    }

    fn numeric_order(x: f64, y: f64) -> Ordering {
        match (x.is_nan(), y.is_nan()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => x.total_cmp(&y),
        }
    }

    fn sorted<'scope>(a: &mut Scope<'scope, '_>, f: Value<'scope>, v: Vec<Value<'scope>>) -> Result<Vec<Value<'scope>>, Error> {
        if v.len() <= 1 {
            return Ok(v);
        }
        if f == Value::Undefined && v.iter().all(|x| x.is_number()) {
            let mut xs: Vec<f64> = v.iter().map(|x| x.as_number().unwrap_or(0.0)).collect();
            a.par().sort_by(&mut xs, &|x, y| Self::numeric_order(*x, *y));
            return Ok(xs.into_iter().map(Value::Number).collect());
        }
        let mut right = v;
        let left = right.drain(..right.len() / 2).collect();
        let (left, right) = (Self::sorted(a, f, left)?, Self::sorted(a, f, right)?);
        let mut out = Vec::with_capacity(left.len() + right.len());
        let (mut l, mut r) = (left.into_iter().peekable(), right.into_iter().peekable());
        while let (Some(&x), Some(&y)) = (l.peek(), r.peek()) {
            if Self::order(a, f, x, y)? == Ordering::Greater {
                out.push(y);
                r.next();
            } else {
                out.push(x);
                l.next();
            }
        }
        out.extend(l);
        out.extend(r);
        Ok(out)
    }

    fn comparator<'scope>(a: &Scope<'scope, '_>, f: Value<'scope>) -> Result<(), Error> {
        if f != Value::Undefined && !a.is_callable(f) {
            return Err(a.throw(ErrorKind::Type, "The comparison function must be either a function or undefined"));
        }
        Ok(())
    }

    fn sort<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::comparator(a, args.at(0))?;
        let (o, kind, view, len) = Self::validate(a, this)?;
        let values = Self::elements(a, kind, &view, len);
        let sorted = Self::sorted(a, args.at(0), values)?;
        let len = Scope::view_length(&view, kind.size()).unwrap_or(0);
        Self::write_all(a, kind, &view, &sorted[..len.min(sorted.len())]);
        Ok(o.into())
    }

    fn to_sorted<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::comparator(a, args.at(0))?;
        let (_, kind, view, len) = Self::validate(a, this)?;
        let out = Self::create(a, kind, a.context.typed_protos[Self::index_of_kind(kind)], len)?;
        let values = Self::elements(a, kind, &view, len);
        let sorted = Self::sorted(a, args.at(0), values)?;
        let (_, ov) = Scope::typed_array(out).expect("just created");
        Self::write_all(a, kind, &ov, &sorted);
        Ok(out.into())
    }

    fn with<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, kind, _, len) = Self::validate(a, this)?;
        let lenf = Self::f(len);
        let rel = a.to_integer_or_infinity(args.at(0))?;
        let at = if rel >= 0.0 { rel } else { lenf + rel };
        let v = if kind.is_bigint() { a.to_bigint(args.at(1)).map(|b| a.bigint(b))? } else { a.to_number(args.at(1))?.into() };
        if a.typed_get(o, at).is_none() {
            return Err(a.throw(ErrorKind::Range, "Invalid typed array index"));
        }
        let out = Self::create(a, kind, a.context.typed_protos[Self::index_of_kind(kind)], len)?;
        for k in 0..len {
            let kf = Self::f(k);
            let x = if kf == at { v } else { a.get_index(o.into(), kf)? };
            a.typed_set(out, kf, x)?;
        }
        Ok(out.into())
    }

    fn slice<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, kind, _, len) = Self::validate(a, this)?;
        let lenf = Self::f(len);
        let start = Self::relative(a, args.at(0), lenf, 0.0)?;
        let end = Self::relative(a, args.at(1), lenf, lenf)?;
        let count = (end - start).max(0.0);
        let out = Self::species_create(a, o, kind, &[count.into()])?;
        if count > 0.0 {
            let (_, _, view, len) = Self::validate(a, o.into())?;
            let end = end.min(Self::f(len));
            let mut k = start;
            let mut n = 0.0;
            while k < end {
                #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "clamped to [0, len]")]
                let i = k as usize;
                if let Some(e) = Scope::read_raw(view.buffer, view.offset + i * kind.size(), kind) {
                    let v = a.element_value(e);
                    a.typed_set(out, n, v)?;
                }
                k += 1.0;
                n += 1.0;
            }
        }
        Ok(out.into())
    }

    fn subarray<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (kind, view) = Self::this_typed(a, this)?;
        let Some(o) = this.as_object() else { unreachable!("checked above") };
        let len = Self::f(Scope::view_length(&view, kind.size()).unwrap_or(0));
        let start = Self::relative(a, args.at(0), len, 0.0)?;
        let tracking = view.length.is_none() && args.at(1) == Value::Undefined;
        let end = Self::relative(a, args.at(1), len, len)?;
        let begin = Self::f(view.offset) + start * Self::f(kind.size());
        let mut ctor_args = alloc::vec![view.buffer.into(), begin.into()];
        if !tracking {
            ctor_args.push((end - start).max(0.0).into());
        }
        Ok(Self::species_create(a, o, kind, &ctor_args)?.into())
    }

    fn set<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (kind, _) = Self::this_typed(a, this)?;
        let Some(target) = this.as_object() else { unreachable!("checked above") };
        let offset = a.to_integer_or_infinity(args.at(1))?;
        if offset < 0.0 {
            return Err(a.throw(ErrorKind::Range, "offset is out of bounds"));
        }
        let (_, _, tview, tlen) = Self::validate(a, this)?;
        let source = args.at(0);
        if let Some(s) = source.as_object()
            && let Some((sk, sview)) = Scope::typed_array(s)
        {
            let Some(slen) = Scope::view_length(&sview, sk.size()) else {
                return Err(a.throw(ErrorKind::Type, "source TypedArray is detached or out of bounds"));
            };
            if sk.is_bigint() != kind.is_bigint() {
                return Err(a.throw(ErrorKind::Type, "Cannot mix BigInt and other types"));
            }
            if offset == f64::INFINITY || Self::f(slen) + offset > Self::f(tlen) {
                return Err(a.throw(ErrorKind::Range, "offset is out of bounds"));
            }
            #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "checked to be in bounds")]
            let off = offset as usize;
            let items: Vec<Element> = (0..slen).filter_map(|i| Scope::read_raw(sview.buffer, sview.offset + i * sk.size(), sk)).collect();
            for (i, e) in items.iter().enumerate() {
                a.write_raw(tview.buffer, tview.offset + (off + i) * kind.size(), kind, e);
            }
            return Ok(Value::Undefined);
        }
        let src = a.to_object(source)?;
        let slen = a.length_of_array_like(src.into())?;
        if offset == f64::INFINITY || slen + offset > Self::f(tlen) {
            return Err(a.throw(ErrorKind::Range, "offset is out of bounds"));
        }
        let mut k = 0.0;
        while k < slen {
            let v = a.get_index(src.into(), k)?;
            a.typed_set(target, offset + k, v)?;
            k += 1.0;
        }
        Ok(Value::Undefined)
    }

    fn copy_within<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, kind, _, len) = Self::validate(a, this)?;
        let lenf = Self::f(len);
        let to = Self::relative(a, args.at(0), lenf, 0.0)?;
        let from = Self::relative(a, args.at(1), lenf, 0.0)?;
        let end = Self::relative(a, args.at(2), lenf, lenf)?;
        let count = (end - from).min(lenf - to);
        if count > 0.0 {
            let (_, _, view, len) = Self::validate(a, o.into())?;
            let size = kind.size();
            #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "clamped to [0, len]")]
            let (to, from, count) = (to as usize, from as usize, count as usize);
            let byte_len = len * size;
            let (src, dst) = (from * size, to * size);
            let n = (count * size).min(byte_len.saturating_sub(src)).min(byte_len.saturating_sub(dst));
            if let ObjectKind::ArrayBuffer(b) = &mut view.buffer.borrow_mut(a.mc).kind
                && let Some(bytes) = b.bytes.as_mut()
            {
                let base = view.offset;
                bytes.copy_within(base + src..base + src + n, base + dst);
            }
        }
        Ok(o.into())
    }

    fn iterate<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, kind: IterKind) -> Result<Value<'scope>, Error> {
        let (o, _, _, _) = Self::validate(a, this)?;
        Ok(a.array_iterator(o.into(), kind).into())
    }

    fn values<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::iterate(a, this, IterKind::Values)
    }

    fn keys<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::iterate(a, this, IterKind::Keys)
    }

    fn entries<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::iterate(a, this, IterKind::Entries)
    }
}

impl<'scope> Installer<'_, 'scope> {
    pub(crate) fn typed_arrays(&mut self) {
        let r = self.context;
        let base = r.typed_array_ctor;
        let proto = r.typed_array_proto;
        base.borrow_mut(self.mc).kind = ObjectKind::Native(j8framework::Native::constructor(Typed::abstract_ctor));
        r.name_function(self.mc, base, "TypedArray", Some(0));
        base.borrow_mut(self.mc).define("prototype", Property { slot: Slot::Data(proto.into()), enumerable: false, writable: false, configurable: false });
        self.set(proto, "constructor", base.into());
        self.method(base, "from", Typed::from, 1);
        self.method(base, "of", Typed::of, 0);
        let species = r.named_native(self.mc, Typed::species, "get [Symbol.species]", 0);
        base.borrow_mut(self.mc).define_symbol(r.symbols.species, Property { slot: Slot::Accessor { get: Some(species), set: None }, enumerable: false, writable: false, configurable: true });
        self.getter(proto, "buffer", Typed::buffer);
        self.getter(proto, "byteLength", Typed::byte_length);
        self.getter(proto, "byteOffset", Typed::byte_offset);
        self.getter(proto, "length", Typed::length);
        let tag = r.named_native(self.mc, Typed::to_string_tag, "get [Symbol.toStringTag]", 0);
        proto.borrow_mut(self.mc).define_symbol(r.symbols.to_string_tag, Property { slot: Slot::Accessor { get: Some(tag), set: None }, enumerable: false, writable: false, configurable: true });
        let methods: [(&str, j8framework::NativeFn, u32); 30] = [
            ("at", Typed::at, 1),
            ("copyWithin", Typed::copy_within, 2),
            ("entries", Typed::entries, 0),
            ("every", Typed::every, 1),
            ("fill", Typed::fill, 1),
            ("filter", Typed::filter, 1),
            ("find", Typed::find, 1),
            ("findIndex", Typed::find_index, 1),
            ("findLast", Typed::find_last, 1),
            ("findLastIndex", Typed::find_last_index, 1),
            ("forEach", Typed::for_each, 1),
            ("includes", Typed::includes, 1),
            ("indexOf", Typed::index_of, 1),
            ("join", Typed::join, 1),
            ("keys", Typed::keys, 0),
            ("lastIndexOf", Typed::last_index_of, 1),
            ("map", Typed::map, 1),
            ("reduce", Typed::reduce, 1),
            ("reduceRight", Typed::reduce_right, 1),
            ("reverse", Typed::reverse, 0),
            ("set", Typed::set, 1),
            ("slice", Typed::slice, 2),
            ("some", Typed::some, 1),
            ("sort", Typed::sort, 1),
            ("subarray", Typed::subarray, 2),
            ("toLocaleString", Typed::to_locale_string, 0),
            ("toReversed", Typed::to_reversed, 0),
            ("toSorted", Typed::to_sorted, 1),
            ("with", Typed::with, 2),
            ("values", Typed::values, 0),
        ];
        for (name, f, n) in methods {
            self.method(proto, name, f, n);
        }
        let values = proto.borrow().own(self.mc, "values").unwrap_or_default();
        proto.borrow_mut(self.mc).define_symbol(r.symbols.iterator, Property::hidden(values));
        let to_string = r.array_proto.borrow().own(self.mc, "toString").unwrap_or_default();
        self.set(proto, "toString", to_string);
        let ctors: [j8framework::NativeFn; 12] = [
            Typed::construct::<0>,
            Typed::construct::<1>,
            Typed::construct::<2>,
            Typed::construct::<3>,
            Typed::construct::<4>,
            Typed::construct::<5>,
            Typed::construct::<6>,
            Typed::construct::<7>,
            Typed::construct::<8>,
            Typed::construct::<9>,
            Typed::construct::<10>,
            Typed::construct::<11>,
        ];
        for (i, (kind, f)) in ElementKind::ALL.into_iter().zip(ctors).enumerate() {
            let c = r.typed_ctors[i];
            let p = r.typed_protos[i];
            c.borrow_mut(self.mc).kind = ObjectKind::Native(j8framework::Native::constructor(f));
            r.name_function(self.mc, c, kind.name(), Some(3));
            let fixed = |v: Value<'scope>| Property { slot: Slot::Data(v), enumerable: false, writable: false, configurable: false };
            #[expect(clippy::cast_precision_loss, reason = "element size is 1 / 2 / 4 / 8 bytes")]
            let bytes = Value::Number(kind.size() as f64);
            c.borrow_mut(self.mc).define("prototype", fixed(p.into()));
            c.borrow_mut(self.mc).define("BYTES_PER_ELEMENT", fixed(bytes));
            p.borrow_mut(self.mc).define("BYTES_PER_ELEMENT", fixed(bytes));
            self.set(p, "constructor", c.into());
            self.set(r.global, kind.name(), c.into());
        }
    }
}
