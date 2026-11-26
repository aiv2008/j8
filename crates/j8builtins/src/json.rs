use j8framework::ValueKind;
use alloc::borrow::Cow;
use alloc::string::String;
use alloc::vec::Vec;

use j8framework::{Scope, Error, ErrorKind, JsObject, JsString, ObjectData, Property, ShapeRef, Slot, SmallStr, Value};

use crate::{Args, Installer};

const MAX_GAP: usize = 10;

const SHAPES_CACHED: usize = 8;

const PAR_ARRAY: usize = 1 << 14;

const TOJSON_SHAPES: usize = 32;

const TOJSON_PROTOS: usize = 8;

const SHAPES_WRITTEN: usize = 32;

const RESERVE_MAX: usize = 1 << 20;

const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

const ESCAPE: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 0x20 {
        t[i] = 1;
        i += 1;
    }
    t[b'"' as usize] = 1;
    t[b'\\' as usize] = 1;
    t
};

struct Parser<'s, 'a, 'scope, 'x> {
    a: &'a mut Scope<'scope, 'x>,
    text: &'s str,
    pos: usize,
    shapes: Vec<(Vec<String>, ShapeRef<'scope>)>,
    spare_pairs: Vec<Vec<(Cow<'s, str>, Value<'scope>)>>,
    spare_items: Vec<Vec<Value<'scope>>>,
}

impl<'s, 'scope> Parser<'s, '_, 'scope, '_> {
    fn error(&self, what: &str) -> Error {
        let found = self.text[self.pos..].chars().next().map_or_else(|| String::from("end of JSON input"), |c| alloc::format!("token '{c}'"));
        self.a.throw(ErrorKind::Syntax, &alloc::format!("Unexpected {found} in JSON at position {} ({what})", self.pos))
    }

    fn skip_ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.text.as_bytes().get(self.pos) {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.pos).copied()
    }

