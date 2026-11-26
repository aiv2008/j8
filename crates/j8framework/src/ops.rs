use crate::ValueKind;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::{ScopeLocal};

use num_traits::Zero;

use crate::{Scope, Descriptor, Error, Key, Numeric, IterKind, IteratorState, JsObject, JsString, JsSymbol, ObjectData, ObjectKind, Property, PropertyKey, Slot, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    Error,
    Type,
    Reference,
    Range,
    Syntax,
    Eval,
    Uri,
    Aggregate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Hint {
    Default,
    Number,
    String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Exp,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    UShr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CompareOp {
    Eq,
    Ne,
    StrictEq,
    StrictNe,
    Lt,
    Gt,
    Le,
    Ge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UnaryOp {
    Neg,
    Plus,
    Not,
    BitNot,
    Typeof,
    Void,
}

pub trait Numbers {
    type Output;
    fn numbers(self, a: f64, b: f64) -> Self::Output;
}

impl Numbers for BinaryOp {
    type Output = f64;

    #[expect(clippy::inline_always, reason = "measured: left to LLVM it stays a call with a jump table in the interpreter loop")]
    #[inline(always)]
    fn numbers(self, a: f64, b: f64) -> f64 {
        match self {
            BinaryOp::Add => a + b,
            BinaryOp::Sub => a - b,
            BinaryOp::Mul => a * b,
            BinaryOp::Div => a / b,
            BinaryOp::Rem => remainder(a, b),
            BinaryOp::Exp => pow(a, b),
            BinaryOp::BitAnd => f64::from(to_int32(a) & to_int32(b)),
            BinaryOp::BitOr => f64::from(to_int32(a) | to_int32(b)),
            BinaryOp::BitXor => f64::from(to_int32(a) ^ to_int32(b)),
            BinaryOp::Shl => f64::from(to_int32(a).wrapping_shl(modulo_u32(b) & 31)),
            BinaryOp::Shr => f64::from(to_int32(a).wrapping_shr(modulo_u32(b) & 31)),
            BinaryOp::UShr => f64::from(modulo_u32(a) >> (modulo_u32(b) & 31)),
        }
    }
}

impl Numbers for CompareOp {
    type Output = bool;

    #[inline]
    fn numbers(self, a: f64, b: f64) -> bool {
        match self {
            CompareOp::Eq | CompareOp::StrictEq => a == b,
            CompareOp::Ne | CompareOp::StrictNe => a != b,
            CompareOp::Lt => a < b,
            CompareOp::Gt => a > b,
            CompareOp::Le => a <= b,
            CompareOp::Ge => a >= b,
        }
    }
}

impl ErrorKind {
    pub const ALL: [ErrorKind; 8] = [ErrorKind::Error, ErrorKind::Type, ErrorKind::Reference, ErrorKind::Range, ErrorKind::Syntax, ErrorKind::Eval, ErrorKind::Uri, ErrorKind::Aggregate];

    pub fn name(self) -> &'static str {
        match self {
            ErrorKind::Error => "Error",
            ErrorKind::Type => "TypeError",
            ErrorKind::Reference => "ReferenceError",
            ErrorKind::Range => "RangeError",
            ErrorKind::Syntax => "SyntaxError",
            ErrorKind::Eval => "EvalError",
            ErrorKind::Uri => "URIError",
            ErrorKind::Aggregate => "AggregateError",
        }
    }
}

#[derive(Clone, Copy, Debug, crate::Collect)]
#[collect(no_drop)]
pub struct IterRecord<'scope> {
    pub iterator: Value<'scope>,
    pub next: Value<'scope>,
}

const TWO_POW_32: f64 = 4_294_967_296.0;

const MAX_INDEX: f64 = 4_294_967_294.0;

pub(crate) fn parse_number(s: &str) -> f64 {
    let t = s.trim_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}');
    match t {
        "" => 0.0,
        "Infinity" | "+Infinity" => f64::INFINITY,
        "-Infinity" => f64::NEG_INFINITY,
        _ => {
            let radix = match t.get(..2) {
                Some("0x" | "0X") => 16,
                Some("0o" | "0O") => 8,
                Some("0b" | "0B") => 2,
                _ => 10,
            };
            if radix == 10 {
                let ok = t.bytes().all(|c| c.is_ascii_digit() || matches!(c, b'+' | b'-' | b'.' | b'e' | b'E'));
                return if ok { t.parse().unwrap_or(f64::NAN) } else { f64::NAN };
            }
            let digits = &t[2..];
            if digits.is_empty() {
                return f64::NAN;
            }
            digits.chars().try_fold(0.0, |acc, c| c.to_digit(radix).map(|d| acc * f64::from(radix) + f64::from(d))).unwrap_or(f64::NAN)
        }
    }
}

pub const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

#[inline]
fn modulo_u32(x: f64) -> u32 {
    #[expect(clippy::cast_possible_truncation, reason = "the comparison below confirms nothing was truncated")]
    let i = x as i32;
    if f64::from(i) == x {
        return i.cast_unsigned();
    }
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "the comparison below confirms nothing was truncated")]
    let u = x as u32;
    if f64::from(u) == x {
        return u;
    }
    modulo_u32_slow(x)
}

#[cold]
#[inline(never)]
fn modulo_u32_slow(x: f64) -> u32 {
    if !x.is_finite() {
        return 0;
    }
    let m = libm::trunc(x) % TWO_POW_32;
    let m = if m < 0.0 { m + TWO_POW_32 } else { m };
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "m is already reduced to [0, 2^32)")]
    let u = m as u32;
    u
}

pub fn number_text(x: f64) -> crate::SmallStr {
    let mut buf = [0u8; 20];
    if let Some(digits) = integer_digits(x, &mut buf) {
        return crate::SmallStr::from(core::str::from_utf8(digits).unwrap_or_default());
    }
    crate::SmallStr::from(ryu_js::Buffer::new().format(x))
}

fn integer_digits(x: f64, buf: &mut [u8; 20]) -> Option<&[u8]> {
    #[expect(clippy::cast_possible_truncation, reason = "the comparison below confirms it is a safe integer")]
    let i = x as i64;
    #[expect(clippy::cast_precision_loss, reason = "exact within the safe integer range")]
    if x.abs() <= MAX_SAFE_INTEGER && i as f64 == x {
        let mut at = buf.len();
        let mut n = i.unsigned_abs();
        loop {
            at -= 1;
            let digit = (n % 10) as u8;
            buf[at] = b'0' + digit;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        if i < 0 {
            at -= 1;
            buf[at] = b'-';
        }
        return Some(&buf[at..]);
    }
    None
}

#[inline]
fn to_int32(x: f64) -> i32 {
    modulo_u32(x).cast_signed()
}

const MOD_INT_LIMIT: f64 = 4_611_686_018_427_387_904.0;

#[inline]
fn remainder(a: f64, b: f64) -> f64 {
    if a.abs() <= MOD_INT_LIMIT && b.abs() <= MOD_INT_LIMIT && b != 0.0 {
        #[expect(clippy::cast_possible_truncation, reason = "magnitude at most 2^62, so the value fits in i64; the comparison below confirms it is an integer")]
        let (x, y) = (a as i64, b as i64);
        #[expect(clippy::cast_precision_loss, reason = "an integer whose magnitude is at most 2^62 is an exact f64")]
        if x as f64 == a && y as f64 == b {
            #[expect(clippy::cast_precision_loss, reason = "the remainder's magnitude is below the divisor's; exact")]
            let r = (x % y) as f64;
            return if r == 0.0 { 0.0f64.copysign(a) } else { r };
        }
    }
    fmod(a, b)
}

#[cold]
#[inline(never)]
fn fmod(a: f64, b: f64) -> f64 {
    a % b
}

#[inline(never)]
fn pow(a: f64, b: f64) -> f64 {
    if b.is_nan() || ((a == 1.0 || a == -1.0) && b.is_infinite()) { f64::NAN } else { libm::pow(a, b) }
}

fn index_of_number(x: f64) -> Option<u32> {
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "converted back and checked for equality")]
    let n = x as u32;
    (f64::from(n) == x && x <= MAX_INDEX).then_some(n)
}

