use j8framework::ValueKind;
use alloc::string::String;
use alloc::vec::Vec;

use j8framework::{Scope, Error, ErrorKind, JsString, Units, Value};

use crate::{Args, Installer};

const MAX_STRING_LENGTH: f64 = 536_870_888.0;

pub(crate) struct Strings;

const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

impl Strings {
    fn this<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>) -> Result<JsString<'scope>, Error> {
        if this.is_nullish() {
            return Err(a.throw(ErrorKind::Type, "String.prototype method called on null or undefined"));
        }
        a.to_string(this)
    }

    fn units<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>) -> Result<Vec<u16>, Error> {
        Ok(Self::this(a, this)?.encode_utf16().collect())
    }

    fn cut<'scope>(a: &Scope<'scope, '_>, s: JsString<'scope>, r: core::ops::Range<usize>) -> JsString<'scope> {
        match s.units() {
            Units::Ascii(b) => {
                let end = r.end.min(b.len());
                a.piece(s, &s[r.start.min(end)..end])
            }
            Units::Wide(_) => a.slice_units(s, r),
        }
    }

    fn from_units<'scope>(a: &Scope<'scope, '_>, u: &[u16]) -> Value<'scope> {
        a.string_units(u).into()
    }

    fn count<'scope>(n: usize) -> Value<'scope> {
        Value::Number(f64::from(u32::try_from(n).unwrap_or(u32::MAX)))
    }

    fn clamp<'scope>(a: &mut Scope<'scope, '_>, v: Value<'scope>, len: usize, default: usize) -> Result<usize, Error> {
        if v == Value::Undefined {
            return Ok(default);
        }
        let x = libm::trunc(a.to_number(v)?);
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "clamped to [0, len]")]
        let r = if x.is_nan() || x <= 0.0 { 0 } else { (x.min(f64::from(u32::try_from(len).unwrap_or(u32::MAX)))) as usize };
        Ok(r)
    }

    fn relative<'scope>(a: &mut Scope<'scope, '_>, v: Value<'scope>, len: usize, default: usize) -> Result<usize, Error> {
        if v == Value::Undefined {
            return Ok(default);
        }
        let x = libm::trunc(a.to_number(v)?);
        let len_f = f64::from(u32::try_from(len).unwrap_or(u32::MAX));
        let r = if x < 0.0 { (len_f + x).max(0.0) } else { x.min(len_f) };
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "clamped to [0, len]")]
        let r = if r.is_nan() { 0 } else { r as usize };
        Ok(r)
    }

    fn case<'scope>(a: &Scope<'scope, '_>, s: JsString<'scope>, map: fn(&str) -> String) -> Value<'scope> {
        let Some(w) = s.wide().filter(|_| s.lone_surrogates()) else { return a.string(&map(&s)).into() };
        let mut out = Vec::with_capacity(w.len());
        let mut run = String::new();
        for c in char::decode_utf16(w.iter().copied()) {
            match c {
                Ok(c) => run.push(c),
                Err(e) => {
                    out.extend(map(&run).encode_utf16());
                    run.clear();
                    out.push(e.unpaired_surrogate());
                }
            }
        }
        out.extend(map(&run).encode_utf16());
        a.string_units(&out).into()
    }

    fn trim_units<'scope>(a: &Scope<'scope, '_>, s: JsString<'scope>, start: bool, end: bool) -> Value<'scope> {
        let w: Vec<u16> = s.encode_utf16().collect();
        let space = |u: &u16| char::from_u32(u32::from(*u)).is_some_and(Self::is_space);
        let lo = if start { w.iter().take_while(|u| space(u)).count() } else { 0 };
        let hi = if end { w.len() - w[lo..].iter().rev().take_while(|u| space(u)).count() } else { w.len() };
        a.slice_units(s, lo..hi).into()
    }

    pub(crate) fn is_space(c: char) -> bool {
        (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}'
    }

    pub(crate) fn find(hay: &[u16], needle: &[u16], from: usize) -> Option<usize> {
        (from..=hay.len().checked_sub(needle.len())?).find(|&i| hay[i..].starts_with(needle))
    }

    fn string<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let new_target = a.new_target;
        let s: Value<'scope> = match (args.first(), new_target.kind()) {
            (None, _) => a.string("").into(),
            (Some(&v), ValueKind::Undefined) if let ValueKind::Symbol(s) = v.kind() => a.symbol_descriptive(s).into(),
            (Some(&v), _) => a.to_string(v)?.into(),
        };
        a.wrap_primitive(new_target, s, a.context.string_proto)
    }

    fn to_string<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        match this.unwrapped().kind() {
            v @ ValueKind::String(_) => Ok(Value::from(v)),
            _ => Err(a.throw(ErrorKind::Type, "String.prototype.toString requires that 'this' be a String")),
        }
    }

    fn char_at<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let s = Self::this(a, this)?;
        let i = Self::clamp(a, args.at(0), s.utf16_len(), 0)?;
        Ok(s.code_unit(i).map_or_else(|| a.string("").into(), |u| a.unit_string(u).into()))
    }

    fn char_code_at<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let s = Self::this(a, this)?;
        let i = Self::clamp(a, args.at(0), s.utf16_len(), 0)?;
        Ok(s.code_unit(i).map_or(f64::NAN, f64::from).into())
    }

    fn index_of<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let s = Self::this(a, this)?;
        let n = a.to_string(args.at(0))?;
        if s.is_ascii() {
            let from = Self::clamp(a, args.at(1), s.len(), 0)?;
            return Ok(a.par().find_str(&s[from..], &n).map_or(Value::Number(-1.0), |i| Self::count(from + i)));
        }
        let u: Vec<u16> = s.encode_utf16().collect();
        let n: Vec<u16> = n.encode_utf16().collect();
        let from = Self::clamp(a, args.at(1), u.len(), 0)?;
        Ok(Self::find(&u, &n, from).map_or(Value::Number(-1.0), Self::count))
    }

    fn last_index_of<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let u = Self::units(a, this)?;
        let n: Vec<u16> = a.to_string(args.at(0))?.encode_utf16().collect();
        let last = u.len().checked_sub(n.len());
        Ok(last.and_then(|l| (0..=l).rev().find(|&i| u[i..].starts_with(&n))).map_or(Value::Number(-1.0), Self::count))
    }

    fn needle<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>], method: &str) -> Result<(JsString<'scope>, JsString<'scope>), Error> {
        let s = Self::this(a, this)?;
        if crate::regexp::Regexps::is_regexp(a, args.at(0))? {
            return Err(a.throw(ErrorKind::Type, &alloc::format!("First argument to String.prototype.{method} must not be a regular expression")));
        }
        let n = a.to_string(args.at(0))?;
        Ok((s, n))
    }

    fn includes<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (s, n) = Self::needle(a, this, args, "includes")?;
        let len = s.utf16_len();
        let from = Self::clamp(a, args.at(1), len, 0)?;
        if s.is_ascii() {
            return Ok(a.par().find_str(&s[from..], &n).is_some().into());
        }
        let u = s.units().to_vec();
        let n: Vec<u16> = n.encode_utf16().collect();
        Ok(Self::find(&u, &n, from).is_some().into())
    }

    fn starts_with<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (s, n) = Self::needle(a, this, args, "startsWith")?;
        let len = s.utf16_len();
        let from = Self::clamp(a, args.at(1), len, 0)?;
        if s.is_ascii() {
            return Ok(s[from..].starts_with(&**n).into());
        }
        let u = s.units();
        let n: Vec<u16> = n.encode_utf16().collect();
        Ok((from + n.len() <= len && n.iter().enumerate().all(|(i, &c)| u.get(from + i) == Some(c))).into())
    }

    fn ends_with<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (s, n) = Self::needle(a, this, args, "endsWith")?;
        let len = s.utf16_len();
        let end = Self::clamp(a, args.at(1), len, len)?;
        if s.is_ascii() {
            return Ok(s[..end].ends_with(&**n).into());
        }
        let u = s.units();
        let n: Vec<u16> = n.encode_utf16().collect();
        Ok((n.len() <= end && n.iter().enumerate().all(|(i, &c)| u.get(end - n.len() + i) == Some(c))).into())
    }

    fn slice<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let s = Self::this(a, this)?;
        let len = s.utf16_len();
        let start = Self::relative(a, args.at(0), len, 0)?;
        let end = Self::relative(a, args.at(1), len, len)?;
        Ok(Self::cut(a, s, start..end.max(start)).into())
    }

    fn substring<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let st = Self::this(a, this)?;
        let len = st.utf16_len();
        let s = Self::clamp(a, args.at(0), len, 0)?;
        let e = Self::clamp(a, args.at(1), len, len)?;
        Ok(Self::cut(a, st, s.min(e)..s.max(e)).into())
    }

    fn at<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let s = Self::this(a, this)?;
        let x = a.to_integer_or_infinity(args.at(0))?;
        let len = f64::from(u32::try_from(s.utf16_len()).unwrap_or(u32::MAX));
        let i = if x < 0.0 { len + x } else { x };
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "checked to be in [0, len)")]
        let v = if (0.0..len).contains(&i) { s.code_unit(i as usize).map_or(Value::Undefined, |u| a.unit_string(u).into()) } else { Value::Undefined };
        Ok(v)
    }

    locale_method!(to_locale_upper_case, crate::intl::Cases::to_upper, Self::to_upper_case);

    locale_method!(to_locale_lower_case, crate::intl::Cases::to_lower, Self::to_lower_case);

    fn to_upper_case<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let s = Self::this(a, this)?;
        Ok(Self::case(a, s, str::to_uppercase))
    }

    fn to_lower_case<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let s = Self::this(a, this)?;
        Ok(Self::case(a, s, str::to_lowercase))
    }

    fn trim<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let s = Self::this(a, this)?;
        if s.lone_surrogates() {
            return Ok(Self::trim_units(a, s, true, true));
        }
        Ok(a.piece(s, s.trim_matches(Self::is_space)).into())
    }

    fn trim_start<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let s = Self::this(a, this)?;
        if s.lone_surrogates() {
            return Ok(Self::trim_units(a, s, true, false));
        }
        Ok(a.piece(s, s.trim_start_matches(Self::is_space)).into())
    }

    fn trim_end<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let s = Self::this(a, this)?;
        if s.lone_surrogates() {
            return Ok(Self::trim_units(a, s, false, true));
        }
        Ok(a.piece(s, s.trim_end_matches(Self::is_space)).into())
    }

    pub(crate) fn split<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let s = Self::this(a, this)?;
        let limit = match args.at(1).kind() {
            ValueKind::Undefined => usize::MAX,
            v => a.to_uint32(Value::from(v))? as usize,
        };
        let items: Vec<Value<'scope>> = match args.at(0).kind() {
            ValueKind::Undefined if limit == 0 => Vec::new(),
            ValueKind::Undefined => alloc::vec![s.into()],
            sep => {
                let sep = a.to_string(Value::from(sep))?;
                if sep.is_empty() {
                    if s.is_ascii() {
                        s.as_bytes().iter().take(limit).map(|&c| a.string(core::str::from_utf8(&[c]).unwrap_or_default()).into()).collect()
                    } else {
                        s.encode_utf16().take(limit).map(|c| Self::from_units(a, &[c])).collect()
                    }
                } else {
                    s.split(&**sep).take(limit).map(|p| a.piece(s, p).into()).collect()
                }
            }
        };
        Ok(a.array(items).into())
    }

    fn repeat<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let s = Self::this(a, this)?;
        let n = a.to_number(args.at(0))?;
        if n < 0.0 || n.is_infinite() {
            return Err(a.throw(ErrorKind::Range, "Invalid count value"));
        }
        let n = if n.is_nan() { 0.0 } else { libm::trunc(n) };
        if s.is_empty() || n == 0.0 {
            return Ok(a.string("").into());
        }
        #[expect(clippy::cast_precision_loss, reason = "length comparison; precision is sufficient")]
        let total = s.encode_utf16().count() as f64 * n;
        if total > MAX_STRING_LENGTH {
            return Err(a.throw(ErrorKind::Range, "Invalid string length"));
        }
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "checked non-negative and the product within the string limit")]
        Ok(a.string(&s.repeat(n as usize)).into())
    }

    fn pad<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>], start: bool) -> Result<Value<'scope>, Error> {
        let u = Self::units(a, this)?;
        let target = libm::trunc(a.to_number(args.at(0))?);
        let fill: Vec<u16> = match args.at(1).kind() {
            ValueKind::Undefined => alloc::vec![u16::from(b' ')],
            v => a.to_string(Value::from(v))?.encode_utf16().collect(),
        };
        #[expect(clippy::cast_precision_loss, reason = "length comparison; precision is sufficient")]
        let len = u.len() as f64;
        if target.is_nan() || target <= len || fill.is_empty() {
            return Ok(Self::from_units(a, &u));
        }
        if target > MAX_STRING_LENGTH {
            return Err(a.throw(ErrorKind::Range, "Invalid string length"));
        }
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "checked to be in (len, string limit]")]
        let target = target as usize;
        let pad: Vec<u16> = fill.iter().copied().cycle().take(target - u.len()).collect();
        let out = if start { [pad, u].concat() } else { [u, pad].concat() };
        Ok(Self::from_units(a, &out))
    }

    fn pad_start<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::pad(a, this, args, true)
    }

    fn pad_end<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::pad(a, this, args, false)
    }

    fn concat<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let mut out = String::from(&**Self::this(a, this)?);
        for &v in args {
            out.push_str(&a.to_string(v)?);
        }
        Ok(a.string(&out).into())
    }

    fn number<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let new_target = a.new_target;
        let n = match args.first() {
            None => 0.0,
            Some(&v) => match a.to_numeric(v)? {
                j8framework::Numeric::Number(x) => x,
                j8framework::Numeric::BigInt(b) => num_traits::ToPrimitive::to_f64(&b.0).unwrap_or(f64::NAN),
            },
        };
        a.wrap_primitive(new_target, n.into(), a.context.number_proto)
    }

    fn is_integer<'scope>(_: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Ok(matches!(args.at(0).kind(), ValueKind::Number(x) if x.is_finite() && libm::trunc(x) == x).into())
    }

    fn is_safe_integer<'scope>(_: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Ok(matches!(args.at(0).kind(), ValueKind::Number(x) if x.is_finite() && libm::trunc(x) == x && libm::fabs(x) <= MAX_SAFE_INTEGER).into())
    }

    fn number_is_nan<'scope>(_: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Ok(matches!(args.at(0).kind(), ValueKind::Number(x) if x.is_nan()).into())
    }

    fn number_to_string<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        const MAX_FRACTION_DIGITS: usize = 52;
        const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
        let Some(x) = this.unwrapped().as_number() else {
            return Err(a.throw(ErrorKind::Type, "Number.prototype.toString requires that 'this' be a Number"));
        };
        let radix = match args.at(0).kind() {
            ValueKind::Undefined => 10,
            r => a.to_int32(Value::from(r))?,
        };
        if !(2..=36).contains(&radix) {
            return Err(a.throw(ErrorKind::Range, "toString() radix must be between 2 and 36"));
        }
        if radix == 10 || !x.is_finite() {
            return Ok(a.to_string(Value::Number(x))?.into());
        }
        let r = f64::from(radix);
        let (neg, x) = (x < 0.0, libm::fabs(x));
        let (mut int, mut frac) = (libm::trunc(x), x - libm::trunc(x));
        let mut digits = Vec::new();
        loop {
            let d = libm::fmod(int, r);
            #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "remainder is in [0, radix)")]
            digits.push(DIGITS[d as usize]);
            int = libm::trunc(int / r);
            if int == 0.0 {
                break;
            }
        }
        digits.reverse();
        if frac > 0.0 {
            digits.push(b'.');
            for _ in 0..MAX_FRACTION_DIGITS {
                frac *= r;
                let d = libm::trunc(frac);
                #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "in [0, radix)")]
                digits.push(DIGITS[d as usize]);
                frac -= d;
                if frac == 0.0 {
                    break;
                }
            }
        }
        let s = String::from_utf8(digits).unwrap_or_default();
        Ok(a.string(&if neg { alloc::format!("-{s}") } else { s }).into())
    }

    fn number_value_of<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        match this.unwrapped().kind() {
            v @ ValueKind::Number(_) => Ok(Value::from(v)),
            _ => Err(a.throw(ErrorKind::Type, "Number.prototype.valueOf requires that 'this' be a Number")),
        }
    }
}