    fn eat(&mut self, lit: &str) -> bool {
        if self.text[self.pos..].starts_with(lit) {
            self.pos += lit.len();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, b: u8, what: &str) -> Result<(), Error> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.error(what))
        }
    }

    fn value(&mut self) -> Result<Value<'scope>, Error> {
        self.skip_ws();
        let v = match self.peek() {
            Some(b'{') => self.object()?,
            Some(b'[') => self.array()?,
            Some(b'"') => {
                let s = self.string()?;
                self.a.string(&s).into()
            }
            Some(b'-' | b'0'..=b'9') => self.number()?,
            _ if self.eat("true") => true.into(),
            _ if self.eat("false") => false.into(),
            _ if self.eat("null") => Value::Null,
            _ => return Err(self.error("expected a value")),
        };
        self.skip_ws();
        Ok(v)
    }

    fn object(&mut self) -> Result<Value<'scope>, Error> {
        self.a.check_stack()?;
        self.pos += 1;
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(self.a.object().into());
        }
        let mut pairs = self.spare_pairs.pop().unwrap_or_default();
        pairs.clear();
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.error("expected a property name"));
            }
            let k = self.string()?;
            self.skip_ws();
            self.expect(b':', "expected ':'")?;
            let v = self.value()?;
            pairs.push((k, v));
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    let o = self.build(&pairs);
                    pairs.clear();
                    self.spare_pairs.push(pairs);
                    return Ok(o.into());
                }
                _ => return Err(self.error("expected ',' or '}'")),
            }
        }
    }

    fn build(&mut self, pairs: &[(Cow<'s, str>, Value<'scope>)]) -> JsObject<'scope> {
        let n = pairs.len();
        if let Some(hit) = self.shapes.iter().rposition(|(keys, shape)| shape.len() == n && keys.len() == n && keys.iter().zip(pairs).all(|(k, (p, _))| k.as_str() == p.as_ref())) {
            let last = self.shapes.len() - 1;
            if hit != last {
                self.shapes.swap(hit, last);
            }
            let o = self.a.object_with(n);
            o.fill(self.a.mc, self.shapes[last].1, pairs.iter().map(|&(_, v)| Slot::Data(v)));
            return o;
        }
        let o = self.a.object_with(n);
        for (k, v) in pairs {
            o.borrow_mut(self.a.mc).define(k, Property::data(*v));
        }
        if self.shapes.len() < SHAPES_CACHED
            && let Some(shape) = o.get().props().shape().filter(|s| s.len() == n)
        {
            self.shapes.push((pairs.iter().map(|(k, _)| String::from(k.as_ref())).collect(), shape));
        }
        o
    }

    fn array(&mut self) -> Result<Value<'scope>, Error> {
        self.a.check_stack()?;
        self.pos += 1;
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(self.a.array_from_slice(&[]).into());
        }
        let mut items = self.spare_items.pop().unwrap_or_default();
        items.clear();
        loop {
            items.push(self.value()?);
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    let o = self.a.array_from_slice(&items);
                    items.clear();
                    self.spare_items.push(items);
                    return Ok(o.into());
                }
                _ => return Err(self.error("expected ',' or ']'")),
            }
        }
    }

    fn hex4(&mut self) -> Result<u16, Error> {
        let digits = self.text.get(self.pos..self.pos + 4).filter(|d| d.bytes().all(|c| c.is_ascii_hexdigit())).ok_or_else(|| self.error("bad \\u escape"))?;
        self.pos += 4;
        u16::from_str_radix(digits, 16).map_err(|_| self.error("bad \\u escape"))
    }

    fn string(&mut self) -> Result<Cow<'s, str>, Error> {
        let start = self.pos + 1;
        let text: &'s str = self.text;
        let end = text.as_bytes()[start..].iter().position(|&b| b == b'"' || b == b'\\' || b < 0x20).map(|i| start + i);
        if let Some(end) = end
            && text.as_bytes()[end] == b'"'
        {
            self.pos = end + 1;
            return Ok(Cow::Borrowed(&text[start..end]));
        }
        self.escaped().map(Cow::Owned)
    }

    fn escaped(&mut self) -> Result<String, Error> {
        self.pos += 1;
        let mut out = String::new();
        let mut units: Vec<u16> = Vec::new();
        loop {
            let c = self.text[self.pos..].chars().next().ok_or_else(|| self.error("unterminated string"))?;
            if c != '\\' && !units.is_empty() {
                out.push_str(&String::from_utf16_lossy(&units));
                units.clear();
            }
            match c {
                '"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                '\u{0}'..='\u{1f}' => return Err(self.error("bad control character in string")),
                '\\' => {
                    self.pos += 1;
                    let e = self.peek().ok_or_else(|| self.error("unterminated string"))?;
                    self.pos += 1;
                    let simple = match e {
                        b'"' => Some('"'),
                        b'\\' => Some('\\'),
                        b'/' => Some('/'),
                        b'b' => Some('\u{8}'),
                        b'f' => Some('\u{c}'),
                        b'n' => Some('\n'),
                        b'r' => Some('\r'),
                        b't' => Some('\t'),
                        b'u' => None,
                        _ => {
                            self.pos -= 1;
                            return Err(self.error("bad escape"));
                        }
                    };
                    match simple {
                        Some(ch) => {
                            if !units.is_empty() {
                                out.push_str(&String::from_utf16_lossy(&units));
                                units.clear();
                            }
                            out.push(ch);
                        }
                        None => units.push(self.hex4()?),
                    }
                }
                c => {
                    out.push(c);
                    self.pos += c.len_utf8();
                }
            }
        }
    }

    fn number(&mut self) -> Result<Value<'scope>, Error> {
        let start = self.pos;
        let neg = self.peek() == Some(b'-');
        if neg {
            self.pos += 1;
        }
        let digits_at = self.pos;
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                while let Some(b'0'..=b'9') = self.peek() {
                    self.pos += 1;
                }
            }
            _ => return Err(self.error("bad number")),
        }
        let digits_end = self.pos;
        let mut plain = true;
        if self.peek() == Some(b'.') {
            plain = false;
            self.pos += 1;
            let frac = self.pos;
            while let Some(b'0'..=b'9') = self.peek() {
                self.pos += 1;
            }
            if self.pos == frac {
                return Err(self.error("bad number"));
            }
        }
        if let Some(b'e' | b'E') = self.peek() {
            plain = false;
            self.pos += 1;
            if let Some(b'+' | b'-') = self.peek() {
                self.pos += 1;
            }
            let exp = self.pos;
            while let Some(b'0'..=b'9') = self.peek() {
                self.pos += 1;
            }
            if self.pos == exp {
                return Err(self.error("bad number"));
            }
        }
        if plain && digits_end - digits_at <= 15 {
            return Ok(int_f64(&self.text.as_bytes()[digits_at..digits_end], neg).into());
        }
        let x: f64 = self.text[start..self.pos].parse().map_err(|_| self.error("bad number"))?;
        Ok(x.into())
    }
}

