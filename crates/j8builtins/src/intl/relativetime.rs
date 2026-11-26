use alloc::boxed::Box;
use alloc::string::String;

use fixed_decimal::{Decimal, FloatPrecision, Sign};
use icu_experimental::relativetime::options::Numeric as IcuNumeric;
use icu_experimental::relativetime::{RelativeTimeFormatter, RelativeTimeFormatterOptions, RelativeTimeFormatterPreferences};
use icu_locale_core::Locale;
use icu_locale_core::extensions::unicode::Value as Keyword;
use icu_locale_core::preferences::extensions::unicode::keywords::NumberingSystem;
use icu_provider::DataError;
use j8framework::{Error, ErrorKind, JsObject, Scope, Value, ValueKind};
use writeable::{Part, Writeable};

use super::locale::{ExtKey, LocaleData, Options, Tags, choices};
use super::number::{CompactDisplay, Digits, Notation, NumberFormat};
use super::service::{Hosted, LITERAL, Pieces, Service};
use crate::Args;

choices! {
    RelStyle { Long = "long", Short = "short", Narrow = "narrow" }
    RelNumeric { Always = "always", Auto = "auto" }
}

const MAX_FRACTION: u32 = 3;
const KEY_NUMBERING: usize = 0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Unit {
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Quarter,
    Year,
}

impl Unit {
    const fn name(self) -> &'static str {
        match self {
            Self::Second => "second",
            Self::Minute => "minute",
            Self::Hour => "hour",
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Quarter => "quarter",
            Self::Year => "year",
        }
    }
}

impl TryFrom<&str> for Unit {
    type Error = ();

    fn try_from(s: &str) -> Result<Self, ()> {
        Ok(match s.strip_suffix('s').unwrap_or(s) {
            "second" => Self::Second,
            "minute" => Self::Minute,
            "hour" => Self::Hour,
            "day" => Self::Day,
            "week" => Self::Week,
            "month" => Self::Month,
            "quarter" => Self::Quarter,
            "year" => Self::Year,
            _ => return Err(()),
        })
    }
}

type Constructor = fn(RelativeTimeFormatterPreferences, RelativeTimeFormatterOptions) -> Result<RelativeTimeFormatter, DataError>;

struct RelData;

impl LocaleData for RelData {
    const KEYS: &'static [ExtKey] = &[ExtKey::Numbering];

    fn default_value(_: ExtKey, locale: &Locale) -> Option<String> {
        Some(NumberFormat::default_numbering(locale))
    }

    fn supports(_: ExtKey, value: &str, _: &Locale) -> bool {
        NumberFormat::numbering_systems().contains(&value)
    }
}

pub(crate) struct RelativeTimes {
    locale: String,
    numbering_system: String,
    style: RelStyle,
    numeric: RelNumeric,
    prefs: RelativeTimeFormatterPreferences,
    rounding: NumberFormat,
}

impl RelativeTimes {
    fn new<'scope>(a: &mut Scope<'scope, '_>, locales: Value<'scope>, options: Value<'scope>) -> Result<Self, Error> {
        let requested = Tags::list(a, locales)?;
        let options = Options::coerce(a, options)?;
        options.matcher(a)?;
        let numbering = options.text(a, "numberingSystem")?;
        if numbering.as_deref().is_some_and(|n| !NumberFormat::valid_numbering(n)) {
            return Err(a.throw(ErrorKind::Range, "Invalid numberingSystem"));
        }
        let resolved = Tags::resolve::<RelData>(a, &requested, &[numbering]);
        let style = options.choice(a, "style")?.unwrap_or(RelStyle::Long);
        let numeric = options.choice(a, "numeric")?.unwrap_or(RelNumeric::Always);
        let numbering_system = String::from(resolved.value(KEY_NUMBERING).unwrap_or("latn"));
        let base = Locale::from(resolved.locale.id.clone());
        let mut prefs = RelativeTimeFormatterPreferences::from(&base);
        prefs.numbering_system = Keyword::try_from_str(&numbering_system).ok().and_then(|k| NumberingSystem::try_from(&k).ok());
        let digits = Digits::read(a, Options::coerce(a, Value::Undefined)?, 0, MAX_FRACTION, Notation::Standard)?;
        let rounding = NumberFormat::rounding(a, &base, digits, Notation::Standard, CompactDisplay::Short)?;
        Ok(Self { locale: resolved.tag(), numbering_system, style, numeric, prefs, rounding })
    }

    fn constructor(&self, unit: Unit) -> Constructor {
        use RelativeTimeFormatter as F;
        match (self.style, unit) {
            (RelStyle::Long, Unit::Second) => F::try_new_long_second,
            (RelStyle::Long, Unit::Minute) => F::try_new_long_minute,
            (RelStyle::Long, Unit::Hour) => F::try_new_long_hour,
            (RelStyle::Long, Unit::Day) => F::try_new_long_day,
            (RelStyle::Long, Unit::Week) => F::try_new_long_week,
            (RelStyle::Long, Unit::Month) => F::try_new_long_month,
            (RelStyle::Long, Unit::Quarter) => F::try_new_long_quarter,
            (RelStyle::Long, Unit::Year) => F::try_new_long_year,
            (RelStyle::Short, Unit::Second) => F::try_new_short_second,
            (RelStyle::Short, Unit::Minute) => F::try_new_short_minute,
            (RelStyle::Short, Unit::Hour) => F::try_new_short_hour,
            (RelStyle::Short, Unit::Day) => F::try_new_short_day,
            (RelStyle::Short, Unit::Week) => F::try_new_short_week,
            (RelStyle::Short, Unit::Month) => F::try_new_short_month,
            (RelStyle::Short, Unit::Quarter) => F::try_new_short_quarter,
            (RelStyle::Short, Unit::Year) => F::try_new_short_year,
            (RelStyle::Narrow, Unit::Second) => F::try_new_narrow_second,
            (RelStyle::Narrow, Unit::Minute) => F::try_new_narrow_minute,
            (RelStyle::Narrow, Unit::Hour) => F::try_new_narrow_hour,
            (RelStyle::Narrow, Unit::Day) => F::try_new_narrow_day,
            (RelStyle::Narrow, Unit::Week) => F::try_new_narrow_week,
            (RelStyle::Narrow, Unit::Month) => F::try_new_narrow_month,
            (RelStyle::Narrow, Unit::Quarter) => F::try_new_narrow_quarter,
            (RelStyle::Narrow, Unit::Year) => F::try_new_narrow_year,
        }
    }

    fn formatter(&self, unit: Unit) -> Option<RelativeTimeFormatter> {
        let numeric = match self.numeric {
            RelNumeric::Always => IcuNumeric::Always,
            RelNumeric::Auto => IcuNumeric::Auto,
        };
        self.constructor(unit)(self.prefs, RelativeTimeFormatterOptions::default().with_numeric(numeric)).ok()
    }

    fn value(&self, x: f64) -> Option<Decimal> {
        let d = Decimal::try_from_f64(x, FloatPrecision::RoundTrip).ok()?;
        let (mut rounded, _) = self.rounding.rounded(&d);
        let named_zero = x == 0.0 && self.numeric == RelNumeric::Auto;
        rounded.sign = if named_zero { Sign::None } else if x.is_sign_negative() { Sign::Negative } else { rounded.sign };
        Some(rounded)
    }

    fn part(parts: &[&Part]) -> &'static str {
        match parts.last() {
            Some(p) if p.category == "decimal" => p.value,
            _ => LITERAL,
        }
    }

    fn pieces(&self, x: f64, unit: Unit) -> Option<Pieces> {
        let f = self.formatter(unit)?;
        let w = f.format(self.value(x)?);
        Some(Pieces::collect(&w, Self::part))
    }

    fn text(&self, x: f64, unit: Unit) -> Option<String> {
        let f = self.formatter(unit)?;
        Some(f.format(self.value(x)?).write_to_string().into_owned())
    }
}

