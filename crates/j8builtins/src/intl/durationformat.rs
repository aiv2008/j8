use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use fixed_decimal::Decimal;
use j8framework::{Error, ErrorKind, JsObject, ObjectData, Scope, Value, ValueKind};

use super::listformat::Lists;
use super::locale::{ExtKey, LocaleData, Options, Tags, choices};
use super::number::{NumberFormat, Numeric};
use super::service::{Hosted, LITERAL, Service};
use crate::Args;

choices! {
    BaseStyle { Long = "long", Short = "short", Narrow = "narrow", Digital = "digital" }
    UnitStyle { Long = "long", Short = "short", Narrow = "narrow", Numeric = "numeric", TwoDigit = "2-digit", Fractional = "fractional" }
    Display { Auto = "auto", Always = "always" }
}

const KEY_NUMBERING: usize = 0;
const FRACTION_MAX: u32 = 9;
const TIME_SEPARATOR: &str = ":";
const RECORD_LIMIT: f64 = 4_294_967_296.0;
const SECONDS_LIMIT: i128 = 9_007_199_254_740_992;
const NS_PER_SECOND: i128 = 1_000_000_000;

type UnitPart = (&'static str, String, Option<&'static str>);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Unit {
    Years,
    Months,
    Weeks,
    Days,
    Hours,
    Minutes,
    Seconds,
    Milliseconds,
    Microseconds,
    Nanoseconds,
}

impl Unit {
    const ALL: [Self; 10] = [Self::Years, Self::Months, Self::Weeks, Self::Days, Self::Hours, Self::Minutes, Self::Seconds, Self::Milliseconds, Self::Microseconds, Self::Nanoseconds];
    const ALPHABETICAL: [Self; 10] = [Self::Days, Self::Hours, Self::Microseconds, Self::Milliseconds, Self::Minutes, Self::Months, Self::Nanoseconds, Self::Seconds, Self::Weeks, Self::Years];

    const fn plural(self) -> &'static str {
        match self {
            Self::Years => "years",
            Self::Months => "months",
            Self::Weeks => "weeks",
            Self::Days => "days",
            Self::Hours => "hours",
            Self::Minutes => "minutes",
            Self::Seconds => "seconds",
            Self::Milliseconds => "milliseconds",
            Self::Microseconds => "microseconds",
            Self::Nanoseconds => "nanoseconds",
        }
    }

    const fn singular(self) -> &'static str {
        match self {
            Self::Years => "year",
            Self::Months => "month",
            Self::Weeks => "week",
            Self::Days => "day",
            Self::Hours => "hour",
            Self::Minutes => "minute",
            Self::Seconds => "second",
            Self::Milliseconds => "millisecond",
            Self::Microseconds => "microsecond",
            Self::Nanoseconds => "nanosecond",
        }
    }

    const fn display_option(self) -> &'static str {
        match self {
            Self::Years => "yearsDisplay",
            Self::Months => "monthsDisplay",
            Self::Weeks => "weeksDisplay",
            Self::Days => "daysDisplay",
            Self::Hours => "hoursDisplay",
            Self::Minutes => "minutesDisplay",
            Self::Seconds => "secondsDisplay",
            Self::Milliseconds => "millisecondsDisplay",
            Self::Microseconds => "microsecondsDisplay",
            Self::Nanoseconds => "nanosecondsDisplay",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }

    fn styles(self) -> &'static [UnitStyle] {
        match self {
            Self::Years | Self::Months | Self::Weeks | Self::Days => &[UnitStyle::Long, UnitStyle::Short, UnitStyle::Narrow],
            Self::Hours | Self::Minutes | Self::Seconds => &[UnitStyle::Long, UnitStyle::Short, UnitStyle::Narrow, UnitStyle::Numeric, UnitStyle::TwoDigit],
            Self::Milliseconds | Self::Microseconds | Self::Nanoseconds => &[UnitStyle::Long, UnitStyle::Short, UnitStyle::Narrow, UnitStyle::Numeric],
        }
    }

    const fn digital_base(self) -> UnitStyle {
        match self {
            Self::Years | Self::Months | Self::Weeks | Self::Days => UnitStyle::Short,
            _ => UnitStyle::Numeric,
        }
    }

    const fn subsecond(self) -> bool {
        matches!(self, Self::Milliseconds | Self::Microseconds | Self::Nanoseconds)
    }

    const fn minute_or_second(self) -> bool {
        matches!(self, Self::Minutes | Self::Seconds)
    }

    const fn nanos(self) -> i128 {
        match self {
            Self::Seconds => NS_PER_SECOND,
            Self::Milliseconds => 1_000_000,
            Self::Microseconds => 1_000,
            _ => 1,
        }
    }
}