struct CachedShape<'scope> {
    ptr: usize,
    shape: ShapeRef<'scope>,
    base: usize,
    n: usize,
}

struct Writer<'scope> {
    gap: String,
    indent: String,
    allow: Option<Vec<JsString<'scope>>>,
    replacer: Option<Value<'scope>>,
    stack: j8framework::Visiting<'scope>,
    clean_shapes: Vec<usize>,
    clean_protos: Vec<JsObject<'scope>>,
    quoted: String,
    cached_shapes: Vec<CachedShape<'scope>>,
    cached_at: Vec<usize>,
    cached_len: Vec<usize>,
}

impl<'scope> Writer<'scope> {
    fn quote_string(out: &mut String, s: &j8framework::JsStr<'_>) {
        match s.wide().filter(|_| s.lone_surrogates()) {
            Some(units) => quote_units(out, units),
            None => Self::quote(out, s.as_str()),
        }
    }

    fn quote(out: &mut String, s: &str) {
        let bytes = s.as_bytes();
        if bytes.is_empty() {
            out.push_str("\"\"");
            return;
        }
        if bytes.len() == 1 && ESCAPE[usize::from(bytes[0])] == 0 {
            out.push('"');
            out.push(char::from(bytes[0]));
            out.push('"');
            return;
        }
        let mut i = 0;
        let n = bytes.len();
        while i < n && ESCAPE[usize::from(bytes[i])] == 0 {
            i += 1;
        }
        if i == n {
            out.reserve(n + 2);
            out.push('"');
            out.push_str(s);
            out.push('"');
            return;
        }
        quote_from(out, s, i);
    }

    fn keys(o: JsObject<'scope>) -> Vec<(SmallStr, Option<usize>)> {
        let d = o.borrow();
        let mut keys = Vec::new();
        if o.is_array() {
            keys.extend((0..o.elements().len()).map(|i| (j8framework::number_text(f64::from(u32::try_from(i).unwrap_or(u32::MAX))), None)));
        }
        let props = d.props();
        let direct = o.get().is_plain() && !props.is_dict();
        keys.extend((0..props.len()).filter(|&i| props.attrs(i).enumerable()).map(|i| (props.key(i), direct.then_some(i))));
        keys
    }

    #[cold]
    fn user_code(&mut self) {
        self.clean_shapes.clear();
        self.clean_protos.clear();
    }

    fn owns_to_json(&mut self, o: JsObject<'scope>) -> bool {
        const KEY: &str = "toJSON";
        let props = o.get().props();
        if let Some(ptr) = props.shape_id() {
            if self.clean_shapes.contains(&ptr) {
                return false;
            }
            if props.position(KEY).is_some() {
                return true;
            }
            if self.clean_shapes.len() < TOJSON_SHAPES {
                self.clean_shapes.push(ptr);
            }
            return false;
        }
        props.position(KEY).is_some()
    }

    fn remember_proto(&mut self, o: JsObject<'scope>) {
        if self.clean_protos.len() < TOJSON_PROTOS && !self.clean_protos.iter().any(|&p| j8framework::ScopeLocal::ptr_eq(p, o)) {
            self.clean_protos.push(o);
        }
    }

