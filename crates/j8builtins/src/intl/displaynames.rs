use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use icu_experimental::displaynames::multi::{LanguageDisplayNames, LocaleDisplayNamesFormatter, RegionDisplayNames, ScriptDisplayNames};
use icu_experimental::displaynames::{DisplayNamesOptions, DisplayNamesPreferences, Fallback as IcuFallback, LanguageDisplay as IcuLanguageDisplay, Style as IcuStyle};
use icu_locale_core::Locale;
use icu_locale_core::subtags::{Region, Script};
use j8framework::{Error, ErrorKind, JsObject, Scope, Value, ValueKind};

use super::collator::Collation;
use super::ilocale::Subtags;
use super::locale::{ExtKey, LocaleData, Options, Tags, choices};
use super::service::{Hosted, Service};
use crate::Args;

choices! {
    NameStyle { Narrow = "narrow", Short = "short", Long = "long" }
    NameType { Language = "language", Region = "region", Script = "script", Currency = "currency", Calendar = "calendar", DateTimeField = "dateTimeField" }
    NameFallback { Code = "code", None = "none" }
    LanguageDisplay { Dialect = "dialect", Standard = "standard" }
    DateTimeField {
        Era = "era", Year = "year", Quarter = "quarter", Month = "month", WeekOfYear = "weekOfYear", Weekday = "weekday",
        Day = "day", DayPeriod = "dayPeriod", Hour = "hour", Minute = "minute", Second = "second", TimeZoneName = "timeZoneName",
    }
}

const CURRENCY_LETTERS: usize = 3;

struct NamesData;

impl LocaleData for NamesData {
    const KEYS: &'static [ExtKey] = &[];

    fn default_value(_: ExtKey, _: &Locale) -> Option<String> {
        None
    }

    fn supports(_: ExtKey, _: &str, _: &Locale) -> bool {
        false
    }
}

pub(crate) struct DisplayNames {
    locale: String,
    style: NameStyle,
    kind: NameType,
    fallback: NameFallback,
    language_display: Option<LanguageDisplay>,
    prefs: DisplayNamesPreferences,
}

impl DisplayNames {
    fn new<'scope>(a: &mut Scope<'scope, '_>, locales: Value<'scope>, options: Value<'scope>) -> Result<Self, Error> {
        let requested = Tags::list(a, locales)?;
        if matches!(options.kind(), ValueKind::Undefined) {
            return Err(a.throw(ErrorKind::Type, "Intl.DisplayNames requires an options object with a type"));
        }
        let options = Options::object(a, options)?;
        options.matcher(a)?;
        let resolved = Tags::resolve::<NamesData>(a, &requested, &[]);
        let style = options.choice(a, "style")?.unwrap_or(NameStyle::Long);
        let kind: NameType = options.choice(a, "type")?.ok_or_else(|| a.throw(ErrorKind::Type, "Intl.DisplayNames requires a type"))?;
        let fallback = options.choice(a, "fallback")?.unwrap_or(NameFallback::Code);
        let language_display = options.choice(a, "languageDisplay")?.unwrap_or(LanguageDisplay::Dialect);
        let prefs = DisplayNamesPreferences::from(&Locale::from(resolved.locale.id.clone()));
        Ok(Self { locale: resolved.tag(), style, kind, fallback, language_display: (kind == NameType::Language).then_some(language_display), prefs })
    }

    fn options(&self) -> DisplayNamesOptions {
        let mut o = DisplayNamesOptions::default();
        o.style = Some(match self.style {
            NameStyle::Narrow => IcuStyle::Narrow,
            NameStyle::Short => IcuStyle::Short,
            NameStyle::Long => IcuStyle::Long,
        });
        o.fallback = match self.fallback {
            NameFallback::Code => IcuFallback::Code,
            NameFallback::None => IcuFallback::None,
        };
        o.language_display = match self.language_display {
            Some(LanguageDisplay::Standard) => IcuLanguageDisplay::Standard,
            _ => IcuLanguageDisplay::Dialect,
        };
        o
    }

    fn language_id(code: &str) -> Option<String> {
        let parts: Vec<&str> = code.split('-').collect();
        let (first, rest) = parts.split_first()?;
        if !Subtags::language(first) {
            return None;
        }
        let mut i = 0;
        if rest.get(i).is_some_and(|s| Subtags::script(s)) {
            i += 1;
        }
        if rest.get(i).is_some_and(|s| Subtags::region(s)) {
            i += 1;
        }
        let variants = &rest[i..];
        if !variants.is_empty() && Subtags::variants(&variants.join("-")).is_none() {
            return None;
        }
        Some(Tags::parse(code).map_or_else(|| code.to_ascii_lowercase(), |l| l.id.to_string()))
    }

    fn canonical(&self, code: &str) -> Option<String> {
        match self.kind {
            NameType::Language => Self::language_id(code),
            NameType::Region => Subtags::region(code).then(|| code.to_ascii_uppercase()),
            NameType::Script => Subtags::script(code).then(|| {
                let lower = code.to_ascii_lowercase();
                let mut out = String::from(&lower[..1]).to_ascii_uppercase();
                out.push_str(&lower[1..]);
                out
            }),
            NameType::Currency => (code.len() == CURRENCY_LETTERS && code.bytes().all(|b| b.is_ascii_alphabetic())).then(|| code.to_ascii_uppercase()),
            NameType::Calendar => (Collation::valid_type(code) && !code.contains('_')).then(|| code.to_ascii_lowercase()),
            NameType::DateTimeField => DateTimeField::try_from(code).ok().map(|f| String::from(f.name())),
        }
    }

    fn name(&self, code: &str) -> Option<String> {
        let opts = self.options();
        match self.kind {
            NameType::Language => {
                let locale = Locale::try_from_str(code).ok()?;
                let known = LanguageDisplayNames::try_new(self.prefs, opts).ok()?.of(locale.id.language).is_some();
                if !known {
                    return None;
                }
                Some(String::from(LocaleDisplayNamesFormatter::try_new(self.prefs, opts).ok()?.of(&locale).as_ref()))
            }
            NameType::Region => RegionDisplayNames::try_new(self.prefs, opts).ok()?.of(Region::try_from_str(code).ok()?).map(String::from),
            NameType::Script => ScriptDisplayNames::try_new(self.prefs, opts).ok()?.of(Script::try_from_str(code).ok()?).map(String::from),
            NameType::Currency | NameType::Calendar | NameType::DateTimeField => None,
        }
    }
}

