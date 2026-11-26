use j8framework::ValueKind;
use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

use j8framework::{Scope, Callback, Error, ErrorKind, JsObject, ObjectData, ObjectKind, Property, Slot, Value, ValueBits};

use crate::{Args, Installer};

struct Arrays;

const MAX_LENGTH: f64 = 9_007_199_254_740_991.0;

const MAX_ARRAY_LENGTH: f64 = 4_294_967_295.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Visit {
    ForEach,
    Map,
    Filter,
    Some,
    Every,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Find {
    Value,
    Index,
    LastValue,
    LastIndex,
}

impl Arrays {
    fn target<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>) -> Result<(JsObject<'scope>, f64), Error> {
        let o = a.to_object(this)?;
        let len = a.length_of_array_like(o.into())?;
        Ok((o, len))
    }

    fn create<'scope>(a: &mut Scope<'scope, '_>, len: f64, proto: Option<JsObject<'scope>>) -> Result<JsObject<'scope>, Error> {
        if len > MAX_ARRAY_LENGTH {
            return Err(a.throw(ErrorKind::Range, "Invalid array length"));
        }
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "checked to be in [0, 2^32 − 1]")]
        let n = len as u32;
        let arr = ObjectData { kind: ObjectKind::Array, proto: Some(proto.unwrap_or(a.context.array_proto)), ..ObjectData::default() }.alloc(a.mc);
        arr.borrow_mut(a.mc).set_array_length(n);
        Ok(arr)
    }

    fn species_create<'scope>(a: &mut Scope<'scope, '_>, original: JsObject<'scope>, len: f64) -> Result<(JsObject<'scope>, bool), Error> {
        if !a.is_array(original.into())? {
            return Ok((Self::create(a, len, None)?, true));
        }
        let mut c = a.get_named(original.into(), "constructor")?;
        if c.as_object().is_some() {
            c = a.get_symbol(c, a.context.symbols.species, c)?;
            if c == Value::Null {
                c = Value::Undefined;
            }
        }
        if c == Value::Undefined {
            return Ok((Self::create(a, len, None)?, true));
        }
        if !a.is_constructor(c) {
            return Err(a.throw(ErrorKind::Type, "object.constructor[Symbol.species] is not a constructor"));
        }
        let intrinsic = c.as_object().and_then(|o| o.native()).is_some_and(|n| core::ptr::fn_addr_eq(n.f, Self::array as j8framework::NativeFn));
        match (a.construct(c, &[len.into()], c)?).kind() {
            ValueKind::Object(o) => Ok((o, intrinsic && o.is_array() && !o.non_extensible() && !o.length_readonly())),
            _ => Err(a.throw(ErrorKind::Type, "species constructor returned a non-object")),
        }
    }

    fn set_length<'scope>(a: &mut Scope<'scope, '_>, o: JsObject<'scope>, n: f64) -> Result<(), Error> {
        a.set_named(o.into(), "length", n.into())
    }

    fn relative<'scope>(a: &mut Scope<'scope, '_>, v: Value<'scope>, len: f64, default: f64) -> Result<f64, Error> {
        if v == Value::Undefined {
            return Ok(default);
        }
        let x = a.to_integer_or_infinity(v)?;
        Ok(if x < 0.0 { (len + x).max(0.0) } else { x.min(len) })
    }

    fn callback<'scope>(a: &mut Scope<'scope, '_>, f: Value<'scope>) -> Result<(), Error> {
        if a.is_callable(f) {
            Ok(())
        } else {
            let t = a.describe(f);
            Err(a.throw(ErrorKind::Type, &alloc::format!("{t} is not a function")))
        }
    }

    fn array<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let new_target = a.new_target;
        let proto = a.proto_from_ctor(new_target, a.context.array_proto)?;
        match *args {
            [] => Ok(Self::create(a, 0.0, Some(proto))?.into()),
            [v] if let Some(n) = v.as_number() => {
                let len = a.to_uint32(Value::Number(n))?;
                if f64::from(len) != n {
                    return Err(a.throw(ErrorKind::Range, "Invalid array length"));
                }
                Ok(Self::create(a, f64::from(len), Some(proto))?.into())
            }
            ref items => {
                let arr = a.array(items.to_vec());
                arr.borrow_mut(a.mc).set_proto(Some(proto));
                Ok(arr.into())
            }
        }
    }

    fn is_array<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Ok(a.is_array(args.at(0))?.into())
    }

    fn construct_this<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, len: Option<f64>) -> Result<JsObject<'scope>, Error> {
        if a.is_constructor(this) {
            let args: Vec<Value<'scope>> = len.map(Value::from).into_iter().collect();
            match (a.construct(this, &args, this)?).kind() {
                ValueKind::Object(o) => Ok(o),
                _ => Err(a.throw(ErrorKind::Type, "constructor returned a non-object")),
            }
        } else {
            Self::create(a, len.unwrap_or(0.0), None)
        }
    }

    fn of<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        #[expect(clippy::cast_precision_loss, reason = "argument count is far below 2^53")]
        let len = args.len() as f64;
        let arr = Self::construct_this(a, this, Some(len))?;
        for (k, &v) in (0..).zip(args) {
            a.create_index(arr, f64::from(k), v)?;
        }
        Self::set_length(a, arr, len)?;
        Ok(arr.into())
    }

    fn from<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (items, mapper, this_arg) = (args.at(0), args.at(1), args.at(2));
        let mapping = mapper != Value::Undefined;
        if mapping {
            Self::callback(a, mapper)?;
        }
        let using = if items.is_nullish() { Value::Undefined } else { a.get_symbol(items, a.context.symbols.iterator, items)? };
        if !using.is_nullish() {
            if !a.is_callable(using) {
                return Err(a.throw(ErrorKind::Type, "Symbol.iterator is not a function"));
            }
            let arr = Self::construct_this(a, this, None)?;
            let iterator = a.call(using, items, &[])?;
            if iterator.as_object().is_none() {
                return Err(a.throw(ErrorKind::Type, "Result of the Symbol.iterator method is not an object"));
            }
            let next = a.get_named(iterator, "next")?;
            let it = j8framework::IterRecord { iterator, next };
            let mut k = 0.0;
            while let Some(v) = a.iterator_step(it)? {
                let r = (|| {
                    if k >= MAX_LENGTH {
                        return Err(a.throw(ErrorKind::Type, "Array.from: too many elements"));
                    }
                    let v = if mapping { a.call(mapper, this_arg, &[v, k.into()])? } else { v };
                    a.create_index(arr, k, v)
                })();
                if let Err(e) = r {
                    return Err(a.iterator_close_abrupt(iterator, e));
                }
                k += 1.0;
            }
            Self::set_length(a, arr, k)?;
            return Ok(arr.into());
        }
        let like = a.to_object(items)?;
        let len = a.length_of_array_like(like.into())?;
        let arr = Self::construct_this(a, this, Some(len))?;
        let mut k = 0.0;
        while k < len {
            let v = a.get_index(like.into(), k)?;
            let v = if mapping { a.call(mapper, this_arg, &[v, k.into()])? } else { v };
            a.create_index(arr, k, v)?;
            k += 1.0;
        }
        Self::set_length(a, arr, len)?;
        Ok(arr.into())
    }

    fn species<'scope>(_: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Ok(this)
    }

    fn at<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, len) = Self::target(a, this)?;
        let rel = a.to_integer_or_infinity(args.at(0))?;
        let k = if rel >= 0.0 { rel } else { len + rel };
        if k < 0.0 || k >= len {
            return Ok(Value::Undefined);
        }
        a.get_index(o.into(), k)
    }

    fn push<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, len) = Self::target(a, this)?;
        if Self::dense_object(a, o) {
            for &v in args {
                o.elements().push(a.mc, v);
            }
            #[expect(clippy::cast_precision_loss, reason = "array length is at most 2^32")]
            let n = o.elements().len() as f64;
            return Ok(n.into());
        }
        #[expect(clippy::cast_precision_loss, reason = "argument count is far below 2^53")]
        let count = args.len() as f64;
        if len + count > MAX_LENGTH {
            return Err(a.throw(ErrorKind::Type, "Pushing elements past 2^53 - 1 length"));
        }
        let mut n = len;
        for &v in args {
            a.set_index(o.into(), n, v)?;
            n += 1.0;
        }
        Self::set_length(a, o, n)?;
        Ok(n.into())
    }

    fn indexed_clean<'scope>(a: &Scope<'scope, '_>, o: JsObject<'scope>) -> bool {
        match o.proto() {
            Some(p) if j8framework::ScopeLocal::ptr_eq(p, a.context.array_proto) && a.context.indexed_fast() && p.elements().is_empty() => true,
            _ => o.no_indexed_protos(),
        }
    }

    fn dense_object<'scope>(a: &Scope<'scope, '_>, o: JsObject<'scope>) -> bool {
        o.is_array() && !o.non_extensible() && !o.length_readonly() && o.sparse_length() == 0 && Self::indexed_clean(a, o)
    }

    fn dense_array<'scope>(a: &Scope<'scope, '_>, this: Value<'scope>) -> Option<JsObject<'scope>> {
        let o = this.as_object()?;
        Self::dense_object(a, o).then_some(o)
    }

    fn push_values<'scope>(a: &mut Scope<'scope, '_>, o: JsObject<'scope>, vs: &[Value<'scope>]) -> Option<Value<'scope>> {
        let n = o.elements().len();
        if n.checked_add(vs.len()).is_none_or(|t| t > usize::try_from(u32::MAX).unwrap_or(usize::MAX)) {
            return None;
        }
        for &v in vs {
            o.elements().push(a.mc, v);
        }
        #[expect(clippy::cast_precision_loss, reason = "array length is at most 2^32")]
        Some(Value::Number(o.elements().len() as f64))
    }

    fn push1<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, v: Value<'scope>) -> Option<Value<'scope>> {
        let o = Self::dense_array(a, this)?;
        let n = o.elements().len();
        if n >= usize::try_from(u32::MAX).unwrap_or(usize::MAX) {
            return None;
        }
        o.elements().push(a.mc, v);
        #[expect(clippy::cast_precision_loss, reason = "array length is at most 2^32")]
        Some(Value::Number((n + 1) as f64))
    }

    fn push2<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, x: Value<'scope>, y: Value<'scope>) -> Option<Value<'scope>> {
        let o = Self::dense_array(a, this)?;
        Self::push_values(a, o, &[x, y])
    }

    fn pop0<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>) -> Option<Value<'scope>> {
        let o = Self::dense_array(a, this)?;
        Some(o.elements().pop(a.mc).unwrap_or(Value::Undefined))
    }

    fn pop<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, len) = Self::target(a, this)?;
        if len == 0.0 {
            Self::set_length(a, o, 0.0)?;
            return Ok(Value::Undefined);
        }
        let new_len = len - 1.0;
        let v = a.get_index(o.into(), new_len)?;
        a.delete_index(o, new_len)?;
        Self::set_length(a, o, new_len)?;
        Ok(v)
    }

    fn shift<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, len) = Self::target(a, this)?;
        if len == 0.0 {
            Self::set_length(a, o, 0.0)?;
            return Ok(Value::Undefined);
        }
        let first = a.get_index(o.into(), 0.0)?;
        if Self::dense_object(a, o) && !o.elements().is_empty() {
            o.elements().remove(a.mc, 0);
            return Ok(first);
        }
        Self::move_range(a, o, 1.0, 0.0, len - 1.0)?;
        a.delete_index(o, len - 1.0)?;
        Self::set_length(a, o, len - 1.0)?;
        Ok(first)
    }

    fn move_range<'scope>(a: &mut Scope<'scope, '_>, o: JsObject<'scope>, from: f64, to: f64, count: f64) -> Result<(), Error> {
        let step = |a: &mut Scope<'scope, '_>, f: f64, t: f64| -> Result<(), Error> {
            if let Some(v) = a.present_index(o, f)? {
                a.set_index(o.into(), t, v)
            } else {
                a.delete_index(o, t)
            }
        };
        if from > to {
            let mut k = 0.0;
            while k < count {
                step(a, from + k, to + k)?;
                k += 1.0;
            }
        } else {
            let mut k = count;
            while k > 0.0 {
                step(a, from + k - 1.0, to + k - 1.0)?;
                k -= 1.0;
            }
        }
        Ok(())
    }

    fn unshift<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, len) = Self::target(a, this)?;
        #[expect(clippy::cast_precision_loss, reason = "argument count is far below 2^53")]
        let count = args.len() as f64;
        if count > 0.0 {
            if len + count > MAX_LENGTH {
                return Err(a.throw(ErrorKind::Type, "Unshifting elements past 2^53 - 1 length"));
            }
            Self::move_range(a, o, 0.0, count, len)?;
            for (j, &v) in (0..).zip(args) {
                a.set_index(o.into(), f64::from(j), v)?;
            }
        }
        Self::set_length(a, o, len + count)?;
        Ok((len + count).into())
    }

    fn slice<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, len) = Self::target(a, this)?;
        let mut k = Self::relative(a, args.at(0), len, 0.0)?;
        let end = Self::relative(a, args.at(1), len, len)?;
        let count = (end - k).max(0.0);
        let (arr, _) = Self::species_create(a, o, count)?;
        let mut n = 0.0;
        while k < end {
            if let Some(v) = a.present_index(o, k)? {
                a.create_index(arr, n, v)?;
            }
            k += 1.0;
            n += 1.0;
        }
        Self::set_length(a, arr, n)?;
        Ok(arr.into())
    }

    fn splice<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, len) = Self::target(a, this)?;
        let start = Self::relative(a, args.at(0), len, 0.0)?;
        let items = args.get(2..).unwrap_or(&[]);
        #[expect(clippy::cast_precision_loss, reason = "argument count is far below 2^53")]
        let item_count = items.len() as f64;
        let delete_count = match args.len() {
            0 => 0.0,
            1 => len - start,
            _ => a.to_integer_or_infinity(args[1])?.clamp(0.0, len - start),
        };
        if len + item_count - delete_count > MAX_LENGTH {
            return Err(a.throw(ErrorKind::Type, "Splice result exceeds 2^53 - 1 length"));
        }
        let (removed, _) = Self::species_create(a, o, delete_count)?;
        let mut k = 0.0;
        while k < delete_count {
            if a.has_index(o, start + k)? {
                let v = a.get_index(o.into(), start + k)?;
                a.create_index(removed, k, v)?;
            }
            k += 1.0;
        }
        Self::set_length(a, removed, delete_count)?;
        Self::shift_tail(a, o, len, start, delete_count, item_count)?;
        for (j, &v) in (0..).zip(items) {
            a.set_index(o.into(), start + f64::from(j), v)?;
        }
        Self::set_length(a, o, len - delete_count + item_count)?;
        Ok(removed.into())
    }

    fn shift_tail<'scope>(a: &mut Scope<'scope, '_>, o: JsObject<'scope>, len: f64, start: f64, delete_count: f64, item_count: f64) -> Result<(), Error> {
        if item_count < delete_count {
            Self::move_range(a, o, start + delete_count, start + item_count, len - start - delete_count)?;
            let mut k = len;
            while k > len - delete_count + item_count {
                a.delete_index(o, k - 1.0)?;
                k -= 1.0;
            }
        } else if item_count > delete_count {
            Self::move_range(a, o, start + delete_count, start + item_count, len - start - delete_count)?;
        }
        Ok(())
    }

    fn spreadable<'scope>(a: &mut Scope<'scope, '_>, v: Value<'scope>) -> Result<bool, Error> {
        if v.as_object().is_none() {
            return Ok(false);
        }
        let s = a.get_symbol(v, a.context.symbols.is_concat_spreadable, v)?;
        if s != Value::Undefined {
            return Ok(a.to_boolean(s));
        }
        a.is_array(v)
    }

    fn concat<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let o = a.to_object(this)?;
        let (arr, _) = Self::species_create(a, o, 0.0)?;
        let mut n = 0.0;
        for e in core::iter::once(Value::Object(o)).chain(args.iter().copied()) {
            if Self::spreadable(a, e)? {
                let Some(eo) = e.as_object() else { unreachable!("spreadable values are objects") };
                let len = a.length_of_array_like(e)?;
                if n + len > MAX_LENGTH {
                    return Err(a.throw(ErrorKind::Type, "Concatenation exceeds 2^53 - 1 length"));
                }
                let mut k = 0.0;
                while k < len {
                    if a.has_index(eo, k)? {
                        let v = a.get_index(e, k)?;
                        a.create_index(arr, n, v)?;
                    }
                    n += 1.0;
                    k += 1.0;
                }
            } else {
                if n >= MAX_LENGTH {
                    return Err(a.throw(ErrorKind::Type, "Concatenation exceeds 2^53 - 1 length"));
                }
                a.create_index(arr, n, e)?;
                n += 1.0;
            }
        }
        Self::set_length(a, arr, n)?;
        Ok(arr.into())
    }

    fn join<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, len) = Self::target(a, this)?;
        let sep = match args.at(0).kind() {
            ValueKind::Undefined => a.string(","),
            v => a.to_string(Value::from(v))?,
        };
        if let Some(s) = Self::join_plain(a, o, len, &sep) {
            return Ok(a.string(&s).into());
        }
        Self::unless_joining(a, o, |a| {
            let mut out = String::new();
            let mut k = 0.0;
            while k < len {
                if k > 0.0 {
                    out.push_str(&sep);
                }
                let v = a.get_index(o.into(), k)?;
                if !v.is_nullish() {
                    out.push_str(&a.to_string(v)?);
                }
                k += 1.0;
            }
            Ok(out)
        })
    }

    fn unless_joining<'scope, 'x>(a: &mut Scope<'scope, 'x>, o: JsObject<'scope>, join: impl FnOnce(&mut Scope<'scope, 'x>) -> Result<String, Error>) -> Result<Value<'scope>, Error> {
        if !a.joining().enter(o) {
            return Ok(a.string("").into());
        }
        let joined = join(a);
        a.joining().leave();
        Ok(a.string(&joined?).into())
    }

    fn join_plain<'scope>(a: &Scope<'scope, '_>, o: JsObject<'scope>, len: f64, sep: &str) -> Option<String> {
        #[derive(Clone, Copy)]
        enum Part<'s> {
            Number(f64),
            Text(&'s str),
        }
        if !o.is_array() {
            return None;
        }
        let d = o.borrow();
        let items = o.elements().to_vec();
        #[expect(clippy::cast_precision_loss, reason = "element count is far below 2^53")]
        let dense = items.len() as f64 == len && d.sparse_length() == 0;
        if !dense || !items.iter().all(|v| matches!(v.kind(), ValueKind::Number(_) | ValueKind::String(_) | ValueKind::Bool(_) | ValueKind::Null | ValueKind::Undefined)) {
            return None;
        }
        let parts: Vec<Part<'_>> = items
            .iter()
            .map(|v| match v.kind() {
                ValueKind::Number(x) => Part::Number(x),
                ValueKind::String(s) => Part::Text(j8framework::ScopeLocal::get(s)),
                ValueKind::Bool(true) => Part::Text("true"),
                ValueKind::Bool(false) => Part::Text("false"),
                _ => Part::Text(""),
            })
            .collect();
        let pieces = a.par().chunks(&parts, &|chunk: &[Part<'_>]| {
            let mut s = String::new();
            for (i, p) in chunk.iter().enumerate() {
                if i > 0 {
                    s.push_str(sep);
                }
                match *p {
                    Part::Number(x) => s.push_str(&j8framework::number_text(x)),
                    Part::Text(t) => s.push_str(t),
                }
            }
            s
        });
        let total = pieces.iter().map(String::len).sum::<usize>() + sep.len() * pieces.len().saturating_sub(1);
        let mut out = String::with_capacity(total);
        for (i, p) in pieces.iter().enumerate() {
            if i > 0 {
                out.push_str(sep);
            }
            out.push_str(p);
        }
        Some(out)
    }

    fn to_string<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let o = a.to_object(this)?;
        let join = a.get_named(o.into(), "join")?;
        if a.is_callable(join) {
            return a.call(join, o.into(), &[]);
        }
        let f = a.get_named(a.context.object_proto.into(), "toString")?;
        a.call(f, o.into(), &[])
    }

    fn to_locale_string<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, len) = Self::target(a, this)?;
        Self::unless_joining(a, o, |a| crate::LocaleJoin::join(a, o, len, args))
    }

    fn reverse<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, len) = Self::target(a, this)?;
        let middle = libm::floor(len / 2.0);
        let mut lower = 0.0;
        while lower != middle {
            let upper = len - lower - 1.0;
            let lower_exists = a.has_index(o, lower)?;
            let lower_value = if lower_exists { a.get_index(o.into(), lower)? } else { Value::Undefined };
            let upper_exists = a.has_index(o, upper)?;
            let upper_value = if upper_exists { a.get_index(o.into(), upper)? } else { Value::Undefined };
            match (lower_exists, upper_exists) {
                (true, true) => {
                    a.set_index(o.into(), lower, upper_value)?;
                    a.set_index(o.into(), upper, lower_value)?;
                }
                (false, true) => {
                    a.set_index(o.into(), lower, upper_value)?;
                    a.delete_index(o, upper)?;
                }
                (true, false) => {
                    a.delete_index(o, lower)?;
                    a.set_index(o.into(), upper, lower_value)?;
                }
                (false, false) => {}
            }
            lower += 1.0;
        }
        Ok(o.into())
    }

    fn index_of<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, len) = Self::target(a, this)?;
        if len == 0.0 {
            return Ok((-1.0).into());
        }
        let n = a.to_integer_or_infinity(args.at(1))?;
        if n == f64::INFINITY {
            return Ok((-1.0).into());
        }
        let mut k = if n >= 0.0 { n } else { (len + n).max(0.0) };
        let want = args.at(0);
        while k < len {
            if a.has_index(o, k)? && a.get_index(o.into(), k)? == want {
                return Ok(k.into());
            }
            k += 1.0;
        }
        Ok((-1.0).into())
    }

    fn last_index_of<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, len) = Self::target(a, this)?;
        if len == 0.0 {
            return Ok((-1.0).into());
        }
        let n = if args.len() > 1 { a.to_integer_or_infinity(args[1])? } else { len - 1.0 };
        if n == f64::NEG_INFINITY {
            return Ok((-1.0).into());
        }
        let mut k = if n >= 0.0 { n.min(len - 1.0) } else { len + n };
        let want = args.at(0);
        while k >= 0.0 {
            if a.has_index(o, k)? && a.get_index(o.into(), k)? == want {
                return Ok(k.into());
            }
            k -= 1.0;
        }
        Ok((-1.0).into())
    }

    fn same_value_zero<'scope>(x: Value<'scope>, y: Value<'scope>) -> bool {
        x == y || matches!((x.kind(), y.kind()), (ValueKind::Number(p), ValueKind::Number(q)) if p.is_nan() && q.is_nan())
    }

    fn includes<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, len) = Self::target(a, this)?;
        if len == 0.0 {
            return Ok(false.into());
        }
        let n = a.to_integer_or_infinity(args.at(1))?;
        if n == f64::INFINITY {
            return Ok(false.into());
        }
        let mut k = if n >= 0.0 { n } else { (len + n).max(0.0) };
        let want = args.at(0);
        while k < len {
            if Self::same_value_zero(a.get_index(o.into(), k)?, want) {
                return Ok(true.into());
            }
            k += 1.0;
        }
        Ok(false.into())
    }

    fn span(len: f64) -> Option<u32> {
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "rejected unless the value is an integer in range")]
        let n = len as u32;
        (len >= 0.0 && f64::from(n) == len).then_some(n)
    }

    #[inline]
    fn dense<'scope>(o: JsObject<'scope>, i: usize, run: &mut Option<j8framework::ElementRun<'scope>>) -> Option<Value<'scope>> {
        if !o.is_array() {
            return None;
        }
        let els = o.elements();
        if let Some(r) = *run
            && let Some(v) = r.get(els, i)
        {
            return Some(v);
        }
        let next = els.run(i);
        let v = next.and_then(|r| r.get(els, i));
        *run = next;
        v
    }

    #[inline]
    fn load<'scope>(a: &mut Scope<'scope, '_>, o: JsObject<'scope>, k: f64, idx: Option<u32>, run: &mut Option<j8framework::ElementRun<'scope>>, skip_hole: bool) -> Result<Option<Value<'scope>>, Error> {
        if let Some(i) = idx
            && let Some(v) = Self::dense(o, i as usize, run)
        {
            return Ok(Some(v));
        }
        if skip_hole { a.present_index(o, k) } else { Ok(Some(a.get_index(o.into(), k)?)) }
    }

    #[inline]
    fn append_fresh<'scope>(a: &Scope<'scope, '_>, arr: JsObject<'scope>, v: Value<'scope>, at: u32) -> bool {
        let items = arr.elements();
        if items.len() != at as usize {
            return false;
        }
        items.push(a.mc, v);
        true
    }

    #[inline]
    fn apply<'scope>(a: &mut Scope<'scope, '_>, cb: &Callback<'scope>, n: usize, v0: Value<'scope>, v1: impl FnOnce() -> Value<'scope>, v2: impl FnOnce() -> Value<'scope>, v3: impl FnOnce() -> Value<'scope>) -> Result<Value<'scope>, Error> {
        let k = match cb.passed() {
            Some(p) => p.min(n),
            None => n,
        };
        match k {
            0 => cb.call(a, &[]),
            1 => cb.call(a, &[v0]),
            2 => cb.call(a, &[v0, v1()]),
            3 => cb.call(a, &[v0, v1(), v2()]),
            _ => cb.call(a, &[v0, v1(), v2(), v3()]),
        }
    }

    #[expect(clippy::too_many_arguments, reason = "one step of a visiting loop")]
    #[inline]
    fn drive<'scope>(a: &mut Scope<'scope, '_>, cb: &mut Callback<'scope>, how: Visit, source: &mut JsObject<'scope>, out: &mut Option<JsObject<'scope>>, fresh: bool, k: f64, idx: Option<u32>, run: &mut Option<j8framework::ElementRun<'scope>>, to: &mut f64) -> Result<Option<Value<'scope>>, Error> {
        let src = *source;
        let Some(v) = Self::load(a, src, k, idx, run, true)? else { return Ok(None) };
        let mut held = [src.into(), out.map_or(Value::Undefined, Into::into), v, cb.callee, cb.this];
        let (r, moved) = a.holding(&mut held, |a| Self::apply(a, cb, 3, v, || Value::from(k), || src.into(), || Value::Undefined));
        let r = r?;
        let v = if moved {
            let [s, o, v, callee, this] = held;
            *source = s.as_object().expect("the held source");
            *out = o.as_object();
            *cb = a.callback(callee, this);
            *run = None;
            v
        } else {
            v
        };
        match (how, *out) {
            (Visit::Map, Some(arr)) => {
                if !(fresh && idx.is_some_and(|i| Self::append_fresh(a, arr, r, i))) {
                    a.create_index(arr, k, r)?;
                }
            }
            (Visit::Filter, Some(arr)) if a.to_boolean(r) => {
                let at = *to;
                #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "rejected unless the value is an integer in range")]
                let n = at as u32;
                if !(fresh && f64::from(n) == at && Self::append_fresh(a, arr, v, n)) {
                    a.create_index(arr, at, v)?;
                }
                *to += 1.0;
            }
            (Visit::Some, _) if a.to_boolean(r) => return Ok(Some(true.into())),
            (Visit::Every, _) if !a.to_boolean(r) => return Ok(Some(false.into())),
            _ => {}
        }
        Ok(None)
    }

    #[expect(clippy::inline_always, reason = "the per-element call has to sit in the specialised loop")]
    #[inline(always)]
    fn call3<'scope>(a: &mut Scope<'scope, '_>, cb: &Callback<'scope>, arity: usize, quiet: bool, v: Value<'scope>, k: f64, source: JsObject<'scope>) -> Result<Value<'scope>, Error> {
        match arity {
            0 => Self::pass(a, cb, quiet, &[]),
            1 => Self::pass(a, cb, quiet, &[v]),
            2 => Self::pass(a, cb, quiet, &[v, Value::from(k)]),
            _ => Self::pass(a, cb, quiet, &[v, Value::from(k), source.into()]),
        }
    }

    #[expect(clippy::inline_always, reason = "the per-element call has to sit in the specialised loop")]
    #[inline(always)]
    fn pass<'scope>(a: &mut Scope<'scope, '_>, cb: &Callback<'scope>, quiet: bool, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        if quiet { cb.again(a, args) } else { cb.call(a, args) }
    }

    #[expect(clippy::inline_always, reason = "the per-element call has to sit in the specialised loop")]
    #[inline(always)]
    fn call_fold<'scope>(a: &mut Scope<'scope, '_>, cb: &Callback<'scope>, arity: usize, quiet: bool, acc: Value<'scope>, v: Value<'scope>, k: f64) -> Result<Value<'scope>, Error> {
        match arity {
            0 => Self::pass(a, cb, quiet, &[]),
            1 => Self::pass(a, cb, quiet, &[acc]),
            2 => Self::pass(a, cb, quiet, &[acc, v]),
            _ => Self::pass(a, cb, quiet, &[acc, v, Value::from(k)]),
        }
    }

    #[expect(clippy::inline_always, reason = "the per-element load has to sit in the specialised loop")]
    #[inline(always)]
    fn dense_or<'scope>(a: &mut Scope<'scope, '_>, o: JsObject<'scope>, i: u32, run: &mut Option<j8framework::ElementRun<'scope>>, skip_hole: bool, quiet: &mut bool) -> Result<Option<Value<'scope>>, Error> {
        if let Some(v) = Self::dense(o, i as usize, run) {
            return Ok(Some(v));
        }
        *quiet = false;
        Self::absent(a, o, f64::from(i), skip_hole)
    }

    #[cold]
    #[inline(never)]
    fn absent<'scope>(a: &mut Scope<'scope, '_>, o: JsObject<'scope>, k: f64, skip_hole: bool) -> Result<Option<Value<'scope>>, Error> {
        if skip_hole { a.present_index(o, k) } else { Ok(Some(a.get_index(o.into(), k)?)) }
    }

    #[cold]
    #[inline(never)]
    fn write_at<'scope>(a: &mut Scope<'scope, '_>, arr: JsObject<'scope>, k: f64, v: Value<'scope>) -> Result<(), Error> {
        a.create_index(arr, k, v)
    }

    #[inline(never)]
    fn each_fast<'scope>(a: &mut Scope<'scope, '_>, o: JsObject<'scope>, n: u32, cb: &Callback<'scope>, arity: usize) -> Result<(), Error> {
        let mut run = None;
        let mut quiet = false;
        for i in 0..n {
            let Some(v) = Self::dense_or(a, o, i, &mut run, true, &mut quiet)? else { continue };
            Self::call3(a, cb, arity, quiet, v, f64::from(i), o)?;
            quiet = true;
        }
        Ok(())
    }

    #[inline(never)]
    fn map_fast<'scope>(a: &mut Scope<'scope, '_>, o: JsObject<'scope>, arr: JsObject<'scope>, fresh: bool, n: u32, cb: &Callback<'scope>, arity: usize) -> Result<Value<'scope>, Error> {
        let mut run = None;
        let mut quiet = false;
        for i in 0..n {
            let Some(v) = Self::dense_or(a, o, i, &mut run, true, &mut quiet)? else { continue };
            let r = Self::call3(a, cb, arity, quiet, v, f64::from(i), o)?;
            quiet = fresh && Self::append_fresh(a, arr, r, i);
            if !quiet {
                Self::write_at(a, arr, f64::from(i), r)?;
            }
        }
        Ok(arr.into())
    }

    #[inline(never)]
    fn filter_fast<'scope>(a: &mut Scope<'scope, '_>, o: JsObject<'scope>, arr: JsObject<'scope>, fresh: bool, n: u32, cb: &Callback<'scope>, arity: usize) -> Result<Value<'scope>, Error> {
        let mut run = None;
        let mut to = 0u32;
        let mut quiet = false;
        for i in 0..n {
            let Some(v) = Self::dense_or(a, o, i, &mut run, true, &mut quiet)? else { continue };
            let r = Self::call3(a, cb, arity, quiet, v, f64::from(i), o)?;
            quiet = true;
            if a.to_boolean(r) {
                if !(fresh && Self::append_fresh(a, arr, v, to)) {
                    Self::write_at(a, arr, f64::from(to), v)?;
                    quiet = false;
                }
                to += 1;
            }
        }
        Ok(arr.into())
    }

    #[inline(never)]
    fn pred_fast<'scope>(a: &mut Scope<'scope, '_>, o: JsObject<'scope>, n: u32, cb: &Callback<'scope>, arity: usize, stop_on: bool) -> Result<Value<'scope>, Error> {
        let mut run = None;
        let mut quiet = false;
        for i in 0..n {
            let Some(v) = Self::dense_or(a, o, i, &mut run, true, &mut quiet)? else { continue };
            let r = Self::call3(a, cb, arity, quiet, v, f64::from(i), o)?;
            quiet = true;
            if a.to_boolean(r) == stop_on {
                return Ok(stop_on.into());
            }
        }
        Ok((!stop_on).into())
    }

    #[inline(never)]
    fn find_fast<'scope>(a: &mut Scope<'scope, '_>, o: JsObject<'scope>, n: u32, cb: &Callback<'scope>, arity: usize, how: Find) -> Result<Value<'scope>, Error> {
        let backward = matches!(how, Find::LastValue | Find::LastIndex);
        let value = matches!(how, Find::Value | Find::LastValue);
        let mut run = None;
        let mut quiet = false;
        for i in 0..n {
            let at = if backward { n - 1 - i } else { i };
            let Some(v) = Self::dense_or(a, o, at, &mut run, false, &mut quiet)? else { continue };
            let k = f64::from(at);
            let r = Self::call3(a, cb, arity, quiet, v, k, o)?;
            quiet = true;
            if a.to_boolean(r) {
                return Ok(if value { v } else { Value::from(k) });
            }
        }
        Ok(if value { Value::Undefined } else { (-1.0).into() })
    }

    #[inline(never)]
    fn fold_fast<'scope>(a: &mut Scope<'scope, '_>, o: JsObject<'scope>, n: u32, right: bool, cb: &Callback<'scope>, arity: usize, init: Option<Value<'scope>>) -> Result<Value<'scope>, Error> {
        let mut run = None;
        let mut quiet = false;
        let mut i = 0u32;
        let mut acc = if let Some(v) = init {
            v
        } else {
            loop {
                if i >= n {
                    return Err(a.throw(ErrorKind::Type, "Reduce of empty array with no initial value"));
                }
                let at = if right { n - 1 - i } else { i };
                i += 1;
                if let Some(v) = Self::dense_or(a, o, at, &mut run, true, &mut quiet)? {
                    break v;
                }
            }
        };
        quiet = false;
        while i < n {
            let at = if right { n - 1 - i } else { i };
            if let Some(v) = Self::dense_or(a, o, at, &mut run, true, &mut quiet)? {
                acc = Self::call_fold(a, cb, arity, quiet, acc, v, f64::from(at))?;
                quiet = true;
            }
            i += 1;
        }
        Ok(acc)
    }

    fn visit<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>], how: Visit) -> Result<Value<'scope>, Error> {
        let (o, len) = Self::target(a, this)?;
        let (f, this_arg) = (args.at(0), args.at(1));
        Self::callback(a, f)?;
        let (out, fresh) = match how {
            Visit::Map => {
                let (arr, fresh) = Self::species_create(a, o, len)?;
                (Some(arr), fresh)
            }
            Visit::Filter => {
                let (arr, fresh) = Self::species_create(a, o, 0.0)?;
                (Some(arr), fresh)
            }
            _ => (None, false),
        };
        let mut cb = a.callback(f, this_arg);
        let (mut o, mut out) = (o, out);
        if o.is_array()
            && let Some(n) = Self::span(len)
            && let Some(arity) = cb.passed()
        {
            return match how {
                Visit::ForEach => Self::each_fast(a, o, n, &cb, arity).map(|()| Value::Undefined),
                Visit::Map => Self::map_fast(a, o, out.expect("map builds an array"), fresh, n, &cb, arity),
                Visit::Filter => Self::filter_fast(a, o, out.expect("filter builds an array"), fresh, n, &cb, arity),
                Visit::Some => Self::pred_fast(a, o, n, &cb, arity, true),
                Visit::Every => Self::pred_fast(a, o, n, &cb, arity, false),
            };
        }
        let mut run = None;
        let mut to = 0.0;
        if let Some(n) = Self::span(len) {
            for i in 0..n {
                if let Some(done) = Self::drive(a, &mut cb, how, &mut o, &mut out, fresh, f64::from(i), Some(i), &mut run, &mut to)? {
                    return Ok(done);
                }
            }
        } else {
            let mut k = 0.0;
            while k < len {
                if let Some(done) = Self::drive(a, &mut cb, how, &mut o, &mut out, fresh, k, None, &mut run, &mut to)? {
                    return Ok(done);
                }
                k += 1.0;
            }
        }
        Ok(match (how, out) {
            (_, Some(arr)) => arr.into(),
            (Visit::Some, _) => false.into(),
            (Visit::Every, _) => true.into(),
            _ => Value::Undefined,
        })
    }

    fn for_each<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::visit(a, this, args, Visit::ForEach)
    }

    fn map<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::visit(a, this, args, Visit::Map)
    }

    fn filter<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::visit(a, this, args, Visit::Filter)
    }

    fn some<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::visit(a, this, args, Visit::Some)
    }

    fn every<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::visit(a, this, args, Visit::Every)
    }

    fn find_by<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>], how: Find) -> Result<Value<'scope>, Error> {
        let (o, len) = Self::target(a, this)?;
        let (f, this_arg) = (args.at(0), args.at(1));
        Self::callback(a, f)?;
        let backward = matches!(how, Find::LastValue | Find::LastIndex);
        let cb = a.callback(f, this_arg);
        if o.is_array()
            && let Some(n) = Self::span(len)
            && let Some(arity) = cb.passed()
        {
            return Self::find_fast(a, o, n, &cb, arity, how);
        }
        let mut run = None;
        let mut hit = |a: &mut Scope<'scope, '_>, k: f64, idx: Option<u32>| -> Result<Option<Value<'scope>>, Error> {
            let Some(v) = Self::load(a, o, k, idx, &mut run, false)? else { return Ok(None) };
            let r = Self::apply(a, &cb, 3, v, || Value::from(k), || o.into(), || Value::Undefined)?;
            Ok(a.to_boolean(r).then_some(if matches!(how, Find::Value | Find::LastValue) { v } else { Value::from(k) }))
        };
        if let Some(n) = Self::span(len) {
            for i in 0..n {
                let k = if backward { n - 1 - i } else { i };
                if let Some(v) = hit(a, f64::from(k), Some(k))? {
                    return Ok(v);
                }
            }
        } else {
            let mut i = 0.0;
            while i < len {
                let k = if backward { len - 1.0 - i } else { i };
                if let Some(v) = hit(a, k, None)? {
                    return Ok(v);
                }
                i += 1.0;
            }
        }
        Ok(if matches!(how, Find::Value | Find::LastValue) { Value::Undefined } else { (-1.0).into() })
    }

    fn find<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::find_by(a, this, args, Find::Value)
    }

    fn find_index<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::find_by(a, this, args, Find::Index)
    }

    fn find_last<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::find_by(a, this, args, Find::LastValue)
    }

    fn find_last_index<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::find_by(a, this, args, Find::LastIndex)
    }

    fn fold<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>], right: bool) -> Result<Value<'scope>, Error> {
        let (o, len) = Self::target(a, this)?;
        let f = args.at(0);
        Self::callback(a, f)?;
        let cb = a.callback(f, Value::Undefined);
        if o.is_array()
            && let Some(n) = Self::span(len)
            && let Some(arity) = cb.passed()
        {
            let init = (args.len() >= 2).then(|| args[1]);
            return Self::fold_fast(a, o, n, right, &cb, arity, init);
        }
        let mut run = None;
        if let Some(n) = Self::span(len) {
            let mut i = 0u32;
            let mut acc = if args.len() >= 2 {
                args[1]
            } else {
                loop {
                    if i >= n {
                        return Err(a.throw(ErrorKind::Type, "Reduce of empty array with no initial value"));
                    }
                    let k = if right { n - 1 - i } else { i };
                    i += 1;
                    if let Some(v) = Self::load(a, o, f64::from(k), Some(k), &mut run, true)? {
                        break v;
                    }
                }
            };
            while i < n {
                let k = if right { n - 1 - i } else { i };
                if let Some(v) = Self::load(a, o, f64::from(k), Some(k), &mut run, true)? {
                    acc = Self::apply(a, &cb, 4, acc, || v, || Value::from(f64::from(k)), || o.into())?;
                }
                i += 1;
            }
            return Ok(acc);
        }
        let index = |i: f64| if right { len - 1.0 - i } else { i };
        let mut i = 0.0;
        let mut acc = if args.len() >= 2 {
            args[1]
        } else {
            loop {
                if i >= len {
                    return Err(a.throw(ErrorKind::Type, "Reduce of empty array with no initial value"));
                }
                let k = index(i);
                i += 1.0;
                if let Some(v) = Self::load(a, o, k, None, &mut run, true)? {
                    break v;
                }
            }
        };
        while i < len {
            let k = index(i);
            if let Some(v) = Self::load(a, o, k, None, &mut run, true)? {
                acc = Self::apply(a, &cb, 4, acc, || v, || Value::from(k), || o.into())?;
            }
            i += 1.0;
        }
        Ok(acc)
    }

    fn reduce<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::fold(a, this, args, false)
    }

    fn reduce_right<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::fold(a, this, args, true)
    }

    fn fill<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, len) = Self::target(a, this)?;
        let mut k = Self::relative(a, args.at(1), len, 0.0)?;
        let end = Self::relative(a, args.at(2), len, len)?;
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "integers in [0, len], checked against u32 first")]
        if k < end && end < f64::from(u32::MAX) && o.borrow_mut(a.mc).fill_index(k as u32, end as u32, args.at(0)) {
            return Ok(o.into());
        }
        while k < end {
            a.set_index(o.into(), k, args.at(0))?;
            k += 1.0;
        }
        Ok(o.into())
    }

    fn copy_within<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, len) = Self::target(a, this)?;
        let to = Self::relative(a, args.at(0), len, 0.0)?;
        let from = Self::relative(a, args.at(1), len, 0.0)?;
        let end = Self::relative(a, args.at(2), len, len)?;
        let count = (end - from).min(len - to);
        if count > 0.0 {
            Self::move_range(a, o, from, to, count)?;
        }
        Ok(o.into())
    }

    #[expect(clippy::too_many_arguments, reason = "these are exactly the parameters of the spec's abstract operation")]
    fn flatten<'scope>(a: &mut Scope<'scope, '_>, target: JsObject<'scope>, source: JsObject<'scope>, len: f64, start: f64, depth: f64, mapper: Value<'scope>, this_arg: Value<'scope>) -> Result<f64, Error> {
        let mut to = start;
        let mut k = 0.0;
        while k < len {
            if a.has_index(source, k)? {
                let mut v = a.get_index(source.into(), k)?;
                if mapper != Value::Undefined {
                    v = a.call(mapper, this_arg, &[v, k.into(), source.into()])?;
                }
                if depth > 0.0 && a.is_array(v)? {
                    let Some(inner) = v.as_object() else { unreachable!("arrays are objects") };
                    let inner_len = a.length_of_array_like(v)?;
                    a.check_stack()?;
                    to = Self::flatten(a, target, inner, inner_len, to, depth - 1.0, Value::Undefined, Value::Undefined)?;
                } else {
                    if to >= MAX_LENGTH {
                        return Err(a.throw(ErrorKind::Type, "Flattened array exceeds 2^53 - 1 length"));
                    }
                    a.create_index(target, to, v)?;
                    to += 1.0;
                }
            }
            k += 1.0;
        }
        Ok(to)
    }

    fn flat<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, len) = Self::target(a, this)?;
        let depth = match args.at(0).kind() {
            ValueKind::Undefined => 1.0,
            d => a.to_integer_or_infinity(Value::from(d))?.max(0.0),
        };
        let (arr, _) = Self::species_create(a, o, 0.0)?;
        Self::flatten(a, arr, o, len, 0.0, depth, Value::Undefined, Value::Undefined)?;
        Ok(arr.into())
    }

    fn flat_map<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, len) = Self::target(a, this)?;
        Self::callback(a, args.at(0))?;
        let (arr, _) = Self::species_create(a, o, 0.0)?;
        Self::flatten(a, arr, o, len, 0.0, 1.0, args.at(0), args.at(1))?;
        Ok(arr.into())
    }

    fn order<'scope>(a: &mut Scope<'scope, '_>, cmp: Option<&Callback<'scope>>, quiet: &mut bool, x: Value<'scope>, y: Value<'scope>) -> Result<Ordering, Error> {
        Ok(match (x.bits() == ValueBits::UNDEFINED, y.bits() == ValueBits::UNDEFINED) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => match cmp {
                Some(cmp) => {
                    let r = match cmp.passed() {
                        Some(0) => Self::pass(a, cmp, *quiet, &[])?,
                        Some(1) => Self::pass(a, cmp, *quiet, &[x])?,
                        _ => Self::pass(a, cmp, *quiet, &[x, y])?,
                    };
                    let n = match r.as_number() {
                        Some(n) => {
                            *quiet = true;
                            n
                        }
                        None => {
                            *quiet = false;
                            a.to_number(r)?
                        }
                    };
                    n.partial_cmp(&0.0).unwrap_or(Ordering::Equal)
                }
                None => {
                    let (s, t) = (a.to_string(x)?, a.to_string(y)?);
                    s.encode_utf16().cmp(t.encode_utf16())
                }
            },
        })
    }

    fn sort_plain<'scope>(a: &Scope<'scope, '_>, items: &[Value<'scope>]) -> Option<Vec<Value<'scope>>> {
        if !items.iter().all(|v| matches!(v.kind(), ValueKind::Number(_) | ValueKind::String(_) | ValueKind::Bool(_) | ValueKind::Null | ValueKind::Undefined)) {
            return None;
        }
        let (undef, rest): (Vec<Value<'scope>>, Vec<Value<'scope>>) = items.iter().copied().partition(|v| *v == Value::Undefined);
        let numbers: Vec<f64> = rest.iter().filter_map(|v| v.as_number()).collect();
        let texts: Vec<String> = a.par().map(&numbers, &|&x| String::from(&*j8framework::number_text(x)));
        let mut next = texts.iter();
        let keys: Vec<&str> = rest
            .iter()
            .map(|v| match v.kind() {
                ValueKind::Number(_) => next.next().map_or("", String::as_str),
                ValueKind::String(s) => &**j8framework::ScopeLocal::get(s),
                ValueKind::Bool(true) => "true",
                ValueKind::Bool(false) => "false",
                _ => "null",
            })
            .collect();
        let bytewise = keys.iter().all(|k| !k.chars().any(|c| c >= '\u{E000}'));
        let mut order: Vec<u32> = (0..u32::try_from(keys.len()).ok()?).collect();
        let cmp = |&i: &u32, &j: &u32| {
            let (x, y) = (keys[i as usize], keys[j as usize]);
            if bytewise { x.cmp(y) } else { x.encode_utf16().cmp(y.encode_utf16()) }
        };
        a.par().sort_by(&mut order, &cmp);
        Some(order.iter().map(|&i| rest[i as usize]).chain(undef).collect())
    }

    fn merge_sort<'scope>(a: &mut Scope<'scope, '_>, f: Value<'scope>, mut v: Vec<Value<'scope>>) -> Result<Vec<Value<'scope>>, Error> {
        let n = v.len();
        if n <= 1 {
            return Ok(v);
        }
        let cmp = (f != Value::Undefined).then(|| a.callback(f, Value::Undefined));
        let mut quiet = false;
        let mut out = v.clone();
        let mut width = 1;
        while width < n {
            let mut lo = 0;
            while lo < n {
                let mid = (lo + width).min(n);
                let hi = (lo + 2 * width).min(n);
                let (mut i, mut j, mut k) = (lo, mid, lo);
                while i < mid && j < hi {
                    if Self::order(a, cmp.as_ref(), &mut quiet, v[i], v[j])? == Ordering::Greater {
                        out[k] = v[j];
                        j += 1;
                    } else {
                        out[k] = v[i];
                        i += 1;
                    }
                    k += 1;
                }
                out[k..k + mid - i].copy_from_slice(&v[i..mid]);
                k += mid - i;
                out[k..k + hi - j].copy_from_slice(&v[j..hi]);
                lo = hi;
            }
            core::mem::swap(&mut v, &mut out);
            width *= 2;
        }
        Ok(v)
    }

    fn comparator<'scope>(a: &mut Scope<'scope, '_>, f: Value<'scope>) -> Result<(), Error> {
        if f != Value::Undefined && !a.is_callable(f) {
            return Err(a.throw(ErrorKind::Type, "The comparison function must be either a function or undefined"));
        }
        Ok(())
    }

    fn sort<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let f = args.at(0);
        Self::comparator(a, f)?;
        let (o, len) = Self::target(a, this)?;
        let mut items = Vec::new();
        let mut k = 0.0;
        while k < len {
            if a.has_index(o, k)? {
                items.push(a.get_index(o.into(), k)?);
            }
            k += 1.0;
        }
        let plain = if f == Value::Undefined { Self::sort_plain(a, &items) } else { None };
        let sorted = match plain {
            Some(s) => s,
            None => Self::merge_sort(a, f, items)?,
        };
        #[expect(clippy::cast_precision_loss, reason = "element count is far below 2^53")]
        let count = sorted.len() as f64;
        for (j, v) in (0..).zip(sorted) {
            a.set_index(o.into(), f64::from(j), v)?;
        }
        let mut j = count;
        while j < len {
            a.delete_index(o, j)?;
            j += 1.0;
        }
        Ok(o.into())
    }

    fn to_sorted<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let f = args.at(0);
        Self::comparator(a, f)?;
        let (o, len) = Self::target(a, this)?;
        let arr = Self::create(a, len, None)?;
        let mut items = Vec::new();
        let mut k = 0.0;
        while k < len {
            items.push(a.get_index(o.into(), k)?);
            k += 1.0;
        }
        let plain = if f == Value::Undefined { Self::sort_plain(a, &items) } else { None };
        let sorted = match plain {
            Some(s) => s,
            None => Self::merge_sort(a, f, items)?,
        };
        for (j, v) in (0..).zip(sorted) {
            a.create_index(arr, f64::from(j), v)?;
        }
        Ok(arr.into())
    }

    fn to_reversed<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, len) = Self::target(a, this)?;
        let arr = Self::create(a, len, None)?;
        let mut k = 0.0;
        while k < len {
            let v = a.get_index(o.into(), len - k - 1.0)?;
            a.create_index(arr, k, v)?;
            k += 1.0;
        }
        Ok(arr.into())
    }

    fn to_spliced<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, len) = Self::target(a, this)?;
        let start = Self::relative(a, args.at(0), len, 0.0)?;
        let items = args.get(2..).unwrap_or(&[]);
        #[expect(clippy::cast_precision_loss, reason = "argument count is far below 2^53")]
        let insert = items.len() as f64;
        let skip = match args.len() {
            0 => 0.0,
            1 => len - start,
            _ => a.to_integer_or_infinity(args[1])?.clamp(0.0, len - start),
        };
        let new_len = len + insert - skip;
        if new_len > MAX_LENGTH {
            return Err(a.throw(ErrorKind::Type, "toSpliced result exceeds 2^53 - 1 length"));
        }
        let arr = Self::create(a, new_len, None)?;
        let mut i = 0.0;
        while i < start {
            let v = a.get_index(o.into(), i)?;
            a.create_index(arr, i, v)?;
            i += 1.0;
        }
        for &v in items {
            a.create_index(arr, i, v)?;
            i += 1.0;
        }
        let mut r = start + skip;
        while i < new_len {
            let v = a.get_index(o.into(), r)?;
            a.create_index(arr, i, v)?;
            i += 1.0;
            r += 1.0;
        }
        Ok(arr.into())
    }

    fn with<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, len) = Self::target(a, this)?;
        let rel = a.to_integer_or_infinity(args.at(0))?;
        let at = if rel >= 0.0 { rel } else { len + rel };
        if at >= len || at < 0.0 {
            return Err(a.throw(ErrorKind::Range, "Invalid index"));
        }
        let arr = Self::create(a, len, None)?;
        let mut k = 0.0;
        while k < len {
            let v = if k == at { args.at(1) } else { a.get_index(o.into(), k)? };
            a.create_index(arr, k, v)?;
            k += 1.0;
        }
        Ok(arr.into())
    }
}