pub(crate) struct RelativeTimeFormats;

impl RelativeTimeFormats {
    pub(crate) fn construct<'scope>(a: &mut Scope<'scope, '_>, ctor: JsObject<'scope>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        if matches!(a.new_target.kind(), ValueKind::Undefined) {
            return Err(a.throw(ErrorKind::Type, "Constructor Intl.RelativeTimeFormat requires 'new'"));
        }
        let proto = Hosted::proto(a, ctor, this);
        let r = RelativeTimes::new(a, args.at(0), args.at(1))?;
        Ok(Hosted::alloc(a, Service::RelativeTime(Box::new(r)), proto).into())
    }

    fn with<'scope, R>(a: &Scope<'scope, '_>, this: Value<'scope>, method: &str, f: impl FnOnce(&RelativeTimes) -> R) -> Result<R, Error> {
        Hosted::with(a, this, |h| match &h.service {
            Service::RelativeTime(r) => Some(f(r)),
            _ => None,
        })
        .ok_or_else(|| Hosted::incompatible(a, method))
    }

    fn arguments<'scope>(a: &mut Scope<'scope, '_>, args: &[Value<'scope>]) -> Result<(f64, Unit), Error> {
        let x = a.to_number(args.at(0))?;
        let unit = a.to_string(args.at(1))?;
        if !x.is_finite() {
            return Err(a.throw(ErrorKind::Range, "Invalid value"));
        }
        let unit = Unit::try_from(unit.as_str()).map_err(|()| a.throw(ErrorKind::Range, &alloc::format!("Invalid unit argument '{}'", unit.as_str())))?;
        Ok((x, unit))
    }

    pub(crate) fn format<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::with(a, this, "Intl.RelativeTimeFormat.prototype.format", |_| ())?;
        let (x, unit) = Self::arguments(a, args)?;
        let text = Self::with(a, this, "format", |r| r.text(x, unit))?.ok_or_else(|| a.throw(ErrorKind::Range, "Unsupported relative time"))?;
        Ok(a.string(&text).into())
    }

    pub(crate) fn format_to_parts<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::with(a, this, "Intl.RelativeTimeFormat.prototype.formatToParts", |_| ())?;
        let (x, unit) = Self::arguments(a, args)?;
        let pieces = Self::with(a, this, "formatToParts", |r| r.pieces(x, unit))?.ok_or_else(|| a.throw(ErrorKind::Range, "Unsupported relative time"))?;
        pieces.array_with(a, Some(("unit", unit.name())))
    }

    pub(crate) fn resolved_options<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (locale, style, numeric, nu) = Self::with(a, this, "Intl.RelativeTimeFormat.prototype.resolvedOptions", |r| (r.locale.clone(), r.style, r.numeric, r.numbering_system.clone()))?;
        let o = a.object();
        let fields: [(&str, Value<'scope>); 4] = [
            ("locale", a.string(&locale).into()),
            ("style", a.string(style.name()).into()),
            ("numeric", a.string(numeric.name()).into()),
            ("numberingSystem", a.string(&nu).into()),
        ];
        for (k, v) in fields {
            Tags::put(a, o, k, v)?;
        }
        Ok(o.into())
    }

    pub(crate) fn supported_locales_of<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Tags::supported(a, args.at(0), args.at(1))
    }
}
