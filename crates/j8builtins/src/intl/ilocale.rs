use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use icu_calendar::week::{WeekInformation, WeekPreferences};
use icu_locale::{Direction, LocaleDirectionality, LocaleExpander};
use icu_locale_core::extensions::unicode::{Key, Value as Keyword};
use icu_locale_core::subtags::{Language, Region, Script, Variant, Variants};
use icu_locale_core::{LanguageIdentifier, Locale};
use icu_time::zone::iana::IanaParserExtended;
use j8framework::{Error, ErrorKind, JsObject, Scope, Value, ValueKind};

use super::collator::Collation;
use super::datetime::DateTimeFormat;
use super::locale::{Options, Tags};
use super::number::NumberFormat;
use super::service::{Hosted, PROTO_SLOT, Service};
use crate::Args;

const WORLD: &str = "001";
const REGION_LETTERS: usize = 2;
const REGION_DIGITS: usize = 3;
const WEEKDAYS: [&str; 8] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat", "sun"];
const ROOT_COLLATIONS: [&str; 2] = ["emoji", "eor"];
const NUMERIC_KEY: &str = "kn";
const TRUE: &str = "true";
const ROOT_LANGUAGE: &str = "en";
const UNKNOWN_SCRIPT: &str = "Zzzz";

#[derive(Clone, Copy)]
enum Keyed {
    Calendar,
    Collation,
    FirstDay,
    HourCycle,
    CaseFirst,
    Numeric,
    Numbering,
}

impl Keyed {
    const ORDER: [Self; 7] = [Self::Calendar, Self::Collation, Self::FirstDay, Self::HourCycle, Self::CaseFirst, Self::Numeric, Self::Numbering];

    const fn key(self) -> &'static str {
        match self {
            Self::Calendar => "ca",
            Self::Collation => "co",
            Self::FirstDay => "fw",
            Self::HourCycle => "hc",
            Self::CaseFirst => "kf",
            Self::Numeric => NUMERIC_KEY,
            Self::Numbering => "nu",
        }
    }

    const fn option(self) -> &'static str {
        match self {
            Self::Calendar => "calendar",
            Self::Collation => "collation",
            Self::FirstDay => "firstDayOfWeek",
            Self::HourCycle => "hourCycle",
            Self::CaseFirst => "caseFirst",
            Self::Numeric => "numeric",
            Self::Numbering => "numberingSystem",
        }
    }

    fn valid(self, v: &str) -> bool {
        match self {
            Self::HourCycle => matches!(v, "h11" | "h12" | "h23" | "h24"),
            Self::CaseFirst => matches!(v, "upper" | "lower" | "false"),
            Self::Numeric => true,
            _ => Collation::valid_type(v),
        }
    }

    fn read<'scope>(self, a: &mut Scope<'scope, '_>, options: Options<'scope>) -> Result<Option<String>, Error> {
        if let Self::Numeric = self {
            return Ok(options.boolean(a, self.option())?.map(|b| String::from(if b { TRUE } else { "false" })));
        }
        let Some(v) = options.text(a, self.option())? else { return Ok(None) };
        let v = match self {
            Self::FirstDay => v.parse::<usize>().ok().and_then(|d| WEEKDAYS.get(d)).map_or(v, |d| String::from(*d)),
            _ => v,
        };
        if !self.valid(&v) {
            return Err(a.throw(ErrorKind::Range, &alloc::format!("Incorrect locale information provided: {} is not a valid {}", v, self.option())));
        }
        Ok(Some(v))
    }
}

pub(crate) struct Subtags;

impl Subtags {
    pub(crate) fn language(s: &str) -> bool {
        matches!(s.len(), 2 | 3 | 5..=8) && s.bytes().all(|b| b.is_ascii_alphabetic())
    }

    pub(crate) fn script(s: &str) -> bool {
        s.len() == 4 && s.bytes().all(|b| b.is_ascii_alphabetic())
    }

    pub(crate) fn region(s: &str) -> bool {
        (s.len() == REGION_LETTERS && s.bytes().all(|b| b.is_ascii_alphabetic())) || (s.len() == REGION_DIGITS && s.bytes().all(|b| b.is_ascii_digit()))
    }