    fn to_json_absent(&self, v: Value<'scope>) -> bool {
        let Some(o) = v.as_object() else {
            return !matches!(v.kind(), ValueKind::BigInt(_));
        };
        if !o.get().is_plain() {
            return false;
        }
        let props = o.get().props();
        let own_clean = match props.shape_id() {
            Some(ptr) => self.clean_shapes.contains(&ptr),
            None => !props.is_dict(),
        };
        if !own_clean {
            return false;
        }
        match o.get().proto() {
            Some(p) => self.clean_protos.iter().any(|&c| j8framework::ScopeLocal::ptr_eq(c, p)),
            None => true,
        }
    }

    fn resolve_to_json(&mut self, a: &mut Scope<'scope, '_>, v: Value<'scope>) -> Result<Value<'scope>, Error> {
        const KEY: &str = "toJSON";
        if self.to_json_absent(v) {
            return Ok(Value::Undefined);
        }
        let Some(mut o) = v.as_object() else {
            self.user_code();
            return a.get_named(v, KEY);
        };
        let mut root = true;
        loop {
            if self.clean_protos.iter().any(|&p| j8framework::ScopeLocal::ptr_eq(p, o)) {
                return Ok(Value::Undefined);
            }
            if !o.get().is_plain() || self.owns_to_json(o) {
                self.user_code();
                return a.get_named(v, KEY);
            }
            let Some(p) = o.get().proto() else {
                if !root {
                    self.remember_proto(o);
                }
                return Ok(Value::Undefined);
            };
            if !root {
                self.remember_proto(o);
            }
            root = false;
            o = p;
        }
    }

    fn property(&mut self, a: &mut Scope<'scope, '_>, holder: Value<'scope>, key: &str, read: Option<Value<'scope>>, out: &mut String) -> Result<bool, Error> {
        let mut v = match read {
            Some(v) => v,
            None => {
                let v = a.get_named(holder, key)?;
                self.user_code();
                v
            }
        };
        if matches!(v.kind(), ValueKind::Object(_) | ValueKind::BigInt(_)) {
            let to_json = self.resolve_to_json(a, v)?;
            if a.is_callable(to_json) {
                let k = a.string(key);
                v = a.call(to_json, v, &[k.into()])?;
                self.user_code();
            }
        }
        if let Some(r) = self.replacer {
            let k = a.string(key);
            v = a.call(r, holder, &[k.into(), v])?;
            self.user_code();
        }
        self.write_value(a, v, out)
    }

    fn write_value(&mut self, a: &mut Scope<'scope, '_>, v: Value<'scope>, out: &mut String) -> Result<bool, Error> {
        match v.kind() {
            ValueKind::Null => out.push_str("null"),
            ValueKind::Number(x) if !x.is_finite() => out.push_str("null"),
            ValueKind::Bool(true) => out.push_str("true"),
            ValueKind::Bool(false) => out.push_str("false"),
            ValueKind::String(s) => Self::quote_string(out, j8framework::ScopeLocal::get(s)),
            ValueKind::Number(x) => push_number(out, x),
            ValueKind::BigInt(_) => return Err(a.throw(ErrorKind::Type, "Do not know how to serialize a BigInt")),
            ValueKind::Object(o) if !o.is_callable() => {
                a.check_stack()?;
                if !self.stack.enter(o) {
                    return Err(a.throw(ErrorKind::Type, "Converting circular structure to JSON"));
                }
                if a.is_array(o.into())? { self.array(a, o, out)? } else { self.object(a, o, out)? }
                self.stack.leave();
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn newline(&self, out: &mut String) {
        if !self.gap.is_empty() {
            out.push('\n');
            out.push_str(&self.indent);
        }
    }

    fn reserve_rest(out: &mut String, each: usize, rest: usize) {
        if rest == 0 || each == 0 {
            return;
        }
        let step = each.saturating_add(each / 8).saturating_add(1);
        out.reserve(step.saturating_mul(rest).min(RESERVE_MAX));
    }

    fn one(&mut self, a: &mut Scope<'scope, '_>, holder: Value<'scope>, key: &str, read: Option<Value<'scope>>, out: &mut String, first: &mut bool) -> Result<(), Error> {
        let mark = out.len();
        if !*first {
            out.push(',');
        }
        self.newline(out);
        Self::quote(out, key);
        out.push(':');
        if !self.gap.is_empty() {
            out.push(' ');
        }
        if self.property(a, holder, key, read, out)? {
            *first = false;
        } else {
            out.truncate(mark);
        }
        Ok(())
    }

    fn object_allow(&mut self, a: &mut Scope<'scope, '_>, o: JsObject<'scope>, out: &mut String, first: &mut bool) -> Result<(), Error> {
        let keys = self.allow.clone().unwrap_or_default();
        for k in keys {
            let text = j8framework::SmallStr::from(k.as_str());
            self.one(a, o.into(), &text, None, out, first)?;
        }
        Ok(())
    }

    fn object_keys(&mut self, a: &mut Scope<'scope, '_>, o: JsObject<'scope>, out: &mut String, first: &mut bool) -> Result<(), Error> {
        let keys = Self::keys(o);
        let shape = o.get().props().shape_id();
        for (k, at) in keys {
            let read = at.filter(|_| shape.is_some() && o.get().props().shape_id() == shape).and_then(|i| o.get().props().read_data(i));
            self.one(a, o.into(), &k, read, out, first)?;
        }
        Ok(())
    }

    fn object_cached(&mut self, a: &mut Scope<'scope, '_>, o: JsObject<'scope>, out: &mut String, first: &mut bool) -> Result<(), Error> {
        let ptr = o.get().props().shape_id().expect("shaped object");
        if let Some(hit) = self.cached_shapes.iter().position(|c| c.ptr == ptr) {
            return self.write_cached(a, o, hit, out, first);
        }
        if self.cached_shapes.len() >= SHAPES_WRITTEN {
            return self.object_keys(a, o, out, first);
        }
        let shape = o.get().props().shape().expect("shape pointer is live");
        let n = shape.len();
        let base = self.cached_at.len();
        for i in 0..n {
            if !shape.attrs(i).enumerable() {
                self.cached_at.push(usize::MAX);
                self.cached_len.push(0);
                continue;
            }
            let start = self.quoted.len();
            Self::quote(&mut self.quoted, shape.key(i));
            self.cached_at.push(start);
            self.cached_len.push(self.quoted.len() - start);
        }
        self.cached_shapes.push(CachedShape { ptr: shape.id(), shape, base, n });
        self.write_cached(a, o, self.cached_shapes.len() - 1, out, first)
    }

    fn write_cached(&mut self, a: &mut Scope<'scope, '_>, o: JsObject<'scope>, hit: usize, out: &mut String, first: &mut bool) -> Result<(), Error> {
        let (base, n, ptr) = {
            let c = &self.cached_shapes[hit];
            (c.base, c.n, c.ptr)
        };
        for i in 0..n {
            let start = self.cached_at[base + i];
            if start == usize::MAX {
                continue;
            }
            let len = self.cached_len[base + i];
            let mark = out.len();
            if !*first {
                out.push(',');
            }
            self.newline(out);
            out.push_str(&self.quoted[start..start + len]);
            out.push(':');
            if !self.gap.is_empty() {
                out.push(' ');
            }
            let read = if o.get().props().shape_id() == Some(ptr) { o.get().props().read_data(i) } else { None };
            let wrote = match read {
                Some(v) if self.replacer.is_none() && self.to_json_absent(v) => self.write_value(a, v, out)?,
                _ => {
                    let key = self.cached_shapes[hit].shape.key(i).clone();
                    self.property(a, o.into(), &key, read, out)?
                }
            };
            if wrote {
                *first = false;
            } else {
                out.truncate(mark);
            }
        }
        Ok(())
    }

    fn object(&mut self, a: &mut Scope<'scope, '_>, o: JsObject<'scope>, out: &mut String) -> Result<(), Error> {
        let outer = if self.gap.is_empty() { String::new() } else { self.indent.clone() };
        if !self.gap.is_empty() {
            self.indent.push_str(&self.gap);
        }
        out.push('{');
        let mut first = true;
        if self.allow.is_some() {
            self.object_allow(a, o, out, &mut first)?;
        } else if self.replacer.is_none() && !o.is_array() && o.get().is_plain() && !o.get().props().is_dict() && o.get().props().shape_id().is_some() {
            self.object_cached(a, o, out, &mut first)?;
        } else {
            self.object_keys(a, o, out, &mut first)?;
        }
        if !self.gap.is_empty() {
            self.indent = outer;
        }
        if !first {
            self.newline(out);
        }
        out.push('}');
        Ok(())
    }

    fn dense_len(o: JsObject<'scope>) -> Option<u32> {
        if !o.is_array() {
            return None;
        }
        let len = o.array_length()?;
        (len as usize == o.elements().len()).then_some(len)
    }

    fn element_at(o: JsObject<'scope>, i: u32, len: u32) -> Option<Value<'scope>> {
        let n = o.elements().len();
        if o.array_length() != Some(len) || len as usize != n || i as usize >= n {
            return None;
        }
        o.elements().get(i as usize)
    }

    fn plain_array(&mut self, a: &Scope<'scope, '_>, o: JsObject<'scope>, out: &mut String) -> bool {
        if self.replacer.is_some() || !self.gap.is_empty() || !o.is_array() {
            return false;
        }
        let Some(len) = Self::dense_len(o) else {
            return false;
        };
        let n = len as usize;
        if n == 0 {
            out.push_str("[]");
            return true;
        }
        if o.elements().get(0).is_none_or(|v| !json_primitive(v)) {
            return false;
        }
        if n >= PAR_ARRAY {
            return Self::parallel_plain(a, o, out);
        }
        let before = out.len();
        out.push('[');
        for i in 0..n {
            let Some(v) = o.elements().get(i) else {
                out.truncate(before);
                return false;
            };
            if !json_primitive(v) {
                out.truncate(before);
                return false;
            }
            if i > 0 {
                out.push(',');
            }
            Self::push_plain(out, v);
        }
        out.push(']');
        true
    }

    fn parallel_plain(a: &Scope<'scope, '_>, o: JsObject<'scope>, out: &mut String) -> bool {
        #[derive(Clone, Copy)]
        enum Part<'s> {
            Number(f64),
            Text(&'s str),
            Raw(&'static str),
        }
        let items = o.elements().to_vec();
        if !items.iter().all(|v| json_primitive(*v)) {
            return false;
        }
        let parts: Vec<Part<'_>> = items
            .iter()
            .map(|v| match v.kind() {
                ValueKind::Number(x) if x.is_finite() => Part::Number(x),
                ValueKind::String(s) => Part::Text(j8framework::ScopeLocal::get(s).as_str()),
                ValueKind::Bool(true) => Part::Raw("true"),
                ValueKind::Bool(false) => Part::Raw("false"),
                _ => Part::Raw("null"),
            })
            .collect();
        let pieces = a.par().chunks(&parts, &|chunk: &[Part<'_>]| {
            let mut s = String::new();
            for (i, p) in chunk.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                match *p {
                    Part::Number(x) => push_number(&mut s, x),
                    Part::Text(t) => Writer::quote(&mut s, t),
                    Part::Raw(r) => s.push_str(r),
                }
            }
            s
        });
        let bytes = pieces.iter().map(String::len).sum::<usize>() + pieces.len() + 1;
        out.reserve(bytes);
        out.push('[');
        for (i, p) in pieces.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(p);
        }
        out.push(']');
        true
    }

    fn array(&mut self, a: &mut Scope<'scope, '_>, o: JsObject<'scope>, out: &mut String) -> Result<(), Error> {
        if self.plain_array(a, o, out) {
            return Ok(());
        }
        let outer = if self.gap.is_empty() { String::new() } else { self.indent.clone() };
        if !self.gap.is_empty() {
            self.indent.push_str(&self.gap);
        }
        let dense = Self::dense_len(o);
        let len = if let Some(n) = dense {
            n
        } else {
            let n = a.get_named(o.into(), "length")?;
            a.to_uint32(n)?
        };
        out.push('[');
        for i in 0..len {
            let before = out.len();
            if i > 0 {
                out.push(',');
            }
            self.newline(out);
            let mut buf = [0u8; 10];
            let key = fmt_index(i, &mut buf);
            let read = dense.and_then(|n| Self::element_at(o, i, n));
            if !self.property(a, o.into(), key, read, out)? {
                out.push_str("null");
            }
            if i == 0 && len > 1 {
                Self::reserve_rest(out, out.len().saturating_sub(before), (len - 1) as usize);
            }
        }
        if !self.gap.is_empty() {
            self.indent = outer;
        }
        if len > 0 {
            self.newline(out);
        }
        out.push(']');
        Ok(())
    }
}

struct Json;

impl Json {
    fn parse<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let text = a.to_string(args.at(0))?;
        let mut p = Parser { a, text: &text, pos: 0, shapes: Vec::with_capacity(SHAPES_CACHED), spare_pairs: Vec::new(), spare_items: Vec::new() };
        let v = p.value()?;
        if p.pos != text.len() {
            return Err(p.error("unexpected trailing input"));
        }
        let reviver = args.at(1);
        if !a.is_callable(reviver) {
            return Ok(v);
        }
        let root = a.object();
        root.borrow_mut(a.mc).set("", v);
        let key = a.string("");
        Self::revive(a, root.into(), key, reviver)
    }

    fn revive<'scope>(a: &mut Scope<'scope, '_>, holder: Value<'scope>, key: JsString<'scope>, reviver: Value<'scope>) -> Result<Value<'scope>, Error> {
        a.check_stack()?;
        let v = a.get_named(holder, &key)?;
        if let Some(o) = v.as_object() {
            let keys = Writer::keys(o);
            for (k, _) in keys {
                let k = a.string(&k);
                let nv = Self::revive(a, v, k, reviver)?;
                if nv == Value::Undefined {
                    a.delete(v, &k)?;
                } else {
                    a.create_data_property(o, j8framework::Key::Str(&k), nv)?;
                }
            }
        }
        a.call(reviver, holder, &[key.into(), v])
    }

    fn stringify<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (replacer, allow) = match args.at(1).kind() {
            f if a.is_callable(Value::from(f)) => (Some(Value::from(f)), None),
            ValueKind::Object(o) if a.is_array(o.into())? => {
                let items = a.to_list(Value::Object(o))?;
                let mut keys: Vec<JsString<'scope>> = Vec::new();
                for item in items {
                    let k = match item.kind() {
                        ValueKind::String(s) => Some(s),
                        ValueKind::Number(_) => Some(a.to_string(item)?),
                        _ => None,
                    };
                    if let Some(k) = k
                        && !keys.iter().any(|e| ***e == **k)
                    {
                        keys.push(k);
                    }
                }
                (None, Some(keys))
            }
            _ => (None, None),
        };
        let gap = match args.at(2).kind() {
            ValueKind::Number(n) => {
                #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "clamped to [0, MAX_GAP]")]
                let n = libm::trunc(n.clamp(0.0, 10.0)) as usize;
                " ".repeat(n.min(MAX_GAP))
            }
            ValueKind::String(s) => s.chars().take(MAX_GAP).collect(),
            _ => String::new(),
        };
        let mut w = Writer {
            gap,
            indent: String::new(),
            allow,
            replacer,
            stack: j8framework::Visiting::default(),
            clean_shapes: Vec::new(),
            clean_protos: Vec::new(),
            quoted: String::new(),
            cached_shapes: Vec::new(),
            cached_at: Vec::new(),
            cached_len: Vec::new(),
        };
        let wrapper = ObjectData { proto: Some(a.context.object_proto), ..ObjectData::default() }.alloc(a.mc);
        wrapper.borrow_mut(a.mc).set("", args.at(0));
        let key = SmallStr::from("");
        let mut out = String::new();
        if w.property(a, wrapper.into(), &key, None, &mut out)? { Ok(a.string(&out).into()) } else { Ok(Value::Undefined) }
    }
}

