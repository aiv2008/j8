use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::any::TypeId;
use core::ptr::NonNull;

use writeable::{Part, PartsWrite, Writeable};
use j8framework::{ClosureFn, Error, ErrorKind, GcCell, Host, HostObject, JsObject, NativeClosure, ObjectData, ObjectKind, Scope, ScopeLocal, Trace, Tracer, Value, ValueKind};

use super::collator::Collation;
use super::locale::Tags;
use super::datetime::DateTimeFormat;
use super::number::NumberFormat;
use super::plural::Plurals;
use super::listformat::Lists;
use super::durationformat::Durations;
use super::displaynames::DisplayNames;
use super::relativetime::RelativeTimes;
use super::segmenter::{SegmentCursor, Segmented, Segmenter};

pub(crate) enum Service<'scope> {
    Collator(Box<Collation<'scope>>),
    Number(Box<NumberFormat>),
    DateTime(Box<DateTimeFormat>),
    Plural(Box<Plurals>),
    List(Box<Lists>),
    Duration(Box<Durations>),
    DisplayNames(Box<DisplayNames>),
    RelativeTime(Box<RelativeTimes>),
    Segmenter(Box<Segmenter>),
    Segments(alloc::sync::Arc<Segmented>),
    SegmentIterator(Box<SegmentCursor>),
    Locale(Box<icu_locale_core::Locale>),
}

struct Brand;

pub(crate) struct Hosted<'scope> {
    pub(crate) service: Service<'scope>,
    pub(crate) bound: Option<JsObject<'scope>>,
}

unsafe impl<'scope> HostObject<'scope> for Hosted<'scope> {
    fn trace(&self, t: &mut Tracer<'_>) {
        self.bound.trace(t);
    }

    fn relocate(&mut self, t: &mut Tracer<'_>) {
        self.bound.relocate(t);
    }

    fn find(&self, brand: TypeId) -> Option<NonNull<()>> {
        (brand == TypeId::of::<Brand>()).then(|| NonNull::from(self).cast())
    }

    fn find_mut(&mut self, brand: TypeId) -> Option<NonNull<()>> {
        (brand == TypeId::of::<Brand>()).then(|| NonNull::from(self).cast())
    }
}

pub(crate) const PROTO_SLOT: usize = 0;

pub(crate) const TARGET_SLOT: usize = 0;

