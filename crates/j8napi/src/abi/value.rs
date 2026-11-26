use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::c_char;

use j8framework::{ObjectKind, SymbolData, Value, ValueKind};
use num_bigint::{BigInt, BigUint, Sign};

use crate::convert::Ffi;
use crate::{Env, Handle, Status, ValueType, napi};

const I64_LIMIT: f64 = 9_223_372_036_854_775_808.0;

impl Env<'_> {
    fn write_units<T: Copy + Default>(units: &[T], buf: *mut T, size: usize, result: *mut usize) -> Result<(), Status> {
        if buf.is_null() {
            return Ffi::put(result, units.len());
        }
        let n = units.len().min(size.saturating_sub(1));
        if size > 0 {
            let out = unsafe { core::slice::from_raw_parts_mut(buf, size) };
            out[..n].copy_from_slice(&units[..n]);
            out[n] = T::default();
        }
        Ffi::put_opt(result, n);
        Ok(())
    }

    fn bigint<'scope>(&self, h: Handle) -> Result<&'scope BigInt, Status> {
        match self.value(h)?.kind() {
            ValueKind::BigInt(b) => Ok(&j8framework::ScopeLocal::get(b).0),
            _ => Err(Status::BigintExpected),
        }
    }

    fn type_of(v: Value<'_>) -> ValueType {
        match v.kind() {
            ValueKind::Undefined | ValueKind::Uninit => ValueType::Undefined,
            ValueKind::Null => ValueType::Null,
            ValueKind::Bool(_) => ValueType::Boolean,
            ValueKind::Number(_) => ValueType::Number,
            ValueKind::String(_) => ValueType::String,
            ValueKind::Symbol(_) => ValueType::Symbol,
            ValueKind::BigInt(_) => ValueType::Bigint,
            ValueKind::Object(o) if o.borrow().kind.is_callable() => ValueType::Function,
            ValueKind::Object(o) if Self::holds::<crate::abi::object::External>(o) => ValueType::External,
            ValueKind::Object(_) => ValueType::Object,
        }
    }

    fn strict_equals<'scope>(x: Value<'scope>, y: Value<'scope>) -> bool {
        match (x.as_number(), y.as_number()) {
            (Some(a), Some(b)) => a == b,
            _ => x.same_value(y),
        }
    }

    fn kind_is(&self, h: Handle, test: impl FnOnce(&ObjectKind<'_>) -> bool) -> Result<bool, Status> {
        Ok(self.value(h)?.as_object().is_some_and(|o| test(&o.borrow().kind)))
    }
}