impl Installer<'_, '_> {
    pub(crate) fn json(&mut self) {
        let j = self.plain();
        self.method(j, "parse", Json::parse, 2);
        self.method(j, "stringify", Json::stringify, 3);
        self.set(self.context.global, "JSON", j.into());
    }
}

#[inline]
fn json_primitive(v: Value<'_>) -> bool {
    matches!(v.kind(), ValueKind::Number(_) | ValueKind::String(_) | ValueKind::Bool(_) | ValueKind::Null | ValueKind::Undefined)
}

impl Writer<'_> {
    fn push_plain(out: &mut String, v: Value<'_>) {
        match v.kind() {
            ValueKind::Number(x) if x.is_finite() => push_number(out, x),
            ValueKind::String(s) => Self::quote_string(out, j8framework::ScopeLocal::get(s)),
            ValueKind::Bool(true) => out.push_str("true"),
            ValueKind::Bool(false) => out.push_str("false"),
            _ => out.push_str("null"),
        }
    }
}

#[inline]
fn push_number(out: &mut String, x: f64) {
    #[expect(clippy::cast_possible_truncation, reason = "the comparison below confirms it is a safe integer")]
    let i = x as i64;
    #[expect(clippy::cast_precision_loss, reason = "exact within the safe integer range")]
    if x.abs() <= MAX_SAFE_INTEGER && i as f64 == x {
        push_int(out, i);
        return;
    }
    let mut buf = ryu_js::Buffer::new();
    out.push_str(buf.format(x));
}