impl From<BaseStyle> for UnitStyle {
    fn from(b: BaseStyle) -> Self {
        match b {
            BaseStyle::Long => Self::Long,
            BaseStyle::Short | BaseStyle::Digital => Self::Short,
            BaseStyle::Narrow => Self::Narrow,
        }
    }
}

struct DurationData;

impl LocaleData for DurationData {
    const KEYS: &'static [ExtKey] = &[ExtKey::Numbering];

    fn default_value(_: ExtKey, locale: &icu_locale_core::Locale) -> Option<String> {
        Some(NumberFormat::default_numbering(locale))
    }

    fn supports(_: ExtKey, value: &str, _: &icu_locale_core::Locale) -> bool {
        NumberFormat::numbering_systems().contains(&value)
    }
}

pub(crate) struct Durations {
    locale: String,
    numbering_system: String,
    style: BaseStyle,
    units: [(UnitStyle, Display); 10],
    fractional_digits: Option<u32>,
}

struct Record([f64; 10]);

impl Durations {
    fn unit_options<'scope>(a: &mut Scope<'scope, '_>, options: Options<'scope>, unit: Unit, base: BaseStyle, prev: Option<UnitStyle>) -> Result<(UnitStyle, Display), Error> {
        let given: Option<UnitStyle> = options.choice(a, unit.plural())?;
        if given.is_some_and(|s| !unit.styles().contains(&s)) {
            return Err(a.throw(ErrorKind::Range, &format!("Value out of range for options property {}", unit.plural())));
        }
        let mut display_default = Display::Always;
        let mut style = match given {
            Some(s) => s,
            None if base == BaseStyle::Digital => {
                if !matches!(unit, Unit::Hours | Unit::Minutes | Unit::Seconds) {
                    display_default = Display::Auto;
                }
                unit.digital_base()
            }
            None if matches!(prev, Some(UnitStyle::Fractional | UnitStyle::Numeric | UnitStyle::TwoDigit)) => {
                if !unit.minute_or_second() {
                    display_default = Display::Auto;
                }
                UnitStyle::Numeric
            }
            None => {
                display_default = Display::Auto;
                base.into()
            }
        };
        if style == UnitStyle::Numeric && unit.subsecond() {
            style = UnitStyle::Fractional;
            display_default = Display::Auto;
        }
        let given_display: Option<Display> = options.choice(a, unit.display_option())?;
        let display = given_display.unwrap_or(display_default);
        if style == UnitStyle::Fractional && given_display == Some(Display::Always) {
            return Err(a.throw(ErrorKind::Range, &format!("{} can't be displayed always with fractional style", unit.plural())));
        }
        if prev == Some(UnitStyle::Fractional) && style != UnitStyle::Fractional {
            return Err(a.throw(ErrorKind::Range, &format!("{} must be fractional after a fractional unit", unit.plural())));
        }
        if matches!(prev, Some(UnitStyle::Numeric | UnitStyle::TwoDigit)) {
            if !matches!(style, UnitStyle::Fractional | UnitStyle::Numeric | UnitStyle::TwoDigit) {
                return Err(a.throw(ErrorKind::Range, &format!("{} must be numeric after a numeric unit", unit.plural())));
            }
            if unit.minute_or_second() {
                style = UnitStyle::TwoDigit;
            }
        }
        Ok((style, display))
    }

    fn new<'scope>(a: &mut Scope<'scope, '_>, locales: Value<'scope>, options: Value<'scope>) -> Result<Self, Error> {
        let requested = Tags::list(a, locales)?;
        let options = Options::object(a, options)?;
        options.matcher(a)?;
        let numbering = options.text(a, "numberingSystem")?;
        if numbering.as_deref().is_some_and(|n| !NumberFormat::valid_numbering(n)) {
            return Err(a.throw(ErrorKind::Range, "Invalid numberingSystem"));
        }
        let resolved = Tags::resolve::<DurationData>(a, &requested, &[numbering]);
        let style = options.choice(a, "style")?.unwrap_or(BaseStyle::Short);
        let mut units = [(UnitStyle::Short, Display::Auto); 10];
        let mut prev = None;
        for unit in Unit::ALL {
            let r = Self::unit_options(a, options, unit, style, prev)?;
            prev = Some(r.0);
            units[unit.index()] = r;
        }
        let fractional_digits = options.number(a, "fractionalDigits", 0, FRACTION_MAX)?;
        let numbering_system = String::from(resolved.value(KEY_NUMBERING).unwrap_or("latn"));
        Ok(Self { locale: resolved.tag(), numbering_system, style, units, fractional_digits })
    }

    fn record<'scope>(a: &mut Scope<'scope, '_>, v: Value<'scope>) -> Result<Record, Error> {
        match v.kind() {
            ValueKind::Object(_) => {}
            ValueKind::String(_) => return Err(a.throw(ErrorKind::Range, "Duration strings are not supported")),
            _ => return Err(a.throw(ErrorKind::Type, "Duration must be an object")),
        }
        let mut fields = [0.0; 10];
        let mut any = false;
        for unit in Unit::ALPHABETICAL {
            let x = a.get_named(v, unit.plural())?;
            if matches!(x.kind(), ValueKind::Undefined) {
                continue;
            }
            any = true;
            let n = a.to_number(x)?;
            if !n.is_finite() || libm::trunc(n) != n {
                return Err(a.throw(ErrorKind::Range, &format!("{} must be an integer", unit.plural())));
            }
            fields[unit.index()] = n + 0.0;
        }
        if !any {
            return Err(a.throw(ErrorKind::Type, "Duration must have at least one unit"));
        }
        let r = Record(fields);
        if !r.valid() {
            return Err(a.throw(ErrorKind::Range, "Invalid duration"));
        }
        Ok(r)
    }

    fn options_object<'scope>(a: &mut Scope<'scope, '_>, fields: &[(&str, Value<'scope>)]) -> Result<Value<'scope>, Error> {
        let o = ObjectData { proto: None, ..ObjectData::default() }.alloc(a.mc);
        for &(k, v) in fields {
            Tags::put(a, o, k, v)?;
        }
        Ok(o.into())
    }

    fn partition<'scope>(&self, a: &mut Scope<'scope, '_>, r: &Record) -> Result<Vec<UnitPart>, Error> {
        let locale: Value<'scope> = a.string(&self.locale).into();
        let mut groups: Vec<Vec<UnitPart>> = Vec::new();
        let mut need_separator = false;
        let mut sign_pending = true;
        for unit in Unit::ALL {
            let (style, display) = self.units[unit.index()];
            let mut value = r.decimal(unit);
            let mut fields: Vec<(&str, Value<'scope>)> = Vec::new();
            let next_fractional = Unit::ALL.get(unit.index() + 1).is_some_and(|n| self.units[n.index()].0 == UnitStyle::Fractional);
            let done = matches!(unit, Unit::Seconds | Unit::Milliseconds | Unit::Microseconds) && next_fractional;
            if done {
                value = r.fraction(unit);
                let digits = self.fractional_digits;
                fields.push(("maximumFractionDigits", Value::from(f64::from(digits.unwrap_or(FRACTION_MAX)))));
                fields.push(("minimumFractionDigits", Value::from(f64::from(digits.unwrap_or(0)))));
                fields.push(("roundingMode", a.string("trunc").into()));
            }
            let display_required = unit == Unit::Minutes
                && need_separator
                && (self.units[Unit::Seconds.index()].1 == Display::Always || [Unit::Seconds, Unit::Milliseconds, Unit::Microseconds, Unit::Nanoseconds].iter().any(|u| r.0[u.index()] != 0.0));
            let zero = value.is_zero();
            if !zero || display != Display::Auto || display_required {
                if sign_pending {
                    sign_pending = false;
                    if zero && r.negative() {
                        value.sign = fixed_decimal::Sign::Negative;
                    }
                } else {
                    fields.push(("signDisplay", a.string("never").into()));
                }
                fields.push(("numberingSystem", a.string(&self.numbering_system).into()));
                if style == UnitStyle::TwoDigit {
                    fields.push(("minimumIntegerDigits", Value::from(2.0)));
                }
                let numeric = matches!(style, UnitStyle::Numeric | UnitStyle::TwoDigit | UnitStyle::Fractional);
                if numeric {
                    fields.push(("useGrouping", Value::from(false)));
                } else {
                    fields.push(("style", a.string("unit").into()));
                    fields.push(("unit", a.string(unit.singular()).into()));
                    fields.push(("unitDisplay", a.string(style.name()).into()));
                }
                let opts = Self::options_object(a, &fields)?;
                let nf = NumberFormat::new(a, locale, opts)?;
                let parts = nf.pieces(&Numeric::Finite(value));
                let mut list = if need_separator { groups.pop().unwrap_or_default() } else { Vec::new() };
                if need_separator {
                    list.push((LITERAL, String::from(TIME_SEPARATOR), None));
                }
                list.extend(parts.0.into_iter().map(|(k, t)| (k, t, Some(unit.singular()))));
                if !need_separator && numeric {
                    need_separator = true;
                }
                groups.push(list);
            }
            if done {
                break;
            }
        }
        let list_style = match self.style {
            BaseStyle::Digital => "short",
            s => s.name(),
        };
        let fields = [("type", a.string("unit").into()), ("style", a.string(list_style).into())];
        let opts = Self::options_object(a, &fields)?;
        let lists = Lists::new(a, locale, opts)?;
        let strings: Vec<String> = groups.iter().map(|g| g.iter().map(|(_, t, _)| t.as_str()).collect()).collect();
        let mut groups = groups.into_iter();
        let mut out = Vec::new();
        for (kind, text) in lists.pieces(&strings).0 {
            if kind == LITERAL {
                out.push((LITERAL, text, None));
            } else if let Some(g) = groups.next() {
                out.extend(g);
            }
        }
        Ok(out)
    }
}

