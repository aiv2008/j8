use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::string::String;
use alloc::vec::Vec;

use icu_locale_core::Locale;
use icu_segmenter::options::{SentenceBreakInvariantOptions, WordBreakInvariantOptions};
use icu_segmenter::{GraphemeClusterSegmenter, SentenceSegmenter, WordSegmenter};
use j8framework::{Error, ErrorKind, JsObject, Scope, Value, ValueKind};

use super::locale::{ExtKey, LocaleData, Options, Tags, choices};
use super::service::{Hosted, Service};
use crate::Args;

choices! {
    Granularity { Grapheme = "grapheme", Word = "word", Sentence = "sentence" }
}

pub(crate) const SEGMENTS_PROTO_SLOT: usize = 0;
pub(crate) const ITERATOR_PROTO_SLOT: usize = 1;

struct SegmenterData;

impl LocaleData for SegmenterData {
    const KEYS: &'static [ExtKey] = &[];

    fn default_value(_: ExtKey, _: &Locale) -> Option<String> {
        None
    }

    fn supports(_: ExtKey, _: &str, _: &Locale) -> bool {
        false
    }
}

pub(crate) struct Segmenter {
    locale: String,
    granularity: Granularity,
}

pub(crate) struct Segmented {
    units: Vec<u16>,
    bounds: Vec<usize>,
    word_like: Option<Vec<bool>>,
}

pub(crate) struct SegmentCursor {
    data: Arc<Segmented>,
    at: usize,
}

impl Segmenter {
    fn new<'scope>(a: &mut Scope<'scope, '_>, locales: Value<'scope>, options: Value<'scope>) -> Result<Self, Error> {
        let requested = Tags::list(a, locales)?;
        let options = Options::object(a, options)?;
        options.matcher(a)?;
        let granularity = options.choice(a, "granularity")?.unwrap_or(Granularity::Grapheme);
        let resolved = Tags::resolve::<SegmenterData>(a, &requested, &[]);
        Ok(Self { locale: resolved.tag(), granularity })
    }

    fn split(&self, units: Vec<u16>) -> Segmented {
        let (mut bounds, word_like): (Vec<usize>, Option<Vec<bool>>) = match self.granularity {
            Granularity::Grapheme => (GraphemeClusterSegmenter::new().segment_utf16(&units).collect(), None),
            Granularity::Sentence => (SentenceSegmenter::new(SentenceBreakInvariantOptions::default()).segment_utf16(&units).collect(), None),
            Granularity::Word => {
                let mut it = WordSegmenter::new_auto(WordBreakInvariantOptions::default()).segment_utf16(&units);
                let mut bounds = Vec::new();
                let mut flags = Vec::new();
                while let Some(b) = it.next() {
                    if !bounds.is_empty() {
                        flags.push(it.is_word_like());
                    }
                    bounds.push(b);
                }
                (bounds, Some(flags))
            }
        };
        if bounds.first() != Some(&0) {
            bounds.insert(0, 0);
        }
        if bounds.last() != Some(&units.len()) {
            bounds.push(units.len());
        }
        Segmented { units, bounds, word_like }
    }
}

impl Segmented {
    fn containing(&self, n: usize) -> Option<usize> {
        if n >= self.units.len() {
            return None;
        }
        Some(self.bounds.partition_point(|&b| b <= n) - 1)
    }

    fn data<'scope>(&self, a: &mut Scope<'scope, '_>, i: usize) -> Result<Value<'scope>, Error> {
        let (start, end) = (self.bounds[i], self.bounds[i + 1]);
        let o = a.object();
        let segment = a.string_units(&self.units[start..end]).into();
        Tags::put(a, o, "segment", segment)?;
        #[expect(clippy::cast_precision_loss, reason = "string offsets are far below 2^53")]
        Tags::put(a, o, "index", Value::from(start as f64))?;
        let input = a.string_units(&self.units).into();
        Tags::put(a, o, "input", input)?;
        if let Some(flags) = &self.word_like {
            Tags::put(a, o, "isWordLike", Value::from(flags.get(i).copied().unwrap_or(false)))?;
        }
        Ok(o.into())
    }
}

pub(crate) struct Segmenters;