impl Installer<'_, '_> {
    pub(crate) fn string(&mut self) {
        let proto = self.context.string_proto;
        proto.borrow_mut(self.mc).kind = j8framework::ObjectKind::Primitive(self.text(""));
        self.constructor("String", Strings::string, proto, 1);
        let methods: [(&str, j8framework::NativeFn, u32); 25] = [
            ("toString", Strings::to_string, 0),
            ("valueOf", Strings::to_string, 0),
            ("charAt", Strings::char_at, 1),
            ("charCodeAt", Strings::char_code_at, 1),
            ("indexOf", Strings::index_of, 1),
            ("lastIndexOf", Strings::last_index_of, 1),
            ("includes", Strings::includes, 1),
            ("startsWith", Strings::starts_with, 1),
            ("endsWith", Strings::ends_with, 1),
            ("slice", Strings::slice, 2),
            ("substring", Strings::substring, 2),
            ("at", Strings::at, 1),
            ("toUpperCase", Strings::to_upper_case, 0),
            ("toLowerCase", Strings::to_lower_case, 0),
            ("trim", Strings::trim, 0),
            ("trimStart", Strings::trim_start, 0),
            ("trimEnd", Strings::trim_end, 0),
            ("split", Strings::split, 2),
            ("repeat", Strings::repeat, 1),
            ("padStart", Strings::pad_start, 1),
            ("padEnd", Strings::pad_end, 1),
            ("concat", Strings::concat, 1),
            ("toLocaleUpperCase", Strings::to_locale_upper_case, 0),
            ("toLocaleLowerCase", Strings::to_locale_lower_case, 0),
            ("trimLeft", Strings::trim_start, 0),
        ];
        for (name, f, n) in methods {
            self.method(proto, name, f, n);
        }
    }