napi! {
    fn napi_get_undefined(env, result: *mut Handle) {
        env.out(result, Value::Undefined)
    }

    fn napi_get_null(env, result: *mut Handle) {
        env.out(result, Value::Null)
    }

    fn napi_get_global(env, result: *mut Handle) {
        let a = env.scope()?;
        env.out(result, a.context.global.into())
    }

    fn napi_get_boolean(env, value: bool, result: *mut Handle) {
        env.out(result, value.into())
    }

    fn napi_get_value_bool(env, value: Handle, result: *mut bool) {
        match env.value(value)?.kind() {
            ValueKind::Bool(b) => Ffi::put(result, b),
            _ => Err(Status::BooleanExpected),
        }
    }

    fn napi_create_double(env, value: f64, result: *mut Handle) {
        env.out(result, value.into())
    }

    fn napi_create_int32(env, value: i32, result: *mut Handle) {
        env.out(result, f64::from(value).into())
    }

    fn napi_create_uint32(env, value: u32, result: *mut Handle) {
        env.out(result, f64::from(value).into())
    }

    fn napi_create_int64(env, value: i64, result: *mut Handle) {
        #[expect(clippy::cast_precision_loss, reason = "JS numbers are f64; past 2^53 precision is lost, as in Node")]
        let x = value as f64;
        env.out(result, x.into())
    }

    fn napi_get_value_double(env, value: Handle, result: *mut f64) {
        Ffi::put(result, env.number(value)?)
    }

    fn napi_get_value_int32(env, value: Handle, result: *mut i32) {
        let a = env.scope()?;
        let x = env.number(value)?;
        Ffi::put(result, a.to_int32(x.into()).map_err(|_| Status::NumberExpected)?)
    }

    fn napi_get_value_uint32(env, value: Handle, result: *mut u32) {
        let a = env.scope()?;
        let x = env.number(value)?;
        Ffi::put(result, a.to_uint32(x.into()).map_err(|_| Status::NumberExpected)?)
    }

    fn napi_get_value_int64(env, value: Handle, result: *mut i64) {
        let x = env.number(value)?;
        #[expect(clippy::cast_possible_truncation, reason = "range checked")]
        let n = if !x.is_finite() { 0 } else if (-I64_LIMIT..I64_LIMIT).contains(&x) { x as i64 } else { i64::MIN };
        Ffi::put(result, n)
    }

    fn napi_create_bigint_int64(env, value: i64, result: *mut Handle) {
        let a = env.scope()?;
        env.out(result, a.bigint(BigInt::from(value)))
    }

    fn napi_create_bigint_uint64(env, value: u64, result: *mut Handle) {
        let a = env.scope()?;
        env.out(result, a.bigint(BigInt::from(value)))
    }

    fn napi_create_bigint_words(env, sign_bit: i32, word_count: usize, words: *const u64, result: *mut Handle) {
        env.preamble()?;
        let a = env.scope()?;
        if i32::try_from(word_count).is_err() {
            return Err(Status::InvalidArg);
        }
        let bytes: Vec<u8> = Ffi::slice(words, word_count)?.iter().flat_map(|w| w.to_le_bytes()).collect();
        let sign = if sign_bit == 0 { Sign::Plus } else { Sign::Minus };
        env.out(result, a.bigint(BigInt::from_biguint(sign, BigUint::from_bytes_le(&bytes))))
    }

    fn napi_get_value_bigint_int64(env, value: Handle, result: *mut i64, lossless: *mut bool) {
        let b = env.bigint(value)?;
        let wrapped = j8framework::Scope::wrap_bigint(b, 64, true);
        let n = i64::try_from(&wrapped).unwrap_or_default();
        Ffi::put(lossless, wrapped == *b)?;
        Ffi::put(result, n)
    }

    fn napi_get_value_bigint_uint64(env, value: Handle, result: *mut u64, lossless: *mut bool) {
        let b = env.bigint(value)?;
        let wrapped = j8framework::Scope::wrap_bigint(b, 64, false);
        let n = u64::try_from(&wrapped).unwrap_or_default();
        Ffi::put(lossless, wrapped == *b)?;
        Ffi::put(result, n)
    }

    fn napi_get_value_bigint_words(env, value: Handle, sign_bit: *mut i32, word_count: *mut usize, words: *mut u64) {
        let b = env.bigint(value)?;
        let (sign, digits) = b.to_u64_digits();
        if word_count.is_null() {
            return Err(Status::InvalidArg);
        }
        if words.is_null() {
            return Ffi::put(word_count, digits.len());
        }
        let cap = unsafe { word_count.read() };
        let n = digits.len().min(cap);
        unsafe { core::ptr::copy_nonoverlapping(digits.as_ptr(), words, n) };
        Ffi::put_opt(sign_bit, i32::from(sign == Sign::Minus));
        Ffi::put(word_count, n)
    }

    fn napi_create_string_utf8(env, str: *const c_char, length: usize, result: *mut Handle) {
        let a = env.scope()?;
        let s = Ffi::utf8(str, length)?;
        env.out(result, a.string(&s).into())
    }

    fn napi_create_string_latin1(env, str: *const c_char, length: usize, result: *mut Handle) {
        let a = env.scope()?;
        let s: alloc::string::String = Ffi::bytes(str, length)?.iter().map(|&b| char::from(b)).collect();
        env.out(result, a.string(&s).into())
    }

    fn napi_create_string_utf16(env, str: *const u16, length: usize, result: *mut Handle) {
        let a = env.scope()?;
        let units = if length == crate::AUTO_LENGTH {
            if str.is_null() {
                return Err(Status::InvalidArg);
            }
            let mut n = 0;
            while unsafe { str.add(n).read() } != 0 {
                n += 1;
            }
            Ffi::slice(str, n)?
        } else {
            Ffi::slice(str, length)?
        };
        env.out(result, a.string(&alloc::string::String::from_utf16_lossy(units)).into())
    }

    fn napi_get_value_string_utf8(env, value: Handle, buf: *mut c_char, bufsize: usize, result: *mut usize) {
        let s = env.string(value)?;
        let text: &str = &s;
        let mut n = text.len().min(bufsize.saturating_sub(1));
        while !text.is_char_boundary(n) {
            n -= 1;
        }
        let bytes = if buf.is_null() { text.as_bytes() } else { &text.as_bytes()[..n] };
        Env::write_units(bytes, buf.cast::<u8>(), bufsize, result)
    }

    fn napi_get_value_string_latin1(env, value: Handle, buf: *mut c_char, bufsize: usize, result: *mut usize) {
        let s = env.string(value)?;
        let bytes: Vec<u8> = s.chars().map(|c| c as u8).collect();
        Env::write_units(&bytes, buf.cast::<u8>(), bufsize, result)
    }

    fn napi_get_value_string_utf16(env, value: Handle, buf: *mut u16, bufsize: usize, result: *mut usize) {
        let s = env.string(value)?;
        let units: Vec<u16> = s.encode_utf16().collect();
        Env::write_units(&units, buf, bufsize, result)
    }

    fn napi_create_symbol(env, description: Handle, result: *mut Handle) {
        let a = env.scope()?;
        let description = if description.0.is_none() { None } else { Some(Box::<str>::from(&**env.string(description)?)) };
        env.out(result, Value::Symbol(a.mc.alloc(SymbolData { description, private: false })))
    }

    fn node_api_symbol_for(env, utf8description: *const c_char, length: usize, result: *mut Handle) {
        let a = env.scope()?;
        let key = Ffi::utf8(utf8description, length)?;
        let symbol = env.js(a.get_named(a.context.global.into(), "Symbol"))?;
        let for_ = env.js(a.get_named(symbol, "for"))?;
        let k = a.string(&key).into();
        let s = env.js(a.call(for_, symbol, &[k]))?;
        env.out(result, s)
    }

    fn napi_typeof(env, value: Handle, result: *mut ValueType) {
        Ffi::put(result, Env::type_of(env.value(value)?))
    }

    fn napi_strict_equals(env, lhs: Handle, rhs: Handle, result: *mut bool) {
        Ffi::put(result, Env::strict_equals(env.value(lhs)?, env.value(rhs)?))
    }

    fn napi_coerce_to_bool(env, value: Handle, result: *mut Handle) {
        let a = env.scope()?;
        env.out(result, a.to_boolean(env.value(value)?).into())
    }

    fn napi_coerce_to_number(env, value: Handle, result: *mut Handle) {
        env.preamble()?;
        let a = env.scope()?;
        let x = env.js(a.to_number(env.value(value)?))?;
        env.out(result, x.into())
    }

    fn napi_coerce_to_string(env, value: Handle, result: *mut Handle) {
        env.preamble()?;
        let a = env.scope()?;
        let s = env.js(a.to_string(env.value(value)?))?;
        env.out(result, s.into())
    }

    fn napi_coerce_to_object(env, value: Handle, result: *mut Handle) {
        env.preamble()?;
        let a = env.scope()?;
        let o = env.js(a.to_object(env.value(value)?))?;
        env.out(result, o.into())
    }

    fn napi_instanceof(env, object: Handle, constructor: Handle, result: *mut bool) {
        env.preamble()?;
        let a = env.scope()?;
        let c = env.callable(a, constructor)?;
        let b = env.js(a.instance_of(env.value(object)?, c))?;
        Ffi::put(result, b)
    }

    fn napi_is_array(env, value: Handle, result: *mut bool) {
        let a = env.scope()?;
        let b = env.js(a.is_array(env.value(value)?))?;
        Ffi::put(result, b)
    }

    fn napi_is_error(env, value: Handle, result: *mut bool) {
        Ffi::put(result, env.kind_is(value, |k| matches!(k, ObjectKind::Error(_)))?)
    }

    fn napi_is_arraybuffer(env, value: Handle, result: *mut bool) {
        Ffi::put(result, env.kind_is(value, |k| matches!(k, ObjectKind::ArrayBuffer(b) if !b.shared))?)
    }

    fn napi_is_typedarray(env, value: Handle, result: *mut bool) {
        Ffi::put(result, env.kind_is(value, |k| matches!(k, ObjectKind::TypedArray(..)))?)
    }

    fn napi_is_dataview(env, value: Handle, result: *mut bool) {
        Ffi::put(result, env.kind_is(value, |k| matches!(k, ObjectKind::DataView(_)))?)
    }

    fn napi_is_date(env, value: Handle, result: *mut bool) {
        Ffi::put(result, env.kind_is(value, |k| matches!(k, ObjectKind::Date(_)))?)
    }

    fn napi_is_promise(env, value: Handle, result: *mut bool) {
        Ffi::put(result, j8framework::Scope::is_promise(env.value(value)?))
    }
}