pub(crate) struct DisplayNamesBuiltins;

impl DisplayNamesBuiltins {
    pub(crate) fn construct<'scope>(a: &mut Scope<'scope, '_>, ctor: JsObject<'scope>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        if matches!(a.new_target.kind(), ValueKind::Undefined) {
            return Err(a.throw(ErrorKind::Type, "Constructor Intl.DisplayNames requires 'new'"));
        }
        let proto = Hosted::proto(a, ctor, this);
        let d = DisplayNames::new(a, args.at(0), args.at(1))?;
        Ok(Hosted::alloc(a, Service::DisplayNames(Box::new(d)), proto).into())
    }

    fn with<'scope, R>(a: &Scope<'scope, '_>, this: Value<'scope>, method: &str, f: impl FnOnce(&DisplayNames) -> R) -> Result<R, Error> {
        Hosted::with(a, this, |h| match &h.service {
            Service::DisplayNames(d) => Some(f(d)),
            _ => None,
        })
        .ok_or_else(|| Hosted::incompatible(a, method))
    }

    pub(crate) fn of<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::with(a, this, "Intl.DisplayNames.prototype.of", |_| ())?;
        let code = String::from(a.to_string(args.at(0))?.as_str());
        let canonical = Self::with(a, this, "of", |d| d.canonical(&code))?.ok_or_else(|| a.throw(ErrorKind::Range, &alloc::format!("invalid_argument: {code}")))?;
        let (name, fallback) = Self::with(a, this, "of", |d| (d.name(&canonical), d.fallback))?;
        Ok(match (name, fallback) {
            (Some(n), _) => a.string(&n).into(),
            (None, NameFallback::Code) => a.string(&canonical).into(),
            (None, NameFallback::None) => Value::Undefined,
        })
    }

    pub(crate) fn resolved_options<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (locale, style, kind, fallback, language_display) = Self::with(a, this, "Intl.DisplayNames.prototype.resolvedOptions", |d| (d.locale.clone(), d.style, d.kind, d.fallback, d.language_display))?;
        let o = a.object();
        let mut fields: Vec<(&str, Value<'scope>)> = alloc::vec![
            ("locale", a.string(&locale).into()),
            ("style", a.string(style.name()).into()),
            ("type", a.string(kind.name()).into()),
            ("fallback", a.string(fallback.name()).into()),
        ];
        if let Some(l) = language_display {
            fields.push(("languageDisplay", a.string(l.name()).into()));
        }
        for (k, v) in fields {
            Tags::put(a, o, k, v)?;
        }
        Ok(o.into())
    }

    pub(crate) fn supported_locales_of<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Tags::supported(a, args.at(0), args.at(1))
    }
}
