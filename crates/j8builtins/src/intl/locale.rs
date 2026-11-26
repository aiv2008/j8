use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use icu_locale::{LocaleCanonicalizer, LocaleExpander, TransformResult};
use icu_locale_core::extensions::unicode::{Key, Keywords, Value as Keyword};
use icu_locale_core::{LanguageIdentifier, Locale};
use j8framework::{Error, ErrorKind, JsObject, Key as PropertyKey, Scope, Value, ValueKind};


macro_rules! choices {
    ($($name:ident { $($variant:ident = $text:literal),* $(,)? })*) => {$(
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub(crate) enum $name {
            $($variant),*
        }

        #[allow(dead_code, reason = "not every option is reported back by resolvedOptions")]
        impl $name {
            pub(crate) const fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),*
                }
            }
        }

        impl TryFrom<&str> for $name {
            type Error = ();

            fn try_from(s: &str) -> Result<Self, ()> {
                match s {
                    $($text => Ok(Self::$variant),)*
                    _ => Err(()),
                }
            }
        }
    )*};
}

pub(crate) use choices;

choices! {
    Matcher { Lookup = "lookup", BestFit = "best fit" }
}

#[derive(Clone, Copy)]
pub(crate) struct Options<'scope> {
    object: Option<JsObject<'scope>>,
}

impl<'scope> Options<'scope> {
    pub(crate) fn coerce(a: &Scope<'scope, '_>, v: Value<'scope>) -> Result<Self, Error> {
        match v.kind() {
            ValueKind::Undefined => Ok(Self { object: None }),
            _ => Ok(Self { object: Some(a.to_object(v)?) }),
        }
    }

    pub(crate) fn object(a: &Scope<'scope, '_>, v: Value<'scope>) -> Result<Self, Error> {
        match v.kind() {
            ValueKind::Undefined => Ok(Self { object: None }),
            ValueKind::Object(o) => Ok(Self { object: Some(o) }),
            _ => Err(a.throw(ErrorKind::Type, "Options must be an object")),
        }
    }

    pub(crate) fn value(self, a: &mut Scope<'scope, '_>, name: &str) -> Result<Value<'scope>, Error> {
        match self.object {
            Some(o) => a.get_named(o.into(), name),
            None => Ok(Value::Undefined),
        }
    }

    pub(crate) fn text(self, a: &mut Scope<'scope, '_>, name: &str) -> Result<Option<String>, Error> {
        let v = self.value(a, name)?;
        match v.kind() {
            ValueKind::Undefined => Ok(None),
            _ => Ok(Some(String::from(&**a.to_string(v)?))),
        }
    }

    pub(crate) fn choice<T: for<'s> TryFrom<&'s str>>(self, a: &mut Scope<'scope, '_>, name: &str) -> Result<Option<T>, Error> {
        let Some(s) = self.text(a, name)? else { return Ok(None) };
        T::try_from(&s).map(Some).map_err(|_| a.throw(ErrorKind::Range, &format!("Value {s} out of range for options property {name}")))
    }

    pub(crate) fn boolean(self, a: &mut Scope<'scope, '_>, name: &str) -> Result<Option<bool>, Error> {
        let v = self.value(a, name)?;
        match v.kind() {
            ValueKind::Undefined => Ok(None),
            _ => Ok(Some(a.to_boolean(v))),
        }
    }

    pub(crate) fn number(self, a: &mut Scope<'scope, '_>, name: &str, min: u32, max: u32) -> Result<Option<u32>, Error> {
        let v = self.value(a, name)?;
        Self::range(a, v, name, min, max)
    }

    pub(crate) fn range(a: &mut Scope<'scope, '_>, v: Value<'scope>, name: &str, min: u32, max: u32) -> Result<Option<u32>, Error> {
        if matches!(v.kind(), ValueKind::Undefined) {
            return Ok(None);
        }
        let x = a.to_number(v)?;
        if x.is_nan() || x < f64::from(min) || x > f64::from(max) {
            return Err(a.throw(ErrorKind::Range, &format!("{name} value is out of range.")));
        }
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "checked to be within [min, max]")]
        let n = libm::floor(x) as u32;
        Ok(Some(n))
    }

    pub(crate) fn matcher(self, a: &mut Scope<'scope, '_>) -> Result<Matcher, Error> {
        Ok(self.choice(a, "localeMatcher")?.unwrap_or(Matcher::BestFit))
    }
}

const FALLBACK_LOCALE: &str = "en-US";