impl Record {
    fn negative(&self) -> bool {
        self.0.iter().any(|&x| x < 0.0)
    }

    fn valid(&self) -> bool {
        let positive = self.0.iter().any(|&x| x > 0.0);
        if positive && self.negative() {
            return false;
        }
        if self.0[..Unit::Days.index()].iter().any(|x| x.abs() >= RECORD_LIMIT) {
            return false;
        }
        self.nanoseconds(Unit::Days).is_some_and(|ns| ns.abs() < SECONDS_LIMIT * NS_PER_SECOND)
    }

    #[expect(clippy::cast_possible_truncation, reason = "integral values bounded before the multiplication overflows")]
    fn exact(x: f64) -> Option<i128> {
        (x.abs() < 1e36).then_some(x as i128)
    }

    fn nanoseconds(&self, from: Unit) -> Option<i128> {
        const SCALE: [(Unit, i128); 7] = [
            (Unit::Days, 86_400 * NS_PER_SECOND),
            (Unit::Hours, 3_600 * NS_PER_SECOND),
            (Unit::Minutes, 60 * NS_PER_SECOND),
            (Unit::Seconds, NS_PER_SECOND),
            (Unit::Milliseconds, 1_000_000),
            (Unit::Microseconds, 1_000),
            (Unit::Nanoseconds, 1),
        ];
        let mut total: i128 = 0;
        for (u, scale) in SCALE {
            if u.index() < from.index() {
                continue;
            }
            total = total.checked_add(Self::exact(self.0[u.index()])?.checked_mul(scale)?)?;
        }
        Some(total)
    }