impl Segmenters {
    pub(crate) fn construct<'scope>(a: &mut Scope<'scope, '_>, ctor: JsObject<'scope>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        if matches!(a.new_target.kind(), ValueKind::Undefined) {
            return Err(a.throw(ErrorKind::Type, "Constructor Intl.Segmenter requires 'new'"));
        }
        let proto = Hosted::proto(a, ctor, this);
        let s = Segmenter::new(a, args.at(0), args.at(1))?;
        Ok(Hosted::alloc(a, Service::Segmenter(Box::new(s)), proto).into())
    }

    pub(crate) fn segment<'scope>(a: &mut Scope<'scope, '_>, f: JsObject<'scope>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Hosted::with(a, this, |h| matches!(h.service, Service::Segmenter(_)).then_some(())).ok_or_else(|| Hosted::incompatible(a, "Intl.Segmenter.prototype.segment"))?;
        let units: Vec<u16> = a.to_string(args.at(0))?.encode_utf16().collect();
        let data = Hosted::with(a, this, |h| match &h.service {
            Service::Segmenter(s) => Some(s.split(units)),
            _ => None,
        })
        .ok_or_else(|| Hosted::incompatible(a, "segment"))?;
        let proto = Hosted::slot(f, SEGMENTS_PROTO_SLOT).and_then(Value::as_object).unwrap_or(a.context.object_proto);
        let iter_proto = Hosted::slot(f, ITERATOR_PROTO_SLOT).unwrap_or(Value::Undefined);
        let o = Hosted::alloc(a, Service::Segments(Arc::new(data)), proto);
        Hosted::with(a, o.into(), |h| {
            h.bound = iter_proto.as_object();
            Some(())
        });
        Ok(o.into())
    }

    fn segments<'scope>(a: &Scope<'scope, '_>, this: Value<'scope>, method: &str) -> Result<(Arc<Segmented>, Option<JsObject<'scope>>), Error> {
        Hosted::with(a, this, |h| match &h.service {
            Service::Segments(d) => Some((Arc::clone(d), h.bound)),
            _ => None,
        })
        .ok_or_else(|| Hosted::incompatible(a, method))
    }

    pub(crate) fn containing<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (data, _) = Self::segments(a, this, "%Segments.prototype%.containing")?;
        let n = a.to_integer_or_infinity(args.at(0))?;
        if n < 0.0 {
            return Ok(Value::Undefined);
        }
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "non-negative; saturates for infinity")]
        let n = n as usize;
        match data.containing(n) {
            Some(i) => data.data(a, i),
            None => Ok(Value::Undefined),
        }
    }

    pub(crate) fn iterator<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (data, proto) = Self::segments(a, this, "%Segments.prototype%[@@iterator]")?;
        let proto = proto.unwrap_or(a.context.iterator_proto);
        Ok(Hosted::alloc(a, Service::SegmentIterator(Box::new(SegmentCursor { data, at: 0 })), proto).into())
    }

    pub(crate) fn next<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let step = Hosted::with(a, this, |h| match &mut h.service {
            Service::SegmentIterator(c) => {
                let i = c.at;
                if i + 1 < c.data.bounds.len() {
                    c.at += 1;
                    Some(Some((Arc::clone(&c.data), i)))
                } else {
                    Some(None)
                }
            }
            _ => None,
        })
        .ok_or_else(|| Hosted::incompatible(a, "%SegmentIterator.prototype%.next"))?;
        Ok(match step {
            Some((data, i)) => {
                let v = data.data(a, i)?;
                a.iter_result(v, false).into()
            }
            None => a.iter_result(Value::Undefined, true).into(),
        })
    }

    pub(crate) fn resolved_options<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (locale, granularity) = Hosted::with(a, this, |h| match &h.service {
            Service::Segmenter(s) => Some((s.locale.clone(), s.granularity)),
            _ => None,
        })
        .ok_or_else(|| Hosted::incompatible(a, "Intl.Segmenter.prototype.resolvedOptions"))?;
        let o = a.object();
        let fields: [(&str, Value<'scope>); 2] = [("locale", a.string(&locale).into()), ("granularity", a.string(granularity.name()).into())];
        for (k, v) in fields {
            Tags::put(a, o, k, v)?;
        }
        Ok(o.into())
    }

    pub(crate) fn supported_locales_of<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Tags::supported(a, args.at(0), args.at(1))
    }
}