impl<'scope> Hosted<'scope> {
    pub(crate) fn alloc(a: &Scope<'scope, '_>, service: Service<'scope>, proto: JsObject<'scope>) -> JsObject<'scope> {
        let data = a.mc.alloc(GcCell::new(a.mc, Host::new(Self { service, bound: None })));
        ObjectData { kind: ObjectKind::Host(data), proto: Some(proto), ..ObjectData::default() }.alloc(a.mc)
    }

    pub(crate) fn with<R>(a: &Scope<'scope, '_>, this: Value<'scope>, f: impl FnOnce(&mut Self) -> Option<R>) -> Option<R> {
        let ValueKind::Object(o) = this.kind() else { return None };
        let ObjectKind::Host(h) = o.borrow().kind else { return None };
        let mut cell = h.try_borrow_mut(a.mc)?;
        let p = cell.find_mut(TypeId::of::<Brand>())?;
        f(unsafe { p.cast::<Self>().as_mut() })
    }

    pub(crate) fn proto(a: &Scope<'scope, '_>, ctor: JsObject<'scope>, this: Value<'scope>) -> JsObject<'scope> {
        let intrinsic = Self::slot(ctor, PROTO_SLOT).and_then(Value::as_object).unwrap_or(a.context.object_proto);
        let given = this.as_object().and_then(|o| o.borrow().proto());
        let explicit = a.new_target.as_object().and_then(|t| t.borrow().own(a.mc, "prototype")).and_then(Value::as_object);
        match given {
            Some(p) if a.new_target.as_object().is_some() && (!ScopeLocal::ptr_eq(p, a.context.object_proto) || explicit.is_some()) => p,
            _ => intrinsic,
        }
    }

    pub(crate) fn slot(f: JsObject<'scope>, i: usize) -> Option<Value<'scope>> {
        match &f.borrow().kind {
            ObjectKind::NativeClosure(c) => c.slots.get(i).copied(),
            _ => None,
        }
    }

    pub(crate) fn closure(a: &Scope<'scope, '_>, f: ClosureFn, slots: alloc::vec::Vec<Value<'scope>>, name: &str, length: u32, constructor: bool) -> JsObject<'scope> {
        let o = ObjectData { kind: ObjectKind::NativeClosure(NativeClosure { f, slots, constructor }), proto: Some(a.context.function_proto), ..ObjectData::default() }.alloc(a.mc);
        a.context.name_function(a.mc, o, name, Some(length));
        o
    }

    pub(crate) fn bound(a: &Scope<'scope, '_>, this: Value<'scope>, f: ClosureFn, length: u32) -> Option<JsObject<'scope>> {
        if let Some(b) = Self::with(a, this, |h| Some(h.bound)).flatten() {
            return Some(b);
        }
        let made = Self::closure(a, f, alloc::vec![this], "", length, false);
        Self::with(a, this, |h| {
            h.bound = Some(made);
            Some(made)
        })
    }

    pub(crate) fn incompatible(a: &Scope<'scope, '_>, method: &str) -> Error {
        a.throw(ErrorKind::Type, &alloc::format!("Method {method} called on incompatible receiver"))
    }
}

pub(crate) const LITERAL: &str = "literal";

struct Span {
    start: usize,
    end: usize,
    part: Part,
    depth: usize,
}

#[derive(Default)]
pub(crate) struct Collector {
    text: String,
    spans: Vec<Span>,
    depth: usize,
}

impl fmt::Write for Collector {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.text.push_str(s);
        Ok(())
    }
}

impl PartsWrite for Collector {
    type SubPartsWrite = Self;

    fn with_part(&mut self, part: Part, mut f: impl FnMut(&mut Self) -> fmt::Result) -> fmt::Result {
        let start = self.text.len();
        self.depth += 1;
        let r = f(self);
        self.depth -= 1;
        self.spans.push(Span { start, end: self.text.len(), part, depth: self.depth });
        r
    }
}

#[derive(Default)]
pub(crate) struct Pieces(pub(crate) Vec<(&'static str, String)>);

impl Pieces {
    pub(crate) fn push(&mut self, kind: &'static str, text: &str) {
        if text.is_empty() {
            return;
        }
        match self.0.last_mut() {
            Some((k, t)) if *k == kind && kind == LITERAL => t.push_str(text),
            _ => self.0.push((kind, String::from(text))),
        }
    }

    pub(crate) fn text(&self) -> String {
        self.0.iter().map(|(_, t)| t.as_str()).collect()
    }

    pub(crate) fn array<'scope>(&self, a: &mut Scope<'scope, '_>) -> Result<Value<'scope>, Error> {
        self.array_with(a, None)
    }

    pub(crate) fn array_with<'scope>(&self, a: &mut Scope<'scope, '_>, extra: Option<(&str, &str)>) -> Result<Value<'scope>, Error> {
        let mut items = Vec::with_capacity(self.0.len());
        for (kind, text) in &self.0 {
            let o = a.object();
            let kind_value = a.string(kind).into();
            Tags::put(a, o, "type", kind_value)?;
            let text = a.string(text).into();
            Tags::put(a, o, "value", text)?;
            if let Some((k, v)) = extra.filter(|_| *kind != LITERAL) {
                let v = a.string(v).into();
                Tags::put(a, o, k, v)?;
            }
            items.push(Value::from(o));
        }
        Ok(a.array_from_slice(&items).into())
    }

    pub(crate) fn marked(w: &impl Writeable, part: Part, kind: &'static str) -> Self {
        let mut c = Collector::default();
        let _ = w.write_to_parts(&mut c);
        let mut marks: Vec<(usize, usize)> = c.spans.iter().filter(|s| s.part == part).map(|s| (s.start, s.end)).collect();
        marks.sort_unstable();
        let mut out = Self::default();
        let mut at = 0;
        for (start, end) in marks {
            out.push(LITERAL, &c.text[at..start]);
            out.0.push((kind, String::from(&c.text[start..end])));
            at = end;
        }
        out.push(LITERAL, &c.text[at..]);
        out
    }

    pub(crate) fn collect(w: &impl Writeable, pick: impl Fn(&[&Part]) -> &'static str) -> Self {
        let mut c = Collector::default();
        let _ = w.write_to_parts(&mut c);
        let mut cover: Vec<Vec<(usize, &Part)>> = (0..c.text.len()).map(|_| Vec::new()).collect();
        for s in &c.spans {
            for slot in &mut cover[s.start..s.end] {
                slot.push((s.depth, &s.part));
            }
        }
        let mut out = Self::default();
        let mut start = 0;
        let mut current: Option<&'static str> = None;
        for (i, slot) in cover.iter_mut().enumerate() {
            if !c.text.is_char_boundary(i) {
                continue;
            }
            slot.sort_by_key(|&(d, _)| d);
            let parts: Vec<&Part> = slot.iter().map(|&(_, p)| p).collect();
            let kind = pick(&parts);
            if current != Some(kind) {
                if let Some(k) = current {
                    out.0.push((k, String::from(&c.text[start..i])));
                }
                start = i;
                current = Some(kind);
            }
        }
        if let Some(k) = current {
            out.0.push((k, String::from(&c.text[start..])));
        }
        out
    }
}