    fn decimal(&self, unit: Unit) -> Decimal {
        let x = self.0[unit.index()];
        Self::exact(x).and_then(|i| Decimal::try_from_str(&format!("{}", i.abs())).ok()).map_or_else(|| Decimal::from(0u8), |mut d| {
            if x < 0.0 {
                d.sign = fixed_decimal::Sign::Negative;
            }
            d
        })
    }

    fn fraction(&self, unit: Unit) -> Decimal {
        let ns = self.nanoseconds(unit).unwrap_or(0);
        let scale = unit.nanos();
        let (q, r) = (ns / scale, (ns % scale).abs());
        let width = format!("{}", scale - 1).len();
        let mut text = format!("{}{}", if ns < 0 { "-" } else { "" }, q.abs());
        if r != 0 {
            let digits = format!("{r:0width$}");
            text.push('.');
            text.push_str(digits.trim_end_matches('0'));
        }
        Decimal::try_from_str(&text).unwrap_or_else(|_| Decimal::from(0u8))
    }
}

pub(crate) struct DurationFormats;

impl DurationFormats {
    pub(crate) fn construct<'scope>(a: &mut Scope<'scope, '_>, ctor: JsObject<'scope>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        if matches!(a.new_target.kind(), ValueKind::Undefined) {
            return Err(a.throw(ErrorKind::Type, "Constructor Intl.DurationFormat requires 'new'"));
        }
        let proto = Hosted::proto(a, ctor, this);
        let d = Durations::new(a, args.at(0), args.at(1))?;
        Ok(Hosted::alloc(a, Service::Duration(Box::new(d)), proto).into())
    }

    fn settings<'scope>(a: &Scope<'scope, '_>, this: Value<'scope>, method: &str) -> Result<Durations, Error> {
        Hosted::with(a, this, |h| match &h.service {
            Service::Duration(d) => Some(Durations { locale: d.locale.clone(), numbering_system: d.numbering_system.clone(), style: d.style, units: d.units, fractional_digits: d.fractional_digits }),
            _ => None,
        })
        .ok_or_else(|| Hosted::incompatible(a, method))
    }

    pub(crate) fn format<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let d = Self::settings(a, this, "Intl.DurationFormat.prototype.format")?;
        let r = Durations::record(a, args.at(0))?;
        let parts = d.partition(a, &r)?;
        let text: String = parts.iter().map(|(_, t, _)| t.as_str()).collect();
        Ok(a.string(&text).into())
    }

    pub(crate) fn format_to_parts<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let d = Self::settings(a, this, "Intl.DurationFormat.prototype.formatToParts")?;
        let r = Durations::record(a, args.at(0))?;
        let parts = d.partition(a, &r)?;
        let mut items = Vec::with_capacity(parts.len());
        for (kind, text, unit) in parts {
            let o = a.object();
            let k = a.string(kind).into();
            Tags::put(a, o, "type", k)?;
            let t = a.string(&text).into();
            Tags::put(a, o, "value", t)?;
            if let Some(u) = unit {
                let u = a.string(u).into();
                Tags::put(a, o, "unit", u)?;
            }
            items.push(Value::from(o));
        }
        Ok(a.array_from_slice(&items).into())
    }

    pub(crate) fn resolved_options<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let d = Self::settings(a, this, "Intl.DurationFormat.prototype.resolvedOptions")?;
        let o = a.object();
        let mut fields: Vec<(&str, Value<'scope>)> = alloc::vec![
            ("locale", a.string(&d.locale).into()),
            ("numberingSystem", a.string(&d.numbering_system).into()),
            ("style", a.string(d.style.name()).into()),
        ];
        for unit in Unit::ALL {
            let (style, display) = d.units[unit.index()];
            let style = if style == UnitStyle::Fractional { UnitStyle::Numeric } else { style };
            fields.push((unit.plural(), a.string(style.name()).into()));
            fields.push((unit.display_option(), a.string(display.name()).into()));
        }
        if let Some(f) = d.fractional_digits {
            fields.push(("fractionalDigits", Value::from(f64::from(f))));
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
