use alloc::boxed::Box;
use alloc::string::String;
use core::cmp::Ordering;

use icu_casemap::CaseMapper;
use icu_collator::options::{AlternateHandling, CaseLevel, CollatorOptions, Strength};
use icu_collator::{CollatorBorrowed, CollatorPreferences};
use icu_locale_core::{LanguageIdentifier, Locale};
use icu_locale_core::extensions::unicode::Value as Keyword;
use icu_locale_core::preferences::extensions::unicode::keywords::{CollationCaseFirst, CollationNumericOrdering, CollationType};
use icu_provider::prelude::*;
use j8framework::{Error, ErrorKind, JsObject, Scope, Value};

use super::locale::{ExtKey, LocaleData, Options, Tags, choices};

use super::service::{Hosted, Service, TARGET_SLOT};
use crate::Args;

choices! {
    Usage { Sort = "sort", Search = "search" }
    Sensitivity { Base = "base", Accent = "accent", Case = "case", Variant = "variant" }
    CaseFirst { Upper = "upper", Lower = "lower", False = "false" }
}

const KEY_COLLATION: usize = 0;
const KEY_CASE_FIRST: usize = 1;
const KEY_NUMERIC: usize = 2;

const DEFAULT_COLLATION: &str = "default";

const COLLATION_TYPES: [&str; 12] = ["compat", "dict", "emoji", "eor", "phonebk", "phonetic", "pinyin", "searchjl", "stroke", "trad", "unihan", "zhuyin"];

pub(crate) struct Collation<'d> {
    collator: CollatorBorrowed<'d>,
    settings: Settings,
}

#[derive(Clone)]
struct Settings {
    locale: String,
    usage: Usage,
    sensitivity: Sensitivity,
    ignore_punctuation: bool,
    collation: String,
    numeric: bool,
    case_first: CaseFirst,
}

struct CollatorData;

impl LocaleData for CollatorData {
    const KEYS: &'static [ExtKey] = &[ExtKey::Collation, ExtKey::CaseFirst, ExtKey::Numeric];

    fn default_value(key: ExtKey, _: &Locale) -> Option<String> {
        match key {
            ExtKey::CaseFirst | ExtKey::Numeric => Some(String::from("false")),
            _ => None,
        }
    }

    fn supports(key: ExtKey, value: &str, locale: &Locale) -> bool {
        match key {
            ExtKey::Collation => Collation::has_type(value, locale),
            ExtKey::CaseFirst => CaseFirst::try_from(value).is_ok(),
            ExtKey::Numeric => matches!(value, "true" | "false"),
            _ => false,
        }
    }
}

impl Collation<'_> {
    pub(crate) fn has_type(value: &str, locale: &Locale) -> bool {
        if matches!(value, "standard" | "search") || CollationType::try_from(&Keyword::try_from_str(value).unwrap_or_default()).is_err() {
            return false;
        }
        let Ok(attributes) = DataMarkerAttributes::try_from_str(value) else { return false };
        let data_locale = DataLocale::from(&locale.id);
        let mut metadata = DataRequestMetadata::default();
        metadata.silent = true;
        let req = DataRequest { id: DataIdentifierBorrowed::for_marker_attributes_and_locale(attributes, &data_locale), metadata };
        DataProvider::<icu_collator::provider::CollationMetadataV1>::load(&icu_collator::provider::Baked, req).is_ok()
    }

    pub(crate) fn types() -> &'static [&'static str] {
        &COLLATION_TYPES
    }

    pub(crate) fn valid_type(s: &str) -> bool {
        !s.is_empty() && s.split('-').all(|p| (3..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphanumeric()))
    }

    pub(crate) fn new<'scope>(a: &mut Scope<'scope, '_>, locales: Value<'scope>, options: Value<'scope>) -> Result<Self, Error> {
        let requested = Tags::list(a, locales)?;
        let options = Options::coerce(a, options)?;
        let usage = options.choice(a, "usage")?.unwrap_or(Usage::Sort);
        options.matcher(a)?;
        let collation = options.text(a, "collation")?;
        if collation.as_deref().is_some_and(|c| !Self::valid_type(c)) {
            return Err(a.throw(ErrorKind::Range, "Invalid collation"));
        }
        let numeric = options.boolean(a, "numeric")?.map(|b| String::from(if b { "true" } else { "false" }));
        let case_first: Option<CaseFirst> = options.choice(a, "caseFirst")?;
        let overrides = [collation, case_first.map(|c| String::from(c.name())), numeric];
        let resolved = Tags::resolve::<CollatorData>(a, &requested, &overrides);
        let sensitivity = options.choice(a, "sensitivity")?.unwrap_or(Sensitivity::Variant);
        let ignore_punctuation = options.boolean(a, "ignorePunctuation")?;
        let collation = String::from(resolved.value(KEY_COLLATION).unwrap_or(DEFAULT_COLLATION));
        let numeric = resolved.value(KEY_NUMERIC) == Some("true");
        let case_first = resolved.value(KEY_CASE_FIRST).and_then(|c| CaseFirst::try_from(c).ok()).unwrap_or(CaseFirst::False);
        let mut prefs = CollatorPreferences::from(&Locale::from(resolved.locale.id.clone()));
        prefs.collation_type = match usage {
            Usage::Search => Some(CollationType::Search),
            Usage::Sort => Keyword::try_from_str(&collation).ok().and_then(|k| CollationType::try_from(&k).ok()),
        };
        prefs.numeric_ordering = Some(if numeric { CollationNumericOrdering::True } else { CollationNumericOrdering::False });
        prefs.case_first = Some(match case_first {
            CaseFirst::Upper => CollationCaseFirst::Upper,
            CaseFirst::Lower => CollationCaseFirst::Lower,
            CaseFirst::False => CollationCaseFirst::False,
        });
        let mut opts = CollatorOptions::default();
        opts.strength = Some(match sensitivity {
            Sensitivity::Base | Sensitivity::Case => Strength::Primary,
            Sensitivity::Accent => Strength::Secondary,
            Sensitivity::Variant => Strength::Tertiary,
        });
        opts.case_level = Some(if sensitivity == Sensitivity::Case { CaseLevel::On } else { CaseLevel::Off });
        opts.alternate_handling = ignore_punctuation.map(|i| if i { AlternateHandling::Shifted } else { AlternateHandling::NonIgnorable });
        let collator = CollatorBorrowed::try_new(prefs, opts).map_err(|_| a.throw(ErrorKind::Range, "Unsupported collator options"))?;
        let ignore_punctuation = collator.resolved_options().alternate_handling == AlternateHandling::Shifted;
        Ok(Self { collator, settings: Settings { locale: resolved.tag(), usage, sensitivity, ignore_punctuation, collation, numeric, case_first } })
    }

    pub(crate) fn compare(&self, x: &str, y: &str) -> f64 {
        match self.collator.compare(x, y) {
            Ordering::Less => -1.0,
            Ordering::Equal => 0.0,
            Ordering::Greater => 1.0,
        }
    }

}