    pub(crate) fn variants(s: &str) -> Option<Variants> {
        let mut seen: Vec<Variant> = Vec::new();
        for part in s.split('-') {
            let ok = match part.len() {
                5..=8 => part.bytes().all(|b| b.is_ascii_alphanumeric()),
                4 => part.as_bytes()[0].is_ascii_digit() && part.bytes().all(|b| b.is_ascii_alphanumeric()),
                _ => false,
            };
            let v = Variant::try_from_str(part).ok().filter(|_| ok)?;
            if seen.contains(&v) {
                return None;
            }
            seen.push(v);
        }
        seen.sort();
        Some(Variants::from_vec_unchecked(seen))
    }
}

pub(crate) struct Locales;

impl Locales {
    pub(crate) fn of<'scope>(a: &Scope<'scope, '_>, v: Value<'scope>) -> Option<Locale> {
        Hosted::with(a, v, |h| match &h.service {
            Service::Locale(l) => Some((**l).clone()),
            _ => None,
        })
    }

    fn this<'scope>(a: &Scope<'scope, '_>, this: Value<'scope>, method: &str) -> Result<Locale, Error> {
        Self::of(a, this).ok_or_else(|| Hosted::incompatible(a, method))
    }

    fn bad(a: &Scope<'_, '_>, what: &str) -> Error {
        a.throw(ErrorKind::Range, &alloc::format!("Incorrect locale information provided: {what}"))
    }

    fn apply<'scope>(a: &mut Scope<'scope, '_>, mut locale: Locale, options: Options<'scope>) -> Result<Locale, Error> {
        let language = options.text(a, "language")?;
        if language.as_deref().is_some_and(|s| !Subtags::language(s)) {
            return Err(Self::bad(a, "language"));
        }
        let script = options.text(a, "script")?;
        if script.as_deref().is_some_and(|s| !Subtags::script(s)) {
            return Err(Self::bad(a, "script"));
        }
        let region = options.text(a, "region")?;
        if region.as_deref().is_some_and(|s| !Subtags::region(s)) {
            return Err(Self::bad(a, "region"));
        }
        let variants = match options.text(a, "variants")? {
            Some(v) => Some(Subtags::variants(&v).ok_or_else(|| Self::bad(a, "variants"))?),
            None => None,
        };
        if let Some(l) = language {
            locale.id.language = Language::try_from_str(&l).map_err(|_| Self::bad(a, "language"))?;
        }
        if let Some(s) = script {
            locale.id.script = Some(Script::try_from_str(&s).map_err(|_| Self::bad(a, "script"))?);
        }
        if let Some(r) = region {
            locale.id.region = Some(Region::try_from_str(&r).map_err(|_| Self::bad(a, "region"))?);
        }
        if let Some(v) = variants {
            locale.id.variants = v;
        }
        let mut locale = Tags::parse(&locale.to_string()).ok_or_else(|| Self::bad(a, "tag"))?;
        for k in Keyed::ORDER {
            let Some(v) = k.read(a, options)? else { continue };
            let key = Key::try_from_str(k.key()).map_err(|_| Self::bad(a, k.option()))?;
            let value = Keyword::try_from_str(&v.to_ascii_lowercase()).map_err(|_| Self::bad(a, k.option()))?;
            locale.extensions.unicode.keywords.set(key, value);
        }
        Tags::parse(&locale.to_string()).ok_or_else(|| Self::bad(a, "tag"))
    }

    pub(crate) fn construct<'scope>(a: &mut Scope<'scope, '_>, ctor: JsObject<'scope>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        if matches!(a.new_target.kind(), ValueKind::Undefined) {
            return Err(a.throw(ErrorKind::Type, "Constructor Intl.Locale requires 'new'"));
        }
        let proto = Hosted::proto(a, ctor, this);
        let tag = args.at(0);
        if !matches!(tag.kind(), ValueKind::String(_) | ValueKind::Object(_)) {
            return Err(a.throw(ErrorKind::Type, "First argument to Intl.Locale constructor can't be empty or missing"));
        }
        let locale = match Self::of(a, tag) {
            Some(l) => l,
            None => {
                let text = a.to_string(tag)?;
                Tags::parse(text.as_str()).ok_or_else(|| Self::bad(a, text.as_str()))?
            }
        };
        let options = Options::coerce(a, args.at(1))?;
        let locale = Self::apply(a, locale, options)?;
        Ok(Hosted::alloc(a, Service::Locale(Box::new(locale)), proto).into())
    }

    fn keyword(locale: &Locale, k: Keyed) -> Option<String> {
        let key = Key::try_from_str(k.key()).ok()?;
        locale.extensions.unicode.keywords.get(&key).map(Keyword::to_string)
    }

    fn text<'scope>(a: &Scope<'scope, '_>, s: Option<String>) -> Value<'scope> {
        s.map_or(Value::Undefined, |s| a.string(&s).into())
    }

    fn region_of(locale: &Locale) -> String {
        let from_keyword = |key: &str| {
            let v = Key::try_from_str(key).ok().and_then(|k| locale.extensions.unicode.keywords.get(&k)).map(Keyword::to_string)?;
            let r = if v.as_bytes().first()?.is_ascii_digit() { v.get(..REGION_DIGITS)? } else { v.get(..REGION_LETTERS)? };
            Subtags::region(r).then(|| r.to_ascii_uppercase())
        };
        if let Some(r) = from_keyword("rg") {
            return r;
        }
        if let Some(r) = locale.id.region {
            return String::from(r.as_str());
        }
        if let Some(r) = from_keyword("sd") {
            return r;
        }
        let mut id = locale.id.clone();
        Self::maximized(&mut id);
        id.region.map_or_else(|| String::from(WORLD), |r| String::from(r.as_str()))
    }

    fn regional(locale: &Locale) -> Locale {
        let mut id = LanguageIdentifier::from(locale.id.language);
        id.script = locale.id.script;
        id.region = Region::try_from_str(&Self::region_of(locale)).ok();
        let mut out = Locale::from(id);
        if let Some(fw) = Self::keyword(locale, Keyed::FirstDay).and_then(|v| Keyword::try_from_str(&v).ok())
            && let Ok(k) = Key::try_from_str(Keyed::FirstDay.key())
        {
            out.extensions.unicode.keywords.set(k, fw);
        }
        out
    }

    fn calendars(region: &str) -> &'static [&'static str] {
        match region {
            "BD" | "AL" | "AZ" | "MV" | "TJ" | "TM" | "TR" | "UZ" | "XK" | "DJ" | "DZ" | "EH" | "ER" | "IQ" | "JO" | "KM" | "LB" | "LY" | "MA" | "MR" | "OM" | "PK" | "PS" | "SD" | "SY" | "TD" | "TN" | "YE" => &["gregory", "islamic-civil", "islamic-tbla"],
            "AE" | "BH" | "KW" | "QA" => &["gregory", "islamic-umalqura", "islamic-civil", "islamic-tbla"],
            "AF" | "IR" => &["persian", "gregory", "islamic-civil", "islamic-tbla"],
            "CN" | "CX" | "HK" | "MO" | "SG" => &["gregory", "chinese"],
            "EG" => &["gregory", "coptic", "islamic-civil", "islamic-tbla"],
            "ET" => &["gregory", "ethiopic"],
            "IL" => &["gregory", "hebrew", "islamic-civil", "islamic-tbla"],
            "IN" => &["gregory", "indian"],
            "JP" => &["gregory", "japanese"],
            "KR" => &["gregory", "dangi"],
            "SA" => &["islamic-umalqura", "gregory"],
            "TH" => &["buddhist", "gregory"],
            "TW" => &["gregory", "roc", "chinese"],
            _ => &["gregory"],
        }
    }

    fn list<'scope>(a: &mut Scope<'scope, '_>, items: &[String]) -> Value<'scope> {
        Tags::strings(a, items)
    }

    fn preferred(locale: &Locale, k: Keyed) -> Option<Vec<String>> {
        Self::keyword(locale, k).map(|v| alloc::vec![if v.is_empty() && matches!(k, Keyed::Numeric) { String::from(TRUE) } else { v }])
    }

    pub(crate) fn get_calendars<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let l = Self::this(a, this, "Intl.Locale.prototype.getCalendars")?;
        let items = Self::preferred(&l, Keyed::Calendar).unwrap_or_else(|| Self::calendars(&Self::region_of(&l)).iter().map(|s| String::from(*s)).collect());
        Ok(Self::list(a, &items))
    }

    pub(crate) fn get_collations<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let l = Self::this(a, this, "Intl.Locale.prototype.getCollations")?;
        let items = Self::preferred(&l, Keyed::Collation).unwrap_or_else(|| {
            let base = Locale::from(LanguageIdentifier::from(l.id.language));
            let mut found: Vec<String> = Collation::types().iter().filter(|t| Tags::available(&base.id) && Collation::has_type(t, &base)).map(|s| String::from(*s)).collect();
            found.extend(ROOT_COLLATIONS.iter().map(|s| String::from(*s)));
            found.sort();
            found.dedup();
            found
        });
        Ok(Self::list(a, &items))
    }

    pub(crate) fn get_hour_cycles<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let l = Self::this(a, this, "Intl.Locale.prototype.getHourCycles")?;
        let items = Self::preferred(&l, Keyed::HourCycle).unwrap_or_else(|| DateTimeFormat::preferred_cycle(&Self::regional(&l)).map(|c| alloc::vec![String::from(c)]).unwrap_or_default());
        Ok(Self::list(a, &items))
    }

    pub(crate) fn get_numbering_systems<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let l = Self::this(a, this, "Intl.Locale.prototype.getNumberingSystems")?;
        let items = Self::preferred(&l, Keyed::Numbering).unwrap_or_else(|| alloc::vec![NumberFormat::default_numbering(&Locale::from(l.id.clone()))]);
        Ok(Self::list(a, &items))
    }

    pub(crate) fn get_time_zones<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let l = Self::this(a, this, "Intl.Locale.prototype.getTimeZones")?;
        let Some(region) = l.id.region else { return Ok(Value::Undefined) };
        let prefix = region.as_str().to_ascii_lowercase();
        let mut zones: Vec<String> = IanaParserExtended::new().iter().filter(|z| z.time_zone.0.as_str().starts_with(&prefix) && z.time_zone.0.as_str().len() > prefix.len()).map(|z| String::from(z.canonical)).collect();
        zones.sort();
        zones.dedup();
        Ok(Self::list(a, &zones))
    }

    pub(crate) fn get_text_info<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let l = Self::this(a, this, "Intl.Locale.prototype.getTextInfo")?;
        let mut id = l.id.clone();
        Self::maximized(&mut id);
        let unknown = l.id.script.is_some_and(|s| s.as_str() == UNKNOWN_SCRIPT);
        let dir = match LocaleDirectionality::new_extended().get(&id).filter(|_| !unknown) {
            Some(Direction::RightToLeft) => a.string("rtl").into(),
            Some(Direction::LeftToRight) => a.string("ltr").into(),
            _ => Value::Undefined,
        };
        let o = a.object();
        Tags::put(a, o, "direction", dir)?;
        Ok(o.into())
    }

    pub(crate) fn get_week_info<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let l = Self::this(a, this, "Intl.Locale.prototype.getWeekInfo")?;
        let info = WeekInformation::try_new(WeekPreferences::from(&Self::regional(&l))).map_err(|_| a.throw(ErrorKind::Range, "No week data"))?;
        let mut weekend: Vec<u8> = info.weekend().map(|d| d as u8).collect();
        weekend.sort_unstable();
        let days: Vec<Value<'scope>> = weekend.into_iter().map(|d| Value::from(f64::from(d))).collect();
        let o = a.object();
        Tags::put(a, o, "firstDay", Value::from(f64::from(info.first_weekday as u8)))?;
        let w = a.array_from_slice(&days).into();
        Tags::put(a, o, "weekend", w)?;
        Ok(o.into())
    }

    pub(crate) fn maximize<'scope>(a: &mut Scope<'scope, '_>, f: JsObject<'scope>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let mut l = Self::this(a, this, "Intl.Locale.prototype.maximize")?;
        Self::maximized(&mut l.id);
        Self::fresh(a, f, l)
    }

    pub(crate) fn minimize<'scope>(a: &mut Scope<'scope, '_>, f: JsObject<'scope>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let mut l = Self::this(a, this, "Intl.Locale.prototype.minimize")?;
        Self::maximized(&mut l.id);
        LocaleExpander::new_extended().minimize(&mut l.id);
        Self::fresh(a, f, l)
    }

    fn maximized(id: &mut LanguageIdentifier) {
        LocaleExpander::new_extended().maximize(id);
        if id.language.is_unknown() {
            let mut root = LanguageIdentifier::from(Language::try_from_str(ROOT_LANGUAGE).unwrap_or(id.language));
            LocaleExpander::new_extended().maximize(&mut root);
            id.language = root.language;
            id.script = id.script.or(root.script);
            id.region = id.region.or(root.region);
        }
    }

    fn fresh<'scope>(a: &mut Scope<'scope, '_>, f: JsObject<'scope>, l: Locale) -> Result<Value<'scope>, Error> {
        let proto = Hosted::slot(f, PROTO_SLOT).and_then(Value::as_object).unwrap_or(a.context.object_proto);
        Ok(Hosted::alloc(a, Service::Locale(Box::new(l)), proto).into())
    }

    pub(crate) fn to_string<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let l = Self::this(a, this, "Intl.Locale.prototype.toString")?;
        Ok(a.string(&l.to_string()).into())
    }

    pub(crate) fn to_json<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let l = Self::this(a, this, "Intl.Locale.prototype.toJSON")?;
        Ok(a.string(&l.to_string()).into())
    }

    pub(crate) fn base_name<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let l = Self::this(a, this, "get Intl.Locale.prototype.baseName")?;
        Ok(a.string(&l.id.to_string()).into())
    }

    pub(crate) fn language<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let l = Self::this(a, this, "get Intl.Locale.prototype.language")?;
        Ok(a.string(l.id.language.as_str()).into())
    }

    pub(crate) fn script<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let l = Self::this(a, this, "get Intl.Locale.prototype.script")?;
        Ok(Self::text(a, l.id.script.map(|s| String::from(s.as_str()))))
    }

    pub(crate) fn region<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let l = Self::this(a, this, "get Intl.Locale.prototype.region")?;
        Ok(Self::text(a, l.id.region.map(|s| String::from(s.as_str()))))
    }

    pub(crate) fn variants<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let l = Self::this(a, this, "get Intl.Locale.prototype.variants")?;
        Ok(Self::text(a, (!l.id.variants.is_empty()).then(|| l.id.variants.to_string())))
    }

    fn keyed<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, k: Keyed) -> Result<Value<'scope>, Error> {
        let l = Self::this(a, this, &alloc::format!("get Intl.Locale.prototype.{}", k.option()))?;
        Ok(Self::text(a, Self::keyword(&l, k)))
    }

    pub(crate) fn calendar<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::keyed(a, this, Keyed::Calendar)
    }

    pub(crate) fn collation<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::keyed(a, this, Keyed::Collation)
    }

    pub(crate) fn first_day_of_week<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::keyed(a, this, Keyed::FirstDay)
    }

    pub(crate) fn hour_cycle<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::keyed(a, this, Keyed::HourCycle)
    }

    pub(crate) fn case_first<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::keyed(a, this, Keyed::CaseFirst)
    }

    pub(crate) fn numbering_system<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::keyed(a, this, Keyed::Numbering)
    }

    pub(crate) fn numeric<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let l = Self::this(a, this, "get Intl.Locale.prototype.numeric")?;
        Ok(Value::from(Self::keyword(&l, Keyed::Numeric).is_some_and(|v| v.is_empty() || v == TRUE)))
    }
}
