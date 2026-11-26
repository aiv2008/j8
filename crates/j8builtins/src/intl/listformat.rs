use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use icu_list::options::{ListFormatterOptions, ListLength};
use icu_list::{ListFormatter, ListFormatterPreferences};
use icu_locale_core::Locale;
use j8framework::{Error, ErrorKind, JsObject, Scope, Value, ValueKind};

use super::locale::{ExtKey, LocaleData, Options, Tags, choices};
use super::service::{Hosted, Pieces, Service};
use crate::Args;

choices! {
    ListType { Conjunction = "conjunction", Disjunction = "disjunction", Unit = "unit" }
    ListStyle { Long = "long", Short = "short", Narrow = "narrow" }
}

const ELEMENT: &str = "element";

impl From<ListStyle> for ListLength {
    fn from(s: ListStyle) -> Self {
        match s {
            ListStyle::Long => Self::Wide,
            ListStyle::Short => Self::Short,
            ListStyle::Narrow => Self::Narrow,
        }
    }
}

struct ListData;

impl LocaleData for ListData {
    const KEYS: &'static [ExtKey] = &[];

    fn default_value(_: ExtKey, _: &Locale) -> Option<String> {
        None
    }

    fn supports(_: ExtKey, _: &str, _: &Locale) -> bool {
        false
    }
}

pub(crate) struct Lists {
    formatter: ListFormatter,
    locale: String,
    kind: ListType,
    style: ListStyle,
}

impl Lists {
    pub(crate) fn new<'scope>(a: &mut Scope<'scope, '_>, locales: Value<'scope>, options: Value<'scope>) -> Result<Self, Error> {
        let requested = Tags::list(a, locales)?;
        let options = Options::object(a, options)?;
        options.matcher(a)?;
        let kind = options.choice(a, "type")?.unwrap_or(ListType::Conjunction);
        let style = options.choice(a, "style")?.unwrap_or(ListStyle::Long);
        let resolved = Tags::resolve::<ListData>(a, &requested, &[]);
        let locale = Locale::from(resolved.locale.id.clone());
        let prefs = ListFormatterPreferences::from(&locale);
        let opts = ListFormatterOptions::default().with_length(style.into());
        let formatter = match kind {
            ListType::Conjunction => ListFormatter::try_new_and(prefs, opts),
            ListType::Disjunction => ListFormatter::try_new_or(prefs, opts),
            ListType::Unit => ListFormatter::try_new_unit(prefs, opts),
        }
        .map_err(|_| a.throw(ErrorKind::Range, "Unsupported list format options"))?;
        Ok(Self { formatter, locale: resolved.tag(), kind, style })
    }

    pub(crate) fn pieces(&self, items: &[String]) -> Pieces {
        Pieces::marked(&self.formatter.format(items.iter().map(String::as_str)), icu_list::parts::ELEMENT, ELEMENT)
    }
}

pub(crate) struct ListFormats;

impl ListFormats {
    pub(crate) fn construct<'scope>(a: &mut Scope<'scope, '_>, ctor: JsObject<'scope>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        if matches!(a.new_target.kind(), ValueKind::Undefined) {
            return Err(a.throw(ErrorKind::Type, "Constructor Intl.ListFormat requires 'new'"));
        }
        let proto = Hosted::proto(a, ctor, this);
        let l = Lists::new(a, args.at(0), args.at(1))?;
        Ok(Hosted::alloc(a, Service::List(Box::new(l)), proto).into())
    }

    fn with<'scope, R>(a: &Scope<'scope, '_>, this: Value<'scope>, method: &str, f: impl FnOnce(&Lists) -> R) -> Result<R, Error> {
        Hosted::with(a, this, |h| match &h.service {
            Service::List(l) => Some(f(l)),
            _ => None,
        })
        .ok_or_else(|| Hosted::incompatible(a, method))
    }

    fn strings<'scope>(a: &mut Scope<'scope, '_>, list: Value<'scope>) -> Result<Vec<String>, Error> {
        let mut out = Vec::new();
        if matches!(list.kind(), ValueKind::Undefined) {
            return Ok(out);
        }
        let it = a.get_iterator(list)?;
        while let Some(v) = a.iterator_step(it)? {
            if !matches!(v.kind(), ValueKind::String(_)) {
                let e = a.throw(ErrorKind::Type, "Iterable yielded a value that is not a string");
                return Err(a.iterator_close_abrupt(it.iterator, e));
            }
            out.push(String::from(&**a.to_string(v)?));
        }
        Ok(out)
    }

    pub(crate) fn format<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::with(a, this, "Intl.ListFormat.prototype.format", |_| ())?;
        let items = Self::strings(a, args.at(0))?;
        let text = Self::with(a, this, "format", |l| l.formatter.format_to_string(items.iter().map(String::as_str)))?;
        Ok(a.string(&text).into())
    }

    pub(crate) fn format_to_parts<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::with(a, this, "Intl.ListFormat.prototype.formatToParts", |_| ())?;
        let items = Self::strings(a, args.at(0))?;
        let pieces = Self::with(a, this, "formatToParts", |l| l.pieces(&items))?;
        pieces.array(a)
    }

    pub(crate) fn resolved_options<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (locale, kind, style) = Self::with(a, this, "Intl.ListFormat.prototype.resolvedOptions", |l| (l.locale.clone(), l.kind, l.style))?;
        let o = a.object();
        let fields: [(&str, Value<'scope>); 3] = [("locale", a.string(&locale).into()), ("type", a.string(kind.name()).into()), ("style", a.string(style.name()).into())];
        for (k, v) in fields {
            Tags::put(a, o, k, v)?;
        }
        Ok(o.into())
    }

    pub(crate) fn supported_locales_of<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Tags::supported(a, args.at(0), args.at(1))
    }
}