impl Settings {
    fn resolved<'scope>(&self, a: &mut Scope<'scope, '_>) -> Result<Value<'scope>, Error> {
        let o = a.object();
        let fields: [(&str, Value<'scope>); 7] = [
            ("locale", a.string(&self.locale).into()),
            ("usage", a.string(self.usage.name()).into()),
            ("sensitivity", a.string(self.sensitivity.name()).into()),
            ("ignorePunctuation", self.ignore_punctuation.into()),
            ("collation", a.string(&self.collation).into()),
            ("numeric", self.numeric.into()),
            ("caseFirst", a.string(self.case_first.name()).into()),
        ];
        for (k, v) in fields {
            Tags::put(a, o, k, v)?;
        }
        Ok(o.into())
    }
}

pub(crate) struct Collators;

impl Collators {
    pub(crate) fn construct<'scope>(a: &mut Scope<'scope, '_>, ctor: JsObject<'scope>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let proto = Hosted::proto(a, ctor, this);
        let c = Collation::new(a, args.at(0), args.at(1))?;
        Ok(Hosted::alloc(a, Service::Collator(Box::new(c)), proto).into())
    }

    fn with<'scope, R>(a: &Scope<'scope, '_>, this: Value<'scope>, method: &str, f: impl FnOnce(&Collation<'_>) -> R) -> Result<R, Error> {
        Hosted::with(a, this, |h| match &h.service {
            Service::Collator(c) => Some(f(c)),
            _ => None,
        })
        .ok_or_else(|| Hosted::incompatible(a, method))
    }

    fn compare_bound<'scope>(a: &mut Scope<'scope, '_>, f: JsObject<'scope>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let target = Hosted::slot(f, TARGET_SLOT).unwrap_or_default();
        let x = a.to_string(args.at(0))?;
        let y = a.to_string(args.at(1))?;
        Ok(Self::with(a, target, "compare", |c| c.compare(&x, &y))?.into())
    }

    pub(crate) fn compare<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::with(a, this, "get Intl.Collator.prototype.compare", |_| ())?;
        Hosted::bound(a, this, Self::compare_bound, 2).map(Value::from).ok_or_else(|| Hosted::incompatible(a, "compare"))
    }

    pub(crate) fn resolved_options<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let settings = Self::with(a, this, "Intl.Collator.prototype.resolvedOptions", |c| c.settings.clone())?;
        settings.resolved(a)
    }

    pub(crate) fn supported_locales_of<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Tags::supported(a, args.at(0), args.at(1))
    }

    pub(crate) fn locale_compare<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        if this.is_nullish() {
            return Err(a.throw(ErrorKind::Type, "String.prototype.localeCompare called on null or undefined"));
        }
        let s = a.to_string(this)?;
        let t = a.to_string(args.at(0))?;
        let c = Collation::new(a, args.at(1), args.at(2))?;
        Ok(c.compare(&s, &t).into())
    }
}

pub(crate) struct Cases;

impl Cases {
    fn transform<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>], upper: bool) -> Result<Value<'scope>, Error> {
        if this.is_nullish() {
            return Err(a.throw(ErrorKind::Type, "String.prototype.toLocaleUpperCase called on null or undefined"));
        }
        let s = a.to_string(this)?;
        let requested = Tags::list(a, args.at(0))?;
        let language = requested.first().map_or_else(|| Tags::default_locale(a).id.language, |l| l.id.language);
        let id = LanguageIdentifier::from(language);
        let mapper = CaseMapper::new();
        let out = if upper { mapper.uppercase_to_string(s.as_str(), &id) } else { mapper.lowercase_to_string(s.as_str(), &id) };
        Ok(a.string(&out).into())
    }

    pub(crate) fn to_upper<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::transform(a, this, args, true)
    }

    pub(crate) fn to_lower<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::transform(a, this, args, false)
    }
}