const KEYWORD_ALIASES: [(&str, &str, &str); 5] = [
    ("ca", "ethiopic-amete-alem", "ethioaa"),
    ("ca", "islamicc", "islamic-civil"),
    ("ks", "primary", "level1"),
    ("ks", "tertiary", "level3"),
    ("ms", "imperial", "uksystem"),
];

const BOOLEAN_KEYS: [&str; 5] = ["kb", "kc", "kh", "kk", "kn"];

const VARIANT_MIN: usize = 5;

const DIGIT_VARIANT: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ExtKey {
    Calendar,
    Collation,
    HourCycle,
    CaseFirst,
    Numeric,
    Numbering,
}

impl ExtKey {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Calendar => "ca",
            Self::Collation => "co",
            Self::HourCycle => "hc",
            Self::CaseFirst => "kf",
            Self::Numeric => "kn",
            Self::Numbering => "nu",
        }
    }
}

impl From<ExtKey> for Key {
    fn from(k: ExtKey) -> Self {
        Key::try_from_str(k.name()).unwrap_or_else(|_| unreachable!("extension keys are two letters"))
    }
}

pub(crate) trait LocaleData {
    const KEYS: &'static [ExtKey];

    fn default_value(key: ExtKey, locale: &Locale) -> Option<String>;

    fn supports(key: ExtKey, value: &str, locale: &Locale) -> bool;
}

pub(crate) struct Resolved {
    pub(crate) locale: Locale,
    pub(crate) values: Vec<Option<String>>,
}

impl Resolved {
    pub(crate) fn value(&self, i: usize) -> Option<&str> {
        self.values.get(i).and_then(Option::as_deref)
    }

    pub(crate) fn tag(&self) -> String {
        self.locale.to_string()
    }
}

pub(crate) struct Tags;