    pub(crate) fn number(&mut self) {
        let proto = self.context.number_proto;
        proto.borrow_mut(self.mc).kind = j8framework::ObjectKind::Primitive(0.0.into());
        let ctor = self.constructor("Number", Strings::number, proto, 1);
        self.method(ctor, "isInteger", Strings::is_integer, 1);
        self.method(ctor, "isSafeInteger", Strings::is_safe_integer, 1);
        self.method(ctor, "isNaN", Strings::number_is_nan, 1);
        self.set(ctor, "MAX_SAFE_INTEGER", MAX_SAFE_INTEGER.into());
        self.set(ctor, "MIN_SAFE_INTEGER", (-MAX_SAFE_INTEGER).into());
        self.set(ctor, "EPSILON", f64::EPSILON.into());
        self.set(ctor, "MAX_VALUE", f64::MAX.into());
        self.set(ctor, "MIN_VALUE", f64::from_bits(1).into());
        self.set(ctor, "POSITIVE_INFINITY", f64::INFINITY.into());
        self.set(ctor, "NEGATIVE_INFINITY", f64::NEG_INFINITY.into());
        self.set(ctor, "NaN", f64::NAN.into());
        self.method(proto, "toString", Strings::number_to_string, 1);
        self.method(proto, "valueOf", Strings::number_value_of, 0);
    }
}