impl Installer<'_, '_> {
    pub(crate) fn array(&mut self) {
        let proto = self.context.array_proto;
        proto.borrow_mut(self.mc).kind = ObjectKind::Array;
        let ctor = self.constructor("Array", Arrays::array, proto, 1);
        self.method(ctor, "isArray", Arrays::is_array, 1);
        self.method(ctor, "of", Arrays::of, 0);
        self.method(ctor, "from", Arrays::from, 1);
        let species = self.context.named_native(self.mc, Arrays::species, "get [Symbol.species]", 0);
        ctor.borrow_mut(self.mc).define_symbol(self.context.symbols.species, Property { slot: Slot::Accessor { get: Some(species), set: None }, enumerable: false, writable: false, configurable: true });
        let methods: [(&str, j8framework::NativeFn, u32); 34] = [
            ("at", Arrays::at, 1),
            ("concat", Arrays::concat, 1),
            ("copyWithin", Arrays::copy_within, 2),
            ("every", Arrays::every, 1),
            ("fill", Arrays::fill, 1),
            ("filter", Arrays::filter, 1),
            ("find", Arrays::find, 1),
            ("findIndex", Arrays::find_index, 1),
            ("findLast", Arrays::find_last, 1),
            ("findLastIndex", Arrays::find_last_index, 1),
            ("flat", Arrays::flat, 0),
            ("flatMap", Arrays::flat_map, 1),
            ("forEach", Arrays::for_each, 1),
            ("includes", Arrays::includes, 1),
            ("indexOf", Arrays::index_of, 1),
            ("join", Arrays::join, 1),
            ("lastIndexOf", Arrays::last_index_of, 1),
            ("map", Arrays::map, 1),
            ("pop", Arrays::pop, 0),
            ("push", Arrays::push, 1),
            ("reduce", Arrays::reduce, 1),
            ("reduceRight", Arrays::reduce_right, 1),
            ("reverse", Arrays::reverse, 0),
            ("shift", Arrays::shift, 0),
            ("slice", Arrays::slice, 2),
            ("some", Arrays::some, 1),
            ("sort", Arrays::sort, 1),
            ("splice", Arrays::splice, 2),
            ("toLocaleString", Arrays::to_locale_string, 0),
            ("toReversed", Arrays::to_reversed, 0),
            ("toSorted", Arrays::to_sorted, 1),
            ("toSpliced", Arrays::to_spliced, 2),
            ("toString", Arrays::to_string, 0),
            ("unshift", Arrays::unshift, 1),
        ];
        for (name, f, n) in methods {
            self.method(proto, name, f, n);
        }
        self.direct_push(Arrays::push, j8framework::FastCall::A1(Arrays::push1), j8framework::FastOut::Number);
        self.direct(Arrays::push, j8framework::FastCall::A2(Arrays::push2), j8framework::FastOut::Number);
        self.direct(Arrays::pop, j8framework::FastCall::A0(Arrays::pop0), j8framework::FastOut::Value);
        self.method(proto, "with", Arrays::with, 2);
        let unscopables = ObjectData::default().alloc(self.mc);
        for name in ["at", "copyWithin", "entries", "fill", "find", "findIndex", "findLast", "findLastIndex", "flat", "flatMap", "includes", "keys", "toReversed", "toSorted", "toSpliced", "values"] {
            unscopables.borrow_mut(self.mc).define(name, Property::data(true.into()));
        }
        proto.borrow_mut(self.mc).define_symbol(self.context.symbols.unscopables, Property { slot: Slot::Data(unscopables.into()), enumerable: false, writable: false, configurable: true });
    }
}