impl Tags {
    fn duplicate_variants<'t>(subtags: impl Iterator<Item = &'t str>) -> bool {
        let mut seen: Vec<String> = Vec::new();
        for s in subtags {
            let variant = s.len() >= VARIANT_MIN || (s.len() == DIGIT_VARIANT && s.as_bytes()[0].is_ascii_digit());
            if variant {
                let lower = s.to_ascii_lowercase();
                if seen.contains(&lower) {
                    return true;
                }
                seen.push(lower);
            }
        }
        false
    }

    fn well_formed(tag: &str) -> bool {
        if !tag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return false;
        }
        let subtags: Vec<&str> = tag.split('-').collect();
        let mut sections = subtags.split(|s| s.len() == 1);
        if sections.next().is_some_and(|id| Self::duplicate_variants(id.iter().copied().skip(1))) {
            return false;
        }
        let tlang = subtags.iter().position(|s| s.eq_ignore_ascii_case("t")).map(|i| &subtags[i + 1..]);
        !tlang.is_some_and(|rest| {
            let end = rest.iter().position(|s| s.len() == 2 && s.as_bytes()[1].is_ascii_digit() || s.len() == 1).unwrap_or(rest.len());
            Self::duplicate_variants(rest[..end].iter().copied().skip(1))
        })
    }

    pub(crate) fn parse(tag: &str) -> Option<Locale> {
        if !Self::well_formed(tag) {
            return None;
        }
        let mut locale = Locale::try_from_str(tag).ok()?;
        LocaleCanonicalizer::new_extended().canonicalize(&mut locale);
        Self::canonical_keywords(&mut locale);
        Some(locale)
    }

    fn canonical_keywords(locale: &mut Locale) {
        let keywords = &mut locale.extensions.unicode.keywords;
        for (key, from, to) in KEYWORD_ALIASES {
            let Ok(key) = Key::try_from_str(key) else { continue };
            if keywords.get(&key).is_some_and(|v| v.to_string() == *from)
                && let Ok(v) = Keyword::try_from_str(to)
            {
                keywords.set(key, v);
            }
        }
        let yes: Vec<Key> = keywords.iter().filter(|(k, v)| BOOLEAN_KEYS.contains(&k.as_str()) && v.to_string() == "yes").map(|(k, _)| *k).collect();
        for key in yes {
            keywords.set(key, Keyword::default());
        }
    }

    fn element<'scope>(a: &mut Scope<'scope, '_>, v: Value<'scope>) -> Result<Locale, Error> {
        if !matches!(v.kind(), ValueKind::String(_) | ValueKind::Object(_)) {
            return Err(a.throw(ErrorKind::Type, "Language ID should be string or object."));
        }
        if let Some(l) = super::ilocale::Locales::of(a, v) {
            return Ok(l);
        }
        let tag = a.to_string(v)?;
        Self::parse(tag.as_str()).ok_or_else(|| a.throw(ErrorKind::Range, &alloc::format!("Incorrect locale information provided: {}", tag.as_str())))
    }

    pub(crate) fn list<'scope>(a: &mut Scope<'scope, '_>, locales: Value<'scope>) -> Result<Vec<Locale>, Error> {
        let mut seen: Vec<Locale> = Vec::new();
        match locales.kind() {
            ValueKind::Undefined => return Ok(seen),
            ValueKind::String(_) => {
                seen.push(Self::element(a, locales)?);
                return Ok(seen);
            }
            _ => {}
        }
        if let Some(l) = super::ilocale::Locales::of(a, locales) {
            seen.push(l);
            return Ok(seen);
        }
        let o = a.to_object(locales)?;
        let len = a.length_of_array_like(o.into())?;
        let mut k = 0.0;
        while k < len {
            if a.has_index(o, k)? {
                let v = a.get_index(o.into(), k)?;
                let locale = Self::element(a, v)?;
                if !seen.contains(&locale) {
                    seen.push(locale);
                }
            }
            k += 1.0;
        }
        Ok(seen)
    }

    pub(crate) fn available(id: &LanguageIdentifier) -> bool {
        if id.language.is_unknown() {
            return false;
        }
        let mut language = LanguageIdentifier::from(id.language);
        LocaleExpander::new_extended().maximize(&mut language) == TransformResult::Modified
    }

    pub(crate) fn default_locale(a: &Scope<'_, '_>) -> Locale {
        let host = a.platform.locale().and_then(|tag| Self::parse(&tag)).filter(|l| Self::available(&l.id));
        host.or_else(|| Self::parse(FALLBACK_LOCALE)).map_or(Locale::UNKNOWN, |l| Locale::from(l.id))
    }

    pub(crate) fn resolve<D: LocaleData>(a: &Scope<'_, '_>, requested: &[Locale], overrides: &[Option<String>]) -> Resolved {
        let found = requested.iter().find(|l| Self::available(&l.id));
        let mut locale = Locale::from(found.map_or_else(|| Self::default_locale(a).id, |l| l.id.clone()));
        let asked = found.map(|l| &l.extensions.unicode.keywords);
        let mut kept = Keywords::new();
        let mut values = Vec::with_capacity(D::KEYS.len());
        for (i, &key) in D::KEYS.iter().enumerate() {
            let mut value = D::default_value(key, &locale);
            let mut keyword = None;
            if let Some(k) = asked.and_then(|kw| kw.get(&Key::from(key))) {
                let text = if k.is_empty() { String::from("true") } else { k.to_string() };
                if D::supports(key, &text, &locale) {
                    value = Some(text);
                    keyword = Some(k.clone());
                }
            }
            if let Some(Some(o)) = overrides.get(i) {
                let o = Self::canonical_value(key, o);
                if value.as_deref() != Some(o.as_str()) && D::supports(key, &o, &locale) {
                    value = Some(o);
                    keyword = None;
                }
            }
            if let Some(k) = keyword {
                kept.set(Key::from(key), k);
            }
            values.push(value);
        }
        locale.extensions.unicode.keywords = kept;
        Resolved { locale, values }
    }

    fn canonical_value(key: ExtKey, value: &str) -> String {
        let lower = value.to_ascii_lowercase();
        let tag = alloc::format!("und-u-{}-{lower}", key.name());
        let canonical = Self::parse(&tag).and_then(|l| l.extensions.unicode.keywords.get(&Key::from(key)).map(Keyword::to_string));
        match canonical {
            Some(c) if c.is_empty() => String::from("true"),
            Some(c) => c,
            None => lower,
        }
    }

    pub(crate) fn supported<'scope>(a: &mut Scope<'scope, '_>, locales: Value<'scope>, options: Value<'scope>) -> Result<Value<'scope>, Error> {
        let requested = Self::list(a, locales)?;
        let options = Options::coerce(a, options)?;
        options.matcher(a)?;
        let found: Vec<Value<'scope>> = requested.iter().filter(|l| Self::available(&l.id)).map(|l| a.string(&l.to_string()).into()).collect();
        Ok(a.array_from_slice(&found).into())
    }

    pub(crate) fn strings<'scope>(a: &mut Scope<'scope, '_>, items: &[String]) -> Value<'scope> {
        let values: Vec<Value<'scope>> = items.iter().map(|s| a.string(s).into()).collect();
        a.array_from_slice(&values).into()
    }

    pub(crate) fn put<'scope>(a: &mut Scope<'scope, '_>, o: JsObject<'scope>, name: &str, v: Value<'scope>) -> Result<(), Error> {
        a.create_data_property_or_throw(o, PropertyKey::Str(name), v)
    }
}