pub(crate) fn index_of_str(s: &str) -> Option<u32> {
    let canonical = s == "0" || (!s.starts_with('0') && !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()));
    canonical.then(|| s.parse().ok()).flatten().filter(|&i| f64::from(i) <= MAX_INDEX)
}

impl<'scope> Scope<'scope, '_> {
    pub fn par(&self) -> crate::Par<'_> {
        crate::Par(self.platform)
    }

    pub fn unit_string(&self, u: u16) -> JsString<'scope> {
        match self.context.chars.get(usize::from(u)) {
            Some(&s) => s,
            None => self.string_units(&[u]),
        }
    }

    pub fn string(&self, s: &str) -> JsString<'scope> {
        crate::JsStr::alloc(self.mc, s)
    }

    pub fn slice_units(&self, of: JsString<'scope>, r: core::ops::Range<usize>) -> JsString<'scope> {
        crate::JsStr::slice_units(self.mc, of, r)
    }

    pub fn string_units(&self, units: &[u16]) -> JsString<'scope> {
        crate::JsStr::alloc_units(self.mc, units)
    }

    pub fn string_from(&self, text: &str, ascii: bool) -> JsString<'scope> {
        crate::JsStr::alloc_known(self.mc, text, ascii)
    }

    pub fn piece(&self, of: JsString<'scope>, piece: &str) -> JsString<'scope> {
        crate::JsStr::piece(self.mc, of, piece)
    }

    pub fn object(&self) -> JsObject<'scope> {
        ObjectData { proto: Some(self.context.object_proto), ..ObjectData::default() }.alloc(self.mc)
    }

    pub fn object_with(&self, capacity: usize) -> JsObject<'scope> {
        ObjectData::ordinary(self.mc, Some(self.context.object_proto), capacity)
    }

    pub fn array(&self, items: Vec<Value<'scope>>) -> JsObject<'scope> {
        ObjectData { kind: ObjectKind::Array, elements: items, proto: Some(self.context.array_proto), ..ObjectData::default() }.alloc(self.mc)
    }

    pub fn array_from_slice(&self, items: &[Value<'scope>]) -> JsObject<'scope> {
        let o = ObjectData { kind: ObjectKind::Array, proto: Some(self.context.array_proto), ..ObjectData::default() }.alloc(self.mc);
        if !items.is_empty() {
            o.elements().replace(self.mc, items);
        }
        o
    }

    pub fn error(&self, kind: ErrorKind, message: &str) -> JsObject<'scope> {
        let e = ObjectData { kind: ObjectKind::Error(self.capture()), proto: Some(self.context.error_proto(kind)), ..ObjectData::default() }.alloc(self.mc);
        let m = self.string(message);
        e.borrow_mut(self.mc).define("message", Property::hidden(m.into()));
        e
    }

    pub fn throw(&self, kind: ErrorKind, message: &str) -> Error {
        self.throw_value(self.error(kind, message).into())
    }

    pub fn throw_value(&self, v: Value<'scope>) -> Error {
        self.mc.raise(v)
    }

    pub fn rethrow(&self, v: Value<'scope>) -> Error {
        self.mc.rethrow(v)
    }

    pub fn take_exception(&self) -> Value<'scope> {
        self.mc.take_exception().unwrap_or_default()
    }

    pub fn is_callable(&self, v: Value<'scope>) -> bool {
        matches!(v.kind(), ValueKind::Object(o) if o.borrow().kind.is_callable())
    }

    #[inline]
    pub fn to_boolean(&self, v: Value<'scope>) -> bool {
        match v.kind() {
            ValueKind::Undefined | ValueKind::Null | ValueKind::Uninit => false,
            ValueKind::Bool(b) => b,
            ValueKind::Number(x) => !(x == 0.0 || x.is_nan()),
            ValueKind::String(s) => s.byte_len() != 0,
            ValueKind::BigInt(b) => !b.0.is_zero(),
            ValueKind::Symbol(_) | ValueKind::Object(_) => true,
        }
    }

    pub fn to_primitive(&mut self, v: Value<'scope>, hint: Hint) -> Result<Value<'scope>, Error> {
        if v.as_object().is_none() {
            return Ok(v);
        }
        let exotic = self.get_symbol(v, self.context.symbols.to_primitive, v)?;
        if !exotic.is_nullish() {
            let h = self.string(match hint {
                Hint::Default => "default",
                Hint::Number => "number",
                Hint::String => "string",
            });
            let r = self.call(exotic, v, &[h.into()])?;
            if r.as_object().is_some() {
                return Err(self.throw(ErrorKind::Type, "Cannot convert object to primitive value"));
            }
            return Ok(r);
        }
        let order = if hint == Hint::String { ["toString", "valueOf"] } else { ["valueOf", "toString"] };
        for name in order {
            let f = self.get_named(v, name)?;
            if self.is_callable(f) {
                let r = self.call(f, v, &[])?;
                if r.as_object().is_none() {
                    return Ok(r);
                }
            }
        }
        Err(self.throw(ErrorKind::Type, "Cannot convert object to primitive value"))
    }

    pub fn to_number(&mut self, v: Value<'scope>) -> Result<f64, Error> {
        Ok(match v.kind() {
            ValueKind::Undefined | ValueKind::Uninit => f64::NAN,
            ValueKind::Null => 0.0,
            ValueKind::Bool(b) => f64::from(u8::from(b)),
            ValueKind::Number(x) => x,
            ValueKind::String(s) => parse_number(&s),
            ValueKind::Symbol(_) => return Err(self.throw(ErrorKind::Type, "Cannot convert a Symbol value to a number")),
            ValueKind::BigInt(_) => return Err(self.throw(ErrorKind::Type, "Cannot convert a BigInt value to a number")),
            ValueKind::Object(_) => {
                let p = self.to_primitive(v, Hint::Number)?;
                return self.to_number(p);
            }
        })
    }

    pub fn to_int32(&mut self, v: Value<'scope>) -> Result<i32, Error> {
        Ok(modulo_u32(self.to_number(v)?).cast_signed())
    }

    pub fn to_uint32(&mut self, v: Value<'scope>) -> Result<u32, Error> {
        Ok(modulo_u32(self.to_number(v)?))
    }

    pub fn text_of(&mut self, v: Value<'scope>) -> Result<crate::SmallStr, Error> {
        Ok(match v.kind() {
            ValueKind::String(s) => crate::SmallStr::from(s.as_str()),
            ValueKind::Number(x) => number_text(x),
            _ => crate::SmallStr::from(self.to_string(v)?.as_str()),
        })
    }

    pub fn to_string(&mut self, v: Value<'scope>) -> Result<JsString<'scope>, Error> {
        Ok(match v.kind() {
            ValueKind::String(s) => s,
            ValueKind::Undefined | ValueKind::Uninit => self.string("undefined"),
            ValueKind::Null => self.string("null"),
            ValueKind::Bool(true) => self.string("true"),
            ValueKind::Bool(false) => self.string("false"),
            ValueKind::Number(x) => crate::JsStr::alloc_known(self.mc, &number_text(x), true),
            ValueKind::Symbol(_) => return Err(self.throw(ErrorKind::Type, "Cannot convert a Symbol value to a string")),
            ValueKind::BigInt(b) => self.string(&format!("{}", b.0)),
            ValueKind::Object(_) => {
                let p = self.to_primitive(v, Hint::String)?;
                return self.to_string(p);
            }
        })
    }

    pub fn symbol_descriptive(&self, s: JsSymbol<'scope>) -> JsString<'scope> {
        self.string(&format!("Symbol({})", s.description.as_deref().unwrap_or("")))
    }

    pub fn type_of(&self, v: Value<'scope>) -> &'static str {
        match v.kind() {
            ValueKind::Undefined | ValueKind::Uninit => "undefined",
            ValueKind::Bool(_) => "boolean",
            ValueKind::Number(_) => "number",
            ValueKind::String(_) => "string",
            ValueKind::Symbol(_) => "symbol",
            ValueKind::BigInt(_) => "bigint",
            ValueKind::Object(o) if o.borrow().kind.is_callable() => "function",
            ValueKind::Null | ValueKind::Object(_) => "object",
        }
    }

    #[inline]
    pub fn try_concat(&self, lhs: Value<'scope>, rhs: Value<'scope>) -> Option<Value<'scope>> {
        match (lhs.kind(), rhs.kind()) {
            (ValueKind::String(a), ValueKind::String(b)) => Some(Value::String(crate::JsStr::concat(self.mc, a, b))),
            (ValueKind::String(s), ValueKind::Number(x)) => Some(self.concat_number(s, x, false)),
            (ValueKind::Number(x), ValueKind::String(s)) => Some(self.concat_number(s, x, true)),
            (ValueKind::String(s), _) => Self::primitive_bytes(rhs).map(|t| Value::String(crate::JsStr::concat_ascii(self.mc, s, t, false))),
            (_, ValueKind::String(s)) => Self::primitive_bytes(lhs).map(|t| Value::String(crate::JsStr::concat_ascii(self.mc, s, t, true))),
            _ => None,
        }
    }

    fn primitive_bytes(v: Value<'scope>) -> Option<&'static [u8]> {
        match v.kind() {
            ValueKind::Bool(true) => Some(b"true"),
            ValueKind::Bool(false) => Some(b"false"),
            ValueKind::Null => Some(b"null"),
            ValueKind::Undefined | ValueKind::Uninit => Some(b"undefined"),
            _ => None,
        }
    }

    #[inline]
    pub fn concat_add(&mut self, lhs: Value<'scope>, rhs: Value<'scope>) -> Result<(Value<'scope>, bool), Error> {
        if let (Some(x), Some(y)) = (lhs.as_number(), rhs.as_number()) {
            return Ok((Value::Number(x + y), true));
        }
        if let Some(v) = self.try_concat(lhs, rhs) {
            return Ok((v, true));
        }
        let keep = lhs.as_object().is_none() && rhs.as_object().is_none();
        Ok((self.binary(BinaryOp::Add, lhs, rhs)?, keep))
    }

    #[inline]
    fn concat_number(&self, s: JsString<'scope>, x: f64, number_first: bool) -> Value<'scope> {
        let mut buf = [0u8; 20];
        if let Some(digits) = integer_digits(x, &mut buf) {
            return Value::String(crate::JsStr::concat_ascii(self.mc, s, digits, number_first));
        }
        self.concat_number_slow(s, x, number_first)
    }

    #[cold]
    #[inline(never)]
    fn concat_number_slow(&self, s: JsString<'scope>, x: f64, number_first: bool) -> Value<'scope> {
        let digits = number_text(x);
        Value::String(crate::JsStr::concat_ascii(self.mc, s, digits.as_str().as_bytes(), number_first))
    }

    const STRING_CONSTANTS: usize = 1024;

    pub fn constant_string(&self, func: u32, slot: u32, text: &str) -> JsString<'scope> {
        if let [b] = text.as_bytes()
            && *b < 128
        {
            return self.context.chars[usize::from(*b)];
        }
        if text.is_empty() {
            return self.context.empty;
        }
        self.intern_constant(func, slot, || self.string(text))
    }

    pub fn constant_units(&self, func: u32, slot: u32, units: &[u16]) -> JsString<'scope> {
        if let &[u] = units
            && u < 128
        {
            return self.context.chars[usize::from(u)];
        }
        if units.is_empty() {
            return self.context.empty;
        }
        self.intern_constant(func, slot, || self.string_units(units))
    }

    fn intern_constant(&self, func: u32, slot: u32, make: impl FnOnce() -> JsString<'scope>) -> JsString<'scope> {
        let key = (func, slot);
        if let Some(s) = self.context.string_constants.get().get(&key).copied() {
            return s;
        }
        let s = make();
        if self.context.string_constants.get().len() < Self::STRING_CONSTANTS {
            self.context.string_constants.update(self.mc, |m| {
                let mut m = m.clone();
                if m.len() < Self::STRING_CONSTANTS {
                    m.insert(key, s);
                }
                m
            });
        }
        s
    }

    pub fn binary(&mut self, op: BinaryOp, lhs: Value<'scope>, rhs: Value<'scope>) -> Result<Value<'scope>, Error> {
        if let (ValueKind::Number(a), ValueKind::Number(b)) = (lhs.kind(), rhs.kind()) {
            return Ok(op.numbers(a, b).into());
        }
        let (l, r) = if op == BinaryOp::Add {
            if let Some(v) = self.try_concat(lhs, rhs) {
                return Ok(v);
            }
            let (l, r) = (self.to_primitive(lhs, Hint::Default)?, self.to_primitive(rhs, Hint::Default)?);
            if let Some(v) = self.try_concat(l, r) {
                return Ok(v);
            }
            if matches!(l.kind(), ValueKind::String(_)) || matches!(r.kind(), ValueKind::String(_)) {
                let (a, b) = (self.to_string(l)?, self.to_string(r)?);
                return Ok(Value::String(crate::JsStr::concat(self.mc, a, b)));
            }
            (l, r)
        } else {
            (lhs, rhs)
        };
        let ln = self.to_numeric(l)?;
        let rn = self.to_numeric(r)?;
        let (a, b) = match (ln, rn) {
            (Numeric::Number(a), Numeric::Number(b)) => (a, b),
            (Numeric::BigInt(a), Numeric::BigInt(b)) => return self.bigint_binary(op, &a.0, &b.0),
            _ => return Err(self.throw(ErrorKind::Type, "Cannot mix BigInt and other types, use explicit conversions")),
        };
        Ok(op.numbers(a, b).into())
    }

    pub fn loose_equals(&mut self, a: Value<'scope>, b: Value<'scope>) -> Result<bool, Error> {
        Ok(match (a.kind(), b.kind()) {
            _ if a.is_nullish() && b.is_nullish() => true,
            (ValueKind::Undefined | ValueKind::Null, _) | (_, ValueKind::Undefined | ValueKind::Null) => false,
            (ValueKind::Object(_), ValueKind::Object(_)) | (ValueKind::Symbol(_), ValueKind::Symbol(_)) => a == b,
            (ValueKind::Object(_), _) => {
                let p = self.to_primitive(a, Hint::Default)?;
                return self.loose_equals(p, b);
            }
            (_, ValueKind::Object(_)) => {
                let p = self.to_primitive(b, Hint::Default)?;
                return self.loose_equals(a, p);
            }
            (ValueKind::Symbol(_), _) | (_, ValueKind::Symbol(_)) => false,
            (ValueKind::BigInt(x), ValueKind::BigInt(y)) => x.0 == y.0,
            (ValueKind::BigInt(x), ValueKind::String(s)) | (ValueKind::String(s), ValueKind::BigInt(x)) => Self::parse_bigint(&s).is_some_and(|y| x.0 == y),
            (ValueKind::BigInt(x), _) | (_, ValueKind::BigInt(x)) => {
                let other = if matches!(a.kind(), ValueKind::BigInt(_)) { b } else { a };
                let n = self.to_number(other)?;
                Self::cmp_bigint_number(&x.0, n) == Some(core::cmp::Ordering::Equal)
            }
            (ValueKind::Number(_), ValueKind::String(_)) | (ValueKind::String(_), ValueKind::Number(_)) | (ValueKind::Bool(_), _) | (_, ValueKind::Bool(_)) => self.to_number(a)? == self.to_number(b)?,
            _ => a == b,
        })
    }

    pub fn compare(&mut self, op: CompareOp, lhs: Value<'scope>, rhs: Value<'scope>) -> Result<bool, Error> {
        if let (ValueKind::Number(a), ValueKind::Number(b)) = (lhs.kind(), rhs.kind()) {
            return Ok(op.numbers(a, b));
        }
        Ok(match op {
            CompareOp::Eq => self.loose_equals(lhs, rhs)?,
            CompareOp::Ne => !self.loose_equals(lhs, rhs)?,
            CompareOp::StrictEq => lhs == rhs,
            CompareOp::StrictNe => lhs != rhs,
            CompareOp::Lt | CompareOp::Gt | CompareOp::Le | CompareOp::Ge => {
                let (l, r) = (self.to_primitive(lhs, Hint::Number)?, self.to_primitive(rhs, Hint::Number)?);
                if let (ValueKind::String(a), ValueKind::String(b)) = (l.kind(), r.kind()) {
                    let ord = a.encode_utf16().cmp(b.encode_utf16());
                    return Ok(match op {
                        CompareOp::Lt => ord.is_lt(),
                        CompareOp::Gt => ord.is_gt(),
                        CompareOp::Le => ord.is_le(),
                        _ => ord.is_ge(),
                    });
                }
                let Some(ord) = self.numeric_order(l, r)? else { return Ok(false) };
                match op {
                    CompareOp::Lt => ord.is_lt(),
                    CompareOp::Gt => ord.is_gt(),
                    CompareOp::Le => ord.is_le(),
                    _ => ord.is_ge(),
                }
            }
        })
    }

    pub fn unary(&mut self, op: UnaryOp, v: Value<'scope>) -> Result<Value<'scope>, Error> {
        Ok(match op {
            UnaryOp::Neg => match self.to_numeric(v)? {
                Numeric::Number(x) => (-x).into(),
                Numeric::BigInt(b) => self.bigint(-&b.0),
            },
            UnaryOp::Plus => self.to_number(v)?.into(),
            UnaryOp::Not => (!self.to_boolean(v)).into(),
            UnaryOp::BitNot => match self.to_numeric(v)? {
                Numeric::Number(x) => f64::from(!modulo_u32(x).cast_signed()).into(),
                Numeric::BigInt(b) => self.bigint(!&b.0),
            },
            UnaryOp::Typeof => self.string(self.type_of(v)).into(),
            UnaryOp::Void => Value::Undefined,
        })
    }

    pub fn to_object(&self, v: Value<'scope>) -> Result<JsObject<'scope>, Error> {
        match (v.kind(), self.proto_of(v)) {
            (ValueKind::Object(o), _) => Ok(o),
            (_, Some(proto)) => Ok(ObjectData { kind: ObjectKind::Primitive(v), proto: Some(proto), ..ObjectData::default() }.alloc(self.mc)),
            (_, None) => Err(self.throw(ErrorKind::Type, "Cannot convert undefined or null to object")),
        }
    }

    fn proto_of(&self, v: Value<'scope>) -> Option<JsObject<'scope>> {
        match v.kind() {
            ValueKind::String(_) => Some(self.context.string_proto),
            ValueKind::Number(_) => Some(self.context.number_proto),
            ValueKind::Bool(_) => Some(self.context.boolean_proto),
            ValueKind::Symbol(_) => Some(self.context.symbol_proto),
            ValueKind::BigInt(_) => Some(self.context.bigint_proto),
            ValueKind::Object(o) => Some(o),
            ValueKind::Undefined | ValueKind::Null | ValueKind::Uninit => None,
        }
    }

    fn nullish_error(&self, target: Value<'scope>, verb: &str, key: &str) -> Error {
        let what = if target == Value::Undefined { "undefined" } else { "null" };
        self.throw(ErrorKind::Type, &format!("Cannot {verb} properties of {what} (reading '{key}')"))
    }

    pub fn get_key(&mut self, holder: Value<'scope>, key: Key<'_, 'scope>, receiver: Value<'scope>) -> Result<Value<'scope>, Error> {
        if holder.is_nullish() {
            let k = self.key_text(key);
            return Err(self.nullish_error(holder, "read", &k));
        }
        if let (ValueKind::String(s), Key::Str(k)) = (holder.kind(), key) {
            if k == "length" {
                return Ok(f64::from(u32::try_from(s.utf16_len()).unwrap_or(u32::MAX)).into());
            }
            if let Some(i) = index_of_str(k)
                && let Some(u) = s.code_unit(i as usize)
            {
                return Ok(self.unit_string(u).into());
            }
        }
        match self.proto_of(holder) {
            Some(o) => self.object_get(o, key, receiver),
            None => Ok(Value::Undefined),
        }
    }

    pub fn object_get(&mut self, o: JsObject<'scope>, key: Key<'_, 'scope>, receiver: Value<'scope>) -> Result<Value<'scope>, Error> {
        let mut cur = o;
        loop {
            let fast = cur.has_ordinary_properties().then(|| (cur.own_slot(self.mc, key), cur.proto()));
            match fast {
                Some((Some(p), _)) => {
                    return match p.slot {
                        Slot::Data(v) => Ok(v),
                        Slot::Accessor { get: Some(g), .. } => self.call(g.into(), receiver, &[]),
                        Slot::Accessor { get: None, .. } => Ok(Value::Undefined),
                    };
                }
                Some((None, Some(proto))) => {
                    cur = proto;
                    continue;
                }
                Some((None, None)) => return Ok(Value::Undefined),
                None => {}
            }
            if let Some(parts) = self.proxy_parts(cur) {
                let (t, h) = parts?;
                return self.proxy_get(t, h, key, receiver);
            }
            if let Some(i) = Self::typed_numeric(cur, key) {
                return Ok(self.typed_get(cur, i).unwrap_or_default());
            }
            if let (Some(b), Key::Str(k)) = (Self::namespace_export(cur, key), key) {
                return match b {
                    Some(b) => self.binding_value(b, k),
                    None => Ok(Value::Undefined),
                };
            }
            if let Some(p) = self.get_own_property(cur, key)? {
                return match p.slot {
                    Slot::Data(v) => Ok(v),
                    Slot::Accessor { get: Some(g), .. } => self.call(g.into(), receiver, &[]),
                    Slot::Accessor { get: None, .. } => Ok(Value::Undefined),
                };
            }
            match self.get_prototype_of(cur)? {
                Some(n) => cur = n,
                None => return Ok(Value::Undefined),
            }
        }
    }

    pub fn get(&mut self, holder: Value<'scope>, key: &str, receiver: Value<'scope>) -> Result<Value<'scope>, Error> {
        self.get_key(holder, Key::Str(key), receiver)
    }

    pub fn get_named(&mut self, target: Value<'scope>, key: &str) -> Result<Value<'scope>, Error> {
        self.get_key(target, Key::Str(key), target)
    }

    pub fn property_key(&mut self, key: Value<'scope>) -> Result<JsString<'scope>, Error> {
        self.to_string(key)
    }

    pub fn get_symbol(&mut self, holder: Value<'scope>, key: JsSymbol<'scope>, receiver: Value<'scope>) -> Result<Value<'scope>, Error> {
        self.get_key(holder, Key::Sym(key), receiver)
    }

    pub fn set_key(&mut self, target: Value<'scope>, key: Key<'_, 'scope>, value: Value<'scope>, receiver: Value<'scope>) -> Result<bool, Error> {
        if target.is_nullish() {
            let k = self.key_text(key);
            return Err(self.nullish_error(target, "set", &k));
        }
        match self.proto_of(target) {
            Some(o) => self.object_set(o, key, value, receiver),
            None => Ok(false),
        }
    }

    pub fn object_set(&mut self, o: JsObject<'scope>, key: Key<'_, 'scope>, value: Value<'scope>, receiver: Value<'scope>) -> Result<bool, Error> {
        let mut cur = o;
        let found = loop {
            let fast = cur.has_ordinary_properties().then(|| (cur.own_slot(self.mc, key), cur.proto()));
            match fast {
                Some((Some(p), _)) => break Some(p),
                Some((None, Some(proto))) => {
                    cur = proto;
                    continue;
                }
                Some((None, None)) => break None,
                None => {}
            }
            if let Some(parts) = self.proxy_parts(cur) {
                let (t, h) = parts?;
                return self.proxy_set(t, h, key, value, receiver);
            }
            if matches!(cur.borrow().kind, ObjectKind::ModuleNamespace(_)) {
                return Ok(false);
            }
            if let Some(i) = Self::typed_numeric(cur, key) {
                if matches!(receiver.kind(), ValueKind::Object(r) if ScopeLocal::ptr_eq(r, cur)) {
                    self.typed_set(cur, i, value)?;
                    return Ok(true);
                }
                if !Self::typed_has(cur, i) {
                    return Ok(true);
                }
            }
            if let Some(p) = self.get_own_property(cur, key)? {
                break Some(p);
            }
            match self.get_prototype_of(cur)? {
                Some(n) => cur = n,
                None => break None,
            }
        };
        match found {
            Some(Property { slot: Slot::Accessor { set: Some(s), .. }, .. }) => {
                self.call(s.into(), receiver, &[value])?;
                return Ok(true);
            }
            Some(Property { slot: Slot::Accessor { set: None, .. }, .. } | Property { writable: false, .. }) => return Ok(false),
            _ => {}
        }
        let Some(r) = receiver.as_object() else { return Ok(false) };
        if let Some(done) = r.borrow_mut(self.mc).ordinary_put(key, value) {
            return Ok(done);
        }
        match self.get_own_property(r, key)? {
            Some(Property { slot: Slot::Accessor { .. }, .. } | Property { writable: false, .. }) => Ok(false),
            Some(_) => self.define_own_property(r, key, Descriptor::value(value)),
            None => self.create_data_property(r, key, value),
        }
    }

    pub fn try_set_symbol(&mut self, target: Value<'scope>, key: JsSymbol<'scope>, value: Value<'scope>) -> Result<bool, Error> {
        self.set_key(target, Key::Sym(key), value, target)
    }

    fn set_failure(&self, target: Value<'scope>, key: &str) -> Error {
        if let Some(o) = target.as_object() {
            let d = o.borrow();
            if d.non_extensible() && d.own_slot(self.mc, Key::Str(key)).is_none() {
                return self.throw(ErrorKind::Type, &format!("Cannot add property {key}, object is not extensible"));
            }
        }
        self.read_only(key)
    }

    fn read_only(&self, key: &str) -> Error {
        self.throw(ErrorKind::Type, &format!("Cannot assign to read only property '{key}'"))
    }

    pub fn has_keyed(&mut self, target: Value<'scope>, key: Value<'scope>) -> Result<bool, Error> {
        if let Some(k) = Self::private_key(key) {
            return self.private_has(target, k);
        }
        let Some(o) = target.as_object() else {
            let what = match key.kind() {
                ValueKind::Symbol(_) => String::from("a symbol"),
                k => format!("'{}'", &**self.to_string(k.into())?),
            };
            let t = self.to_string(target)?;
            return Err(self.throw(ErrorKind::Type, &format!("Cannot use 'in' operator to search for {what} in {}", &**t)));
        };
        match self.to_property_key(key)? {
            PropertyKey::Symbol(k) => self.has_key(o, Key::Sym(k)),
            PropertyKey::String(k) => self.has_key(o, Key::Str(&k)),
        }
    }

    pub fn has_key(&mut self, o: JsObject<'scope>, key: Key<'_, 'scope>) -> Result<bool, Error> {
        let mut cur = o;
        loop {
            if let Some(parts) = self.proxy_parts(cur) {
                let (t, h) = parts?;
                return self.proxy_has(t, h, key);
            }
            if let Some(i) = Self::typed_numeric(cur, key) {
                return Ok(Self::typed_has(cur, i));
            }
            if let Some(b) = Self::namespace_export(cur, key) {
                return Ok(b.is_some());
            }
            if self.get_own_property(cur, key)?.is_some() {
                return Ok(true);
            }
            match self.get_prototype_of(cur)? {
                Some(n) => cur = n,
                None => return Ok(false),
            }
        }
    }

    pub fn delete_keyed(&mut self, target: Value<'scope>, key: Value<'scope>) -> Result<bool, Error> {
        if target.is_nullish() {
            let k = self.to_string(key)?;
            return Err(self.nullish_error(target, "delete", &k));
        }
        match self.to_property_key(key)? {
            PropertyKey::Symbol(k) => self.delete_key(target, Key::Sym(k)),
            PropertyKey::String(k) => self.delete_key(target, Key::Str(&k)),
        }
    }

    pub fn delete_key(&mut self, target: Value<'scope>, key: Key<'_, 'scope>) -> Result<bool, Error> {
        if target.is_nullish() {
            let k = self.key_text(key);
            return Err(self.nullish_error(target, "delete", &k));
        }
        let o = self.to_object(target)?;
        self.delete_property(o, key)
    }

    pub fn define_keyed(&mut self, o: JsObject<'scope>, key: Value<'scope>, prop: Property<'scope>) -> Result<(), Error> {
        if let Some(k) = Self::private_key(key) {
            return self.private_define(o.into(), k, prop);
        }
        match self.to_property_key(key)? {
            PropertyKey::Symbol(k) => self.define_key(o, Key::Sym(k), prop),
            PropertyKey::String(k) => self.define_key(o, Key::Str(&k), prop),
        }
    }

    pub fn define_accessor_keyed(&mut self, o: JsObject<'scope>, key: Value<'scope>, f: JsObject<'scope>, setter: bool, enumerable: bool) -> Result<(), Error> {
        match self.to_property_key(key)? {
            PropertyKey::Symbol(k) => self.define_accessor_key(o, Key::Sym(k), f, setter, enumerable),
            PropertyKey::String(k) => self.define_accessor_key(o, Key::Str(&k), f, setter, enumerable),
        }
        Ok(())
    }

    pub fn get_keyed(&mut self, target: Value<'scope>, key: Value<'scope>) -> Result<Value<'scope>, Error> {
        if let ValueKind::Symbol(k) = key.kind() {
            if k.private {
                return self.private_get(target, k);
            }
            return self.get_symbol(target, k, target);
        }
        if let (ValueKind::Object(o), ValueKind::Number(x)) = (target.kind(), key.kind())
            && o.is_array()
            && let Some(i) = index_of_number(x)
            && let Some(v) = o.elements().get(i as usize)
        {
            return Ok(v);
        }
        if let (ValueKind::Object(o), ValueKind::Number(x)) = (target.kind(), key.kind())
            && matches!(o.borrow().kind, ObjectKind::TypedArray(..))
        {
            return Ok(self.typed_get(o, x + 0.0).unwrap_or_default());
        }
        if let (ValueKind::String(s), ValueKind::Number(x)) = (target.kind(), key.kind())
            && let Some(u) = index_of_number(x).and_then(|i| s.code_unit(i as usize))
        {
            return Ok(self.unit_string(u).into());
        }
        if target.is_nullish() {
            let k = self.to_string(key)?;
            return Err(self.nullish_error(target, "read", &k));
        }
        match self.to_property_key(key)? {
            PropertyKey::Symbol(k) => self.get_symbol(target, k, target),
            PropertyKey::String(k) => self.get_named(target, &k),
        }
    }

    pub fn try_set_named(&mut self, target: Value<'scope>, key: &str, value: Value<'scope>) -> Result<bool, Error> {
        if let (ValueKind::Object(o), Some(i)) = (target.kind(), index_of_str(key))
            && o.borrow_mut(self.mc).write_index(i, value)
        {
            return Ok(true);
        }
        self.set_key(target, Key::Str(key), value, target)
    }

    pub fn set_named(&mut self, target: Value<'scope>, key: &str, value: Value<'scope>) -> Result<(), Error> {
        if self.try_set_named(target, key, value)? { Ok(()) } else { Err(self.set_failure(target, key)) }
    }

    pub fn try_set_keyed(&mut self, target: Value<'scope>, key: Value<'scope>, value: Value<'scope>) -> Result<bool, Error> {
        if let ValueKind::Symbol(k) = key.kind() {
            return self.try_set_symbol(target, k, value);
        }
        if let (ValueKind::Object(o), ValueKind::Number(x)) = (target.kind(), key.kind())
            && let Some(i) = index_of_number(x)
            && o.borrow_mut(self.mc).write_index(i, value)
        {
            return Ok(true);
        }
        if let (ValueKind::Object(o), ValueKind::Number(x)) = (target.kind(), key.kind())
            && matches!(o.borrow().kind, ObjectKind::TypedArray(..))
        {
            self.typed_set(o, x + 0.0, value)?;
            return Ok(true);
        }
        if target.is_nullish() {
            let k = self.to_string(key)?;
            return Err(self.nullish_error(target, "set", &k));
        }
        match self.to_property_key(key)? {
            PropertyKey::Symbol(k) => self.try_set_symbol(target, k, value),
            PropertyKey::String(k) => self.try_set_named(target, &k, value),
        }
    }

    pub fn set_keyed(&mut self, target: Value<'scope>, key: Value<'scope>, value: Value<'scope>) -> Result<(), Error> {
        if self.try_set_keyed(target, key, value)? {
            Ok(())
        } else {
            let k = match key.kind() {
                ValueKind::Symbol(s) => self.symbol_descriptive(s),
                k => self.to_string(k.into())?,
            };
            Err(self.read_only(&k))
        }
    }

    pub fn put_named(&mut self, target: Value<'scope>, key: &str, value: Value<'scope>, strict: bool) -> Result<(), Error> {
        if strict { self.set_named(target, key, value) } else { self.try_set_named(target, key, value).map(drop) }
    }

    pub fn put_keyed(&mut self, target: Value<'scope>, key: Value<'scope>, value: Value<'scope>, strict: bool) -> Result<(), Error> {
        if let Some(k) = Self::private_key(key) {
            return self.private_set(target, k, value);
        }
        if strict { self.set_keyed(target, key, value) } else { self.try_set_keyed(target, key, value).map(drop) }
    }

    pub fn define(&mut self, o: JsObject<'scope>, key: &str, prop: Property<'scope>) -> Result<(), Error> {
        self.define_key(o, Key::Str(key), prop)
    }

    pub fn define_key(&mut self, o: JsObject<'scope>, key: Key<'_, 'scope>, prop: Property<'scope>) -> Result<(), Error> {
        if self.define_own_property(o, key, prop.into())? {
            Ok(())
        } else {
            let k = self.key_text(key);
            Err(self.throw(ErrorKind::Type, &format!("Cannot define property {k}, object is not extensible")))
        }
    }

    fn define_accessor_key(&mut self, o: JsObject<'scope>, key: Key<'_, 'scope>, f: JsObject<'scope>, setter: bool, enumerable: bool) {
        let (mut get, mut set) = match o.borrow().own_slot(self.mc, key).map(|p| p.slot) {
            Some(Slot::Accessor { get, set }) => (get, set),
            _ => (None, None),
        };
        if setter { set = Some(f) } else { get = Some(f) }
        o.borrow_mut(self.mc).store(key, Property { slot: Slot::Accessor { get, set }, enumerable, writable: true, configurable: true });
    }

    pub fn has_property(&mut self, target: Value<'scope>, key: &str) -> Result<bool, Error> {
        let Some(o) = target.as_object() else {
            let what = self.to_string(target)?;
            return Err(self.throw(ErrorKind::Type, &format!("Cannot use 'in' operator to search for '{key}' in {}", &**what)));
        };
        self.has_key(o, Key::Str(key))
    }

    pub fn delete(&mut self, target: Value<'scope>, key: &str) -> Result<bool, Error> {
        self.delete_key(target, Key::Str(key))
    }

    pub fn can_be_held_weakly(&self, v: Value<'scope>) -> bool {
        match v.kind() {
            ValueKind::Object(_) => true,
            ValueKind::Symbol(s) => !self.context.symbol_registry.get().iter().any(|(_, x)| ScopeLocal::ptr_eq(*x, s)),
            _ => false,
        }
    }

    pub fn describe(&self, v: Value<'scope>) -> String {
        match v.kind() {
            ValueKind::Undefined | ValueKind::Uninit => String::from("undefined"),
            ValueKind::Null => String::from("null"),
            ValueKind::Bool(b) => String::from(if b { "true" } else { "false" }),
            ValueKind::Number(x) => String::from(&*number_text(x)),
            ValueKind::String(s) => format!("\"{}\"", &**s),
            ValueKind::Symbol(s) => String::from(&**self.symbol_descriptive(s)),
            ValueKind::BigInt(b) => format!("{}n", b.0),
            ValueKind::Object(o) => {
                let d = o.borrow();
                if d.kind.is_callable() {
                    match d.get_own(self.mc, "name").and_then(|p| p.value()).map(Value::kind) {
                        Some(ValueKind::String(n)) if !n.is_empty() => String::from(&**n),
                        _ => String::from("function"),
                    }
                } else if matches!(d.kind, ObjectKind::Array) {
                    String::from("[object Array]")
                } else {
                    String::from("#<Object>")
                }
            }
        }
    }

    pub fn instance_of(&mut self, v: Value<'scope>, f: Value<'scope>) -> Result<bool, Error> {
        if f.as_object().is_none() {
            let what = self.describe(f);
            return Err(self.throw(ErrorKind::Type, &format!("Right-hand side of 'instanceof' is not an object: {what}")));
        }
        let h = self.get_symbol(f, self.context.symbols.has_instance, f)?;
        if !h.is_nullish() {
            let r = self.call(h, f, &[v])?;
            return Ok(self.to_boolean(r));
        }
        if !self.is_callable(f) {
            return Err(self.throw(ErrorKind::Type, "Right-hand side of 'instanceof' is not callable"));
        }
        self.ordinary_has_instance(f, v)
    }

    pub fn ordinary_has_instance(&mut self, f: Value<'scope>, v: Value<'scope>) -> Result<bool, Error> {
        if !self.is_callable(f) {
            return Ok(false);
        }
        if let Some(o) = f.as_object()
            && let ObjectKind::Bound(b) = &o.borrow().kind
        {
            let target = b.target;
            return self.instance_of(v, target);
        }
        let Some(o) = v.as_object() else { return Ok(false) };
        let Some(proto) = (self.get_named(f, "prototype")?).as_object() else {
            return Err(self.throw(ErrorKind::Type, "Function has non-object prototype in instanceof check"));
        };
        let mut cur = self.get_prototype_of(o)?;
        while let Some(c) = cur {
            if ScopeLocal::ptr_eq(c, proto) {
                return Ok(true);
            }
            cur = self.get_prototype_of(c)?;
        }
        Ok(false)
    }

    pub fn to_integer_or_infinity(&mut self, v: Value<'scope>) -> Result<f64, Error> {
        let x = self.to_number(v)?;
        const I64_SAFE: f64 = 9_223_372_036_854_775_808.0;
        #[expect(clippy::cast_possible_truncation, clippy::cast_precision_loss, reason = "magnitude below 2^63; dropping the fraction is intended")]
        Ok(if x.is_nan() {
            0.0
        } else if x.abs() < I64_SAFE {
            (x as i64) as f64
        } else {
            libm::trunc(x) + 0.0
        })
    }

    pub fn to_length(&mut self, v: Value<'scope>) -> Result<f64, Error> {
        let n = self.to_integer_or_infinity(v)?;
        Ok(n.clamp(0.0, MAX_SAFE_INTEGER))
    }

    pub fn length_of_array_like(&mut self, o: Value<'scope>) -> Result<f64, Error> {
        if let Some(x) = o.as_object()
            && let Some(n) = x.borrow().array_length()
        {
            return Ok(f64::from(n));
        }
        let len = self.get_named(o, "length")?;
        self.to_length(len)
    }

    pub fn index_key(i: f64) -> String {
        String::from(&*number_text(i))
    }

    pub fn get_index(&mut self, o: Value<'scope>, i: f64) -> Result<Value<'scope>, Error> {
        if let Some(x) = o.as_object()
            && matches!(x.borrow().kind, ObjectKind::TypedArray(..))
        {
            return Ok(self.typed_get(x, i).unwrap_or_default());
        }
        if let Some(x) = o.as_object()
            && x.is_array()
            && let Some(n) = index_of_number(i)
            && let Some(v) = x.elements().get(n as usize)
        {
            return Ok(v);
        }
        self.get_named(o, &Self::index_key(i))
    }

    pub fn set_index(&mut self, o: Value<'scope>, i: f64, v: Value<'scope>) -> Result<(), Error> {
        if let (ValueKind::Object(x), Some(n)) = (o.kind(), index_of_number(i))
            && x.borrow_mut(self.mc).write_index(n, v)
        {
            return Ok(());
        }
        self.set_named(o, &Self::index_key(i), v)
    }

    pub fn has_index(&mut self, o: JsObject<'scope>, i: f64) -> Result<bool, Error> {
        if o.is_array()
            && let Some(n) = index_of_number(i)
            && (n as usize) < o.elements().len()
        {
            return Ok(true);
        }
        self.has_key(o, Key::Str(&Self::index_key(i)))
    }

    pub fn present_index(&mut self, o: JsObject<'scope>, i: f64) -> Result<Option<Value<'scope>>, Error> {
        if o.is_array()
            && let Some(n) = index_of_number(i)
            && let Some(v) = o.elements().get(n as usize)
        {
            return Ok(Some(v));
        }
        if self.has_index(o, i)? { self.get_index(o.into(), i).map(Some) } else { Ok(None) }
    }

    pub fn delete_index(&mut self, o: JsObject<'scope>, i: f64) -> Result<(), Error> {
        let k = Self::index_key(i);
        if self.delete_property(o, Key::Str(&k))? {
            Ok(())
        } else {
            Err(self.throw(ErrorKind::Type, &format!("Cannot delete property '{k}' of {}", "[object Array]")))
        }
    }

    pub fn create_index(&mut self, o: JsObject<'scope>, i: f64, v: Value<'scope>) -> Result<(), Error> {
        if let Some(n) = index_of_number(i) {
            let append = !o.non_extensible() && !o.length_readonly() && (o.sparse_length() <= n || !o.digit_keys());
            if o.is_array() {
                let (at, items) = (n as usize, o.elements());
                if at < items.len() {
                    items.set(self.mc, at, v);
                    return Ok(());
                }
                if at == items.len() && append {
                    items.push(self.mc, v);
                    return Ok(());
                }
            }
        }
        self.create_data_property_or_throw(o, Key::Str(&Self::index_key(i)), v)
    }

    pub fn is_array(&mut self, v: Value<'scope>) -> Result<bool, Error> {
        let Some(o) = v.as_object() else { return Ok(false) };
        if let Some(parts) = self.proxy_parts(o) {
            let (t, _) = parts?;
            return self.is_array(t.into());
        }
        Ok(o.is_array())
    }

    pub fn list_from_array_like(&mut self, v: Value<'scope>) -> Result<Vec<Value<'scope>>, Error> {
        if v.as_object().is_none() {
            return Err(self.throw(ErrorKind::Type, "CreateListFromArrayLike called on non-object"));
        }
        let len = self.length_of_array_like(v)?;
        let mut out = Vec::new();
        let mut i = 0.0;
        while i < len {
            out.push(self.get_index(v, i)?);
            i += 1.0;
        }
        Ok(out)
    }

    pub fn get_iterator(&mut self, v: Value<'scope>) -> Result<IterRecord<'scope>, Error> {
        let iterator = self.iterator_object(v)?;
        let next = self.get_named(iterator, "next")?;
        Ok(IterRecord { iterator, next })
    }

    pub fn iterator_object(&mut self, v: Value<'scope>) -> Result<Value<'scope>, Error> {
        let method = if v.is_nullish() { Value::Undefined } else { self.get_symbol(v, self.context.symbols.iterator, v)? };
        if !self.is_callable(method) {
            let what = match v.kind() {
                ValueKind::Symbol(s) => self.symbol_descriptive(s),
                _ => self.to_string(v)?,
            };
            return Err(self.throw(ErrorKind::Type, &format!("{} is not iterable", &**what)));
        }
        let iterator = self.call(method, v, &[])?;
        if iterator.as_object().is_none() {
            return Err(self.throw(ErrorKind::Type, "Result of the Symbol.iterator method is not an object"));
        }
        Ok(iterator)
    }

    pub fn array_iter_step(&mut self, o: JsObject<'scope>) -> Result<Option<Value<'scope>>, Error> {
        let ObjectKind::Iterator(IteratorState::Array { target, index, kind }) = o.borrow().kind else {
            return Err(self.throw(ErrorKind::Type, "next method called on an incompatible receiver"));
        };
        let Some(target) = target else { return Ok(None) };
        let len = match target.kind() {
            ValueKind::Object(t) => (t.is_array() && t.sparse_length() == 0).then(|| t.elements().len()),
            _ => None,
        };
        let len = match len {
            #[expect(clippy::cast_precision_loss, reason = "the element count is far below 2^53")]
            Some(n) => n as f64,
            None => {
                let l = self.get_named(target, "length")?;
                self.to_number(l)?
            }
        };
        let i = f64::from(u32::try_from(index).unwrap_or(u32::MAX));
        let finished = i.partial_cmp(&len) != Some(core::cmp::Ordering::Less);
        if let ObjectKind::Iterator(IteratorState::Array { target, index, .. }) = &mut o.borrow_mut(self.mc).kind {
            if finished {
                *target = None;
            } else {
                *index += 1;
            }
        }
        if finished {
            return Ok(None);
        }
        Ok(Some(match kind {
            IterKind::Keys => i.into(),
            IterKind::Values => self.get_keyed(target, i.into())?,
            IterKind::Entries => {
                let v = self.get_keyed(target, i.into())?;
                self.array(alloc::vec![i.into(), v]).into()
            }
        }))
    }

    pub fn string_iter_step(&mut self, o: JsObject<'scope>) -> Result<Option<Value<'scope>>, Error> {
        let ObjectKind::Iterator(IteratorState::String { s, pos }) = o.borrow().kind else {
            return Err(self.throw(ErrorKind::Type, "next method called on an incompatible receiver"));
        };
        if let Some(w) = s.wide().filter(|_| s.lone_surrogates()) {
            let Some(&first) = w.get(pos) else { return Ok(None) };
            let pair = (0xD800..0xDC00).contains(&first) && w.get(pos + 1).is_some_and(|u| (0xDC00..0xE000).contains(u));
            let n = if pair { 2 } else { 1 };
            if let ObjectKind::Iterator(IteratorState::String { pos, .. }) = &mut o.borrow_mut(self.mc).kind {
                *pos += n;
            }
            return Ok(Some(self.string_units(&w[pos..pos + n]).into()));
        }
        let Some(c) = s[pos..].chars().next() else { return Ok(None) };
        if let ObjectKind::Iterator(IteratorState::String { pos, .. }) = &mut o.borrow_mut(self.mc).kind {
            *pos += c.len_utf8();
        }
        Ok(Some(if c.is_ascii() { self.unit_string(u16::from(c as u8)) } else { self.string(c.encode_utf8(&mut [0; 4])) }.into()))
    }

    pub fn iterator_step(&mut self, it: IterRecord<'scope>) -> Result<Option<Value<'scope>>, Error> {
        if let (ValueKind::Object(n), ValueKind::Object(o)) = (it.next.kind(), it.iterator.kind()) {
            let [array, string] = *self.context.builtin_nexts.get();
            let (is_array, is_string) = match &o.borrow().kind {
                ObjectKind::Iterator(IteratorState::Array { .. }) => (true, false),
                ObjectKind::Iterator(IteratorState::String { .. }) => (false, true),
                _ => (false, false),
            };
            let builtin = if is_array { array } else if is_string { string } else { None };
            if builtin.is_some_and(|k| ScopeLocal::ptr_eq(k, n)) {
                return if is_array { self.array_iter_step(o) } else { self.string_iter_step(o) };
            }
        }
        let r = self.call(it.next, it.iterator, &[])?;
        if r.as_object().is_none() {
            let what = self.to_string(r)?;
            return Err(self.throw(ErrorKind::Type, &format!("Iterator result {} is not an object", &**what)));
        }
        let done = self.get_named(r, "done")?;
        if self.to_boolean(done) { Ok(None) } else { Ok(Some(self.get_named(r, "value")?)) }
    }

    pub fn iterator_close(&mut self, iterator: Value<'scope>) -> Result<(), Error> {
        let ret = self.get_named(iterator, "return")?;
        if ret.is_nullish() {
            return Ok(());
        }
        let r = self.call(ret, iterator, &[])?;
        if r.as_object().is_none() {
            return Err(self.throw(ErrorKind::Type, "Iterator result is not an object"));
        }
        Ok(())
    }

    pub fn iterator_close_abrupt(&mut self, iterator: Value<'scope>, e: Error) -> Error {
        if matches!(e, Error::Terminated) {
            return e;
        }
        let saved = self.mc.take_exception();
        let _ = self.iterator_close(iterator);
        self.mc.take_exception();
        if let Some(v) = saved {
            self.mc.raise(v);
        }
        e
    }

    pub fn iter_result(&self, value: Value<'scope>, done: bool) -> JsObject<'scope> {
        let o = self.object();
        let d = o.borrow_mut(self.mc);
        d.set("value", value);
        d.set("done", done.into());
        drop(d);
        o
    }

    pub fn builtin_iterator(&self, state: IteratorState<'scope>) -> JsObject<'scope> {
        let proto = match state {
            IteratorState::Array { .. } => self.context.array_iterator_proto,
            IteratorState::String { .. } => self.context.string_iterator_proto,
        };
        ObjectData { kind: ObjectKind::Iterator(state), proto: Some(proto), ..ObjectData::default() }.alloc(self.mc)
    }

    pub fn array_iterator(&self, target: Value<'scope>, kind: IterKind) -> JsObject<'scope> {
        self.builtin_iterator(IteratorState::Array { target: Some(target), index: 0, kind })
    }

    pub fn to_list(&mut self, v: Value<'scope>) -> Result<Vec<Value<'scope>>, Error> {
        let it = self.get_iterator(v)?;
        let mut out = Vec::new();
        while let Some(x) = self.iterator_step(it)? {
            out.push(x);
        }
        Ok(out)
    }

    pub fn copy_data(&mut self, target: JsObject<'scope>, src: Value<'scope>, exclude: &[JsString<'scope>]) -> Result<(), Error> {
        if src.is_nullish() {
            return Ok(());
        }
        let from = self.to_object(src)?;
        for k in self.own_property_keys(from)? {
            if let PropertyKey::String(s) = k
                && exclude.iter().any(|e| ***e == **s)
            {
                continue;
            }
            let key = k.key();
            if self.get_own_property(from, key)?.is_some_and(|p| p.enumerable) {
                let v = self.get_key(from.into(), key, from.into())?;
                self.create_data_property_or_throw(target, key, v)?;
            }
        }
        Ok(())
    }

    pub fn for_in_keys(&mut self, v: Value<'scope>) -> Result<Vec<Value<'scope>>, Error> {
        let mut seen: Vec<JsString<'scope>> = Vec::new();
        let mut out = Vec::new();
        if v.is_nullish() {
            return Ok(out);
        }
        let mut cur = Some(self.to_object(v)?);
        while let Some(o) = cur {
            for k in self.own_property_keys(o)? {
                let PropertyKey::String(s) = k else { continue };
                if seen.iter().any(|x| **x == *s) {
                    continue;
                }
                seen.push(s);
                if self.get_own_property(o, Key::Str(&s))?.is_some_and(|p| p.enumerable) {
                    out.push(Value::String(s));
                }
            }
            cur = self.get_prototype_of(o)?;
        }
        Ok(out)
    }

    pub fn create_this(&mut self, new_target: Value<'scope>) -> Result<JsObject<'scope>, Error> {
        let proto = match (self.get_named(new_target, "prototype")?).kind() {
            ValueKind::Object(p) => p,
            _ => self.context.object_proto,
        };
        Ok(ObjectData { proto: Some(proto), ..ObjectData::default() }.alloc(self.mc))
    }
}