fn push_int(out: &mut String, i: i64) {
    let mut buf = [0u8; 20];
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
    out.push_str(core::str::from_utf8(&buf[at..]).expect("digits are ascii"));
}

fn fmt_index(i: u32, buf: &mut [u8; 10]) -> &str {
    let mut n = i;
    let mut at = buf.len();
    loop {
        at -= 1;
        let digit = (n % 10) as u8;
        buf[at] = b'0' + digit;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    core::str::from_utf8(&buf[at..]).expect("digits are ascii")
}

fn int_f64(digits: &[u8], neg: bool) -> f64 {
    let mut n = 0u64;
    for &d in digits {
        n = n * 10 + (u64::from(d) - u64::from(b'0'));
    }
    #[expect(clippy::cast_precision_loss, reason = "at most 15 digits, exact in f64")]
    let x = n as f64;
    if neg { -x } else { x }
}

fn quote_from(out: &mut String, s: &str, mut i: usize) {
    let bytes = s.as_bytes();
    out.reserve(s.len().saturating_mul(2).saturating_add(2));
    out.push('"');
    let mut from = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if ESCAPE[usize::from(b)] == 0 {
            i += 1;
            continue;
        }
        out.push_str(&s[from..i]);
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            0x08 => out.push_str("\\b"),
            0x0c => out.push_str("\\f"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            _ => push_u00(out, b),
        }
        i += 1;
        from = i;
    }
    out.push_str(&s[from..]);
    out.push('"');
}

fn quote_units(out: &mut String, units: &[u16]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push('"');
    for c in char::decode_utf16(units.iter().copied()) {
        match c {
            Ok('"') => out.push_str("\\\""),
            Ok('\\') => out.push_str("\\\\"),
            Ok('\u{8}') => out.push_str("\\b"),
            Ok('\u{c}') => out.push_str("\\f"),
            Ok('\n') => out.push_str("\\n"),
            Ok('\r') => out.push_str("\\r"),
            Ok('\t') => out.push_str("\\t"),
            Ok(c) if u32::from(c) < 0x20 => push_u00(out, u8::try_from(c).unwrap_or_default()),
            Ok(c) => out.push(c),
            Err(e) => {
                let u = e.unpaired_surrogate();
                out.push_str("\\u");
                for shift in [12, 8, 4, 0] {
                    out.push(char::from(HEX[usize::from(u >> shift & 0xf)]));
                }
            }
        }
    }
    out.push('"');
}

fn push_u00(out: &mut String, b: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push_str("\\u00");
    out.push(char::from(HEX[usize::from(b >> 4)]));
    out.push(char::from(HEX[usize::from(b & 0x0f)]));
}
