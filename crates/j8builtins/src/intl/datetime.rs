use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use icu_calendar::{AnyCalendarKind, Date};
use icu_datetime::fieldbag::DateTimeFieldBag;
use icu_datetime::fieldbag::field;
use icu_datetime::fieldsets::builder::{DateFields, FieldSetBuilder, ZoneStyle};
use icu_datetime::fieldsets::enums::CompositeFieldSet;
use icu_datetime::options::{Length, TimePrecision};
use icu_datetime::preferences::{CalendarAlgorithm, HourCycle, NumberingSystem};
use icu_datetime::{DateTimeFormatter, DateTimeFormatterPreferences};
use icu_locale_core::Locale;
use icu_locale_core::extensions::unicode::{Key, Value as Keyword};
use icu_time::zone::{UtcOffset, ZoneNameTimestamp};
use icu_time::{Time, ZonedDateTime};
use j8framework::{Error, ErrorKind, JsObject, ObjectKind, Scope, Value, ValueKind};
use writeable::{Part, Writeable};

use super::locale::{ExtKey, LocaleData, Options, Tags, choices};
use super::number::NumberFormat;

use super::service::{Hosted, LITERAL, Pieces, Service, TARGET_SLOT};
use super::zone::Zone;
use crate::Args;
use crate::date::Civil;

choices! {
    Width { Narrow = "narrow", Short = "short", Long = "long" }
    Digits2 { Numeric = "numeric", TwoDigit = "2-digit" }
    MonthStyle { Numeric = "numeric", TwoDigit = "2-digit", Narrow = "narrow", Short = "short", Long = "long" }
    ZoneName { Short = "short", Long = "long", ShortOffset = "shortOffset", LongOffset = "longOffset", ShortGeneric = "shortGeneric", LongGeneric = "longGeneric" }
    Style { Full = "full", Long = "long", Medium = "medium", Short = "short" }
    Cycle { H11 = "h11", H12 = "h12", H23 = "h23", H24 = "h24" }
    FormatMatcher { Basic = "basic", BestFit = "best fit" }
}

const MS_PER_SECOND: f64 = 1000.0;
const NS_PER_MS: u32 = 1_000_000;
const MAX_TIME: f64 = 8.64e15;
const KEY_CALENDAR: usize = 0;
const KEY_CYCLE: usize = 1;
const KEY_NUMBERING: usize = 2;
const FRACTION_MAX: u32 = 3;
const CALENDARS: [&str; 16] = [
    "buddhist", "chinese", "coptic", "dangi", "ethioaa", "ethiopic", "gregory", "hebrew", "indian", "islamic-civil", "islamic-tbla", "islamic-umalqura", "iso8601", "japanese", "persian", "roc",
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Required {
    Date,
    Time,
    Any,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Defaults {
    Date,
    Time,
    All,
}

#[derive(Clone, Copy, Default)]
struct Components {
    weekday: Option<Width>,
    era: Option<Width>,
    year: Option<Digits2>,
    month: Option<MonthStyle>,
    day: Option<Digits2>,
    day_period: Option<Width>,
    hour: Option<Digits2>,
    minute: Option<Digits2>,
    second: Option<Digits2>,
    fraction: Option<u32>,
    zone_name: Option<ZoneName>,
}

impl Components {
    fn read<'scope>(a: &mut Scope<'scope, '_>, options: Options<'scope>) -> Result<Self, Error> {
        Ok(Self {
            weekday: options.choice(a, "weekday")?,
            era: options.choice(a, "era")?,
            year: options.choice(a, "year")?,
            month: options.choice(a, "month")?,
            day: options.choice(a, "day")?,
            day_period: options.choice(a, "dayPeriod")?,
            hour: options.choice(a, "hour")?,
            minute: options.choice(a, "minute")?,
            second: options.choice(a, "second")?,
            fraction: options.number(a, "fractionalSecondDigits", 1, FRACTION_MAX)?,
            zone_name: options.choice(a, "timeZoneName")?,
        })
    }

    fn any(&self) -> bool {
        self.has_date() || self.has_time() || self.era.is_some() || self.zone_name.is_some()
    }

    fn has_date(&self) -> bool {
        self.weekday.is_some() || self.year.is_some() || self.month.is_some() || self.day.is_some()
    }

    fn has_time(&self) -> bool {
        self.day_period.is_some() || self.hour.is_some() || self.minute.is_some() || self.second.is_some() || self.fraction.is_some()
    }

    fn defaults(&mut self, required: Required, defaults: Defaults) {
        let need = match required {
            Required::Date => !self.has_date(),
            Required::Time => !self.has_time(),
            Required::Any => !self.has_date() && !self.has_time(),
        };
        if need && matches!(defaults, Defaults::Date | Defaults::All) {
            (self.year, self.month, self.day) = (Some(Digits2::Numeric), Some(MonthStyle::Numeric), Some(Digits2::Numeric));
        }
        if need && matches!(defaults, Defaults::Time | Defaults::All) {
            (self.hour, self.minute, self.second) = (Some(Digits2::Numeric), Some(Digits2::Numeric), Some(Digits2::Numeric));
        }
    }

    fn bag(&self, clock12: Option<bool>) -> DateTimeFieldBag {
        let width = |w: Width| match w {
            Width::Narrow => (field::Weekday::Narrow, field::Era::Narrow, field::DayPeriod::FlexibleNarrow),
            Width::Short => (field::Weekday::Short, field::Era::Short, field::DayPeriod::FlexibleShort),
            Width::Long => (field::Weekday::Long, field::Era::Long, field::DayPeriod::FlexibleLong),
        };
        let two = |d: Digits2| d == Digits2::TwoDigit;
        let mut b = DateTimeFieldBag::default();
        b.weekday = self.weekday.map(|w| width(w).0);
        b.era = self.era.map(|w| width(w).1);
        b.year = self.year.map(|y| if two(y) { field::Year::TwoDigit } else { field::Year::Numeric });
        b.month = self.month.map(|m| match m {
            MonthStyle::Numeric => field::Month::Numeric,
            MonthStyle::TwoDigit => field::Month::TwoDigit,
            MonthStyle::Narrow => field::Month::Narrow,
            MonthStyle::Short => field::Month::Short,
            MonthStyle::Long => field::Month::Long,
        });
        b.day = self.day.map(|d| if two(d) { field::Day::TwoDigit } else { field::Day::Numeric });
        b.day_period = self.day_period.map(|w| width(w).2);
        b.hour = self.hour.map(|h| if two(h) { field::Hour::TwoDigit } else { field::Hour::Numeric });
        b.hour_kind = clock12.map(|c| if c { field::HourKind::Clock12 } else { field::HourKind::Clock24 });
        b.minute = self.minute.map(|m| if two(m) { field::Minute::TwoDigit } else { field::Minute::Numeric });
        b.second = self.second.map(|s| if two(s) { field::Second::TwoDigit } else { field::Second::Numeric });
        b.fractional_second_digits = self.fraction.map(|f| match f {
            1 => field::FractionalSecondDigits::F1,
            2 => field::FractionalSecondDigits::F2,
            _ => field::FractionalSecondDigits::F3,
        });
        b.time_zone_name = self.zone_name.map(ZoneName::field);
        b
    }

    fn from_pattern(pattern: &str) -> Self {
        let mut c = Self::default();
        let chars: Vec<char> = pattern.chars().collect();
        let mut i = 0;
        let mut quoted = false;
        while i < chars.len() {
            let ch = chars[i];
            let mut n = 1;
            while i + n < chars.len() && chars[i + n] == ch {
                n += 1;
            }
            i += n;
            if ch == '\'' {
                quoted ^= n % 2 == 1;
                continue;
            }
            if quoted {
                continue;
            }
            let digits = if n >= 2 { Digits2::TwoDigit } else { Digits2::Numeric };
            let width = match n {
                4 => Width::Long,
                5 => Width::Narrow,
                _ => Width::Short,
            };
            match ch {
                'G' => c.era = Some(width),
                'y' | 'Y' | 'u' | 'r' => c.year = Some(if n == 2 { Digits2::TwoDigit } else { Digits2::Numeric }),
                'M' | 'L' => {
                    c.month = Some(match n {
                        1 => MonthStyle::Numeric,
                        2 => MonthStyle::TwoDigit,
                        3 => MonthStyle::Short,
                        4 => MonthStyle::Long,
                        _ => MonthStyle::Narrow,
                    });
                }
                'd' => c.day = Some(digits),
                'E' | 'c' | 'e' => c.weekday = Some(width),
                'B' => c.day_period = Some(width),
                'h' | 'H' | 'K' | 'k' => c.hour = Some(digits),
                'm' => c.minute = Some(digits),
                's' => c.second = Some(digits),
                'S' => c.fraction = u32::try_from(n).ok(),
                'z' => c.zone_name = Some(if n >= 4 { ZoneName::Long } else { ZoneName::Short }),
                'O' => c.zone_name = Some(if n >= 4 { ZoneName::LongOffset } else { ZoneName::ShortOffset }),
                'v' => c.zone_name = Some(if n >= 4 { ZoneName::LongGeneric } else { ZoneName::ShortGeneric }),
                _ => {}
            }
        }
        c
    }
}

impl ZoneName {
    fn field(self) -> field::TimeZoneName {
        match self {
            Self::Short => field::TimeZoneName::ShortSpecific,
            Self::Long => field::TimeZoneName::LongSpecific,
            Self::ShortOffset => field::TimeZoneName::ShortOffset,
            Self::LongOffset => field::TimeZoneName::LongOffset,
            Self::ShortGeneric => field::TimeZoneName::ShortGeneric,
            Self::LongGeneric => field::TimeZoneName::LongGeneric,
        }
    }
}

impl Style {
    fn length(self) -> Length {
        match self {
            Self::Full | Self::Long => Length::Long,
            Self::Medium => Length::Medium,
            Self::Short => Length::Short,
        }
    }
}

impl From<Cycle> for HourCycle {
    fn from(c: Cycle) -> Self {
        match c {
            Cycle::H11 => Self::H11,
            Cycle::H12 => Self::H12,
            Cycle::H23 | Cycle::H24 => Self::H23,
        }
    }
}

#[derive(Clone)]
struct Settings {
    locale: String,
    calendar: String,
    numbering_system: String,
    time_zone: String,
    cycle: Option<Cycle>,
    components: Components,
    date_style: Option<Style>,
    time_style: Option<Style>,
}

pub(crate) struct DateTimeFormat {
    formatter: DateTimeFormatter<CompositeFieldSet>,
    zone: Zone,
    settings: Settings,
}

struct DateTimeData;

impl LocaleData for DateTimeData {
    const KEYS: &'static [ExtKey] = &[ExtKey::Calendar, ExtKey::HourCycle, ExtKey::Numbering];

    fn default_value(key: ExtKey, locale: &Locale) -> Option<String> {
        match key {
            ExtKey::Numbering => Some(NumberFormat::default_numbering(locale)),
            _ => None,
        }
    }

    fn supports(key: ExtKey, value: &str, _: &Locale) -> bool {
        match key {
            ExtKey::Calendar => CALENDARS.contains(&value),
            ExtKey::HourCycle => Cycle::try_from(value).is_ok(),
            ExtKey::Numbering => NumberFormat::numbering_systems().contains(&value),
            _ => false,
        }
    }
}

impl DateTimeFormat {
    pub(crate) fn calendars() -> &'static [&'static str] {
        &CALENDARS
    }

    fn calendar_name(kind: AnyCalendarKind) -> &'static str {
        match kind {
            AnyCalendarKind::Buddhist => "buddhist",
            AnyCalendarKind::Chinese => "chinese",
            AnyCalendarKind::Coptic => "coptic",
            AnyCalendarKind::Dangi => "dangi",
            AnyCalendarKind::Ethiopian => "ethiopic",
            AnyCalendarKind::EthiopianAmeteAlem => "ethioaa",
            AnyCalendarKind::Hebrew => "hebrew",
            AnyCalendarKind::Indian => "indian",
            AnyCalendarKind::HijriTabularTypeIIFriday => "islamic-civil",
            AnyCalendarKind::HijriTabularTypeIIThursday => "islamic-tbla",
            AnyCalendarKind::HijriUmmAlQura => "islamic-umalqura",
            AnyCalendarKind::Iso => "iso8601",
            AnyCalendarKind::Japanese => "japanese",
            AnyCalendarKind::Persian => "persian",
            AnyCalendarKind::Roc => "roc",
            _ => "gregory",
        }
    }

    pub(crate) fn new<'scope>(a: &mut Scope<'scope, '_>, locales: Value<'scope>, options: Value<'scope>, required: Required, defaults: Defaults) -> Result<Self, Error> {
        let requested = Tags::list(a, locales)?;
        let options = Options::coerce(a, options)?;
        options.matcher(a)?;
        let calendar = options.text(a, "calendar")?;
        let numbering = options.text(a, "numberingSystem")?;
        if calendar.iter().chain(numbering.iter()).any(|s| !NumberFormat::valid_numbering(s)) {
            return Err(a.throw(ErrorKind::Range, "Invalid calendar or numberingSystem"));
        }
        let hour12 = options.boolean(a, "hour12")?;
        let cycle_option: Option<Cycle> = options.choice(a, "hourCycle")?;
        let cycle_option = if hour12.is_some() { None } else { cycle_option.map(|c| String::from(c.name())) };
        let mut resolved = Tags::resolve::<DateTimeData>(a, &requested, &[calendar, cycle_option, numbering]);
        if hour12.is_some() {
            resolved.locale.extensions.unicode.keywords.remove(Key::from(ExtKey::HourCycle));
        }
        let zone_value = options.value(a, "timeZone")?;
        let zone = match zone_value.kind() {
            ValueKind::Undefined => Zone::host(a.platform.time_zone()),
            _ => {
                let name = a.to_string(zone_value)?;
                Zone::parse(name.as_str()).ok_or_else(|| a.throw(ErrorKind::Range, &alloc::format!("Invalid time zone specified: {}", name.as_str())))?
            }
        };
        let mut components = Components::read(a, options)?;
        options.choice::<FormatMatcher>(a, "formatMatcher")?;
        let date_style: Option<Style> = options.choice(a, "dateStyle")?;
        let time_style: Option<Style> = options.choice(a, "timeStyle")?;
        let styled = date_style.is_some() || time_style.is_some();
        if styled {
            if components.any() {
                return Err(a.throw(ErrorKind::Type, "Can't set option with dateStyle or timeStyle"));
            }
            if (required == Required::Date && time_style.is_some()) || (required == Required::Time && date_style.is_some()) {
                return Err(a.throw(ErrorKind::Type, "Invalid option for this method"));
            }
        } else {
            components.defaults(required, defaults);
        }
        let cycle = if hour12.is_some() { None } else { resolved.value(KEY_CYCLE).and_then(|c| Cycle::try_from(c).ok()) };
        let clock12 = hour12.or_else(|| cycle.map(|c| matches!(c, Cycle::H11 | Cycle::H12)));
        let fields = if styled { Self::styled(date_style, time_style) } else { components.bag(clock12).to_field_set_builder() };
        let fields = fields.build_composite().map_err(|_| a.throw(ErrorKind::Range, "Unsupported date-time format options"))?;
        let mut prefs = DateTimeFormatterPreferences::from(&Locale::from(resolved.locale.id.clone()));
        prefs.numbering_system = resolved.value(KEY_NUMBERING).and_then(|n| Keyword::try_from_str(n).ok()).and_then(|k| NumberingSystem::try_from(&k).ok());
        prefs.calendar_algorithm = resolved.value(KEY_CALENDAR).and_then(|c| Keyword::try_from_str(c).ok()).and_then(|k| CalendarAlgorithm::try_from(&k).ok());
        prefs.hour_cycle = match hour12 {
            Some(true) => Some(HourCycle::Clock12),
            Some(false) => Some(HourCycle::Clock24),
            None => cycle.map(HourCycle::from),
        };
        let formatter = DateTimeFormatter::try_new(prefs, fields).map_err(|_| a.throw(ErrorKind::Range, "Unsupported date-time format options"))?;
        let calendar = String::from(resolved.value(KEY_CALENDAR).unwrap_or_else(|| Self::calendar_name(formatter.calendar().kind())));
        let numbering_system = String::from(resolved.value(KEY_NUMBERING).unwrap_or("latn"));
        let settings = Settings { locale: resolved.tag(), calendar, numbering_system, time_zone: zone.identifier(), cycle: None, components: Components::default(), date_style, time_style };
        let mut dtf = Self { formatter, zone, settings };
        let pattern = dtf.pattern();
        let mut shown = Components::from_pattern(&pattern);
        if shown.hour.is_some() && shown.day_period.is_none() {
            shown.day_period = components.day_period;
        }
        dtf.settings.cycle = shown.hour.and_then(|_| Self::cycle_of(&pattern, cycle));
        dtf.settings.components = if styled { Components::default() } else { shown };
        Ok(dtf)
    }

    fn styled(date: Option<Style>, time: Option<Style>) -> FieldSetBuilder {
        let mut b = FieldSetBuilder::default();
        b.length = date.or(time).map(Style::length);
        b.date_fields = date.map(|d| if d == Style::Full { DateFields::YMDE } else { DateFields::YMD });
        b.time_precision = time.map(|t| if t == Style::Short { TimePrecision::Minute } else { TimePrecision::Second });
        b.zone_style = time.and_then(|t| match t {
            Style::Full => Some(ZoneStyle::SpecificLong),
            Style::Long => Some(ZoneStyle::SpecificShort),
            _ => None,
        });
        b
    }

    fn cycle_of(pattern: &str, requested: Option<Cycle>) -> Option<Cycle> {
        let mut quoted = false;
        for ch in pattern.chars() {
            match ch {
                '\'' => quoted = !quoted,
                _ if quoted => {}
                'h' => return Some(Cycle::H12),
                'K' => return Some(Cycle::H11),
                'k' => return Some(Cycle::H24),
                'H' => return Some(if requested == Some(Cycle::H24) { Cycle::H24 } else { Cycle::H23 }),
                _ => {}
            }
        }
        None
    }

    fn input(&self, t: f64) -> Option<ZonedDateTime<icu_calendar::Iso, icu_time::TimeZoneInfo<icu_time::zone::models::AtTime>>> {
        let offset = self.zone.offset_seconds(t);
        let c = Civil::from(t + f64::from(offset) * MS_PER_SECOND);
        let date = Date::try_new_iso(c.year, c.month, c.day).ok()?;
        let time = Time::try_new(c.hour, c.minute, c.second, u32::from(c.millisecond) * NS_PER_MS).ok()?;
        #[expect(clippy::cast_possible_truncation, reason = "time values are within ±8.64e15")]
        let seconds = libm::floor(t / MS_PER_SECOND) as i64;
        let zone = self.zone.id().with_offset(UtcOffset::try_from_seconds(offset).ok()).with_zone_name_timestamp(ZoneNameTimestamp::from_epoch_seconds(seconds));
        Some(ZonedDateTime { date, time, zone })
    }

    fn pattern(&self) -> String {
        self.input(0.0).map(|i| self.formatter.format(&i).pattern().write_to_string().into_owned()).unwrap_or_default()
    }

    fn part(parts: &[&Part]) -> &'static str {
        let Some(inner) = parts.iter().rev().find(|p| p.category == "datetime") else { return LITERAL };
        if inner.value == "second" {
            match parts.last() {
                Some(p) if p.category == "decimal" && p.value == "fraction" => return "fractionalSecond",
                Some(p) if p.category == "decimal" && p.value == "decimal" => return LITERAL,
                _ => {}
            }
        }
        inner.value
    }

    pub(crate) fn pieces(&self, t: f64) -> Pieces {
        self.input(t).map(|i| Pieces::collect(&self.formatter.format(&i), Self::part)).unwrap_or_default()
    }

    pub(crate) fn format(&self, t: f64) -> String {
        self.pieces(t).text()
    }
}

impl Settings {
    fn resolved<'scope>(&self, a: &mut Scope<'scope, '_>) -> Result<Value<'scope>, Error> {
        let o = a.object();
        let text = |a: &mut Scope<'scope, '_>, s: &str| -> Value<'scope> { a.string(s).into() };
        let mut fields: Vec<(&str, Value<'scope>)> = alloc::vec![
            ("locale", text(a, &self.locale)),
            ("calendar", text(a, &self.calendar)),
            ("numberingSystem", text(a, &self.numbering_system)),
            ("timeZone", text(a, &self.time_zone)),
        ];
        if let Some(c) = self.cycle {
            fields.push(("hourCycle", text(a, c.name())));
            fields.push(("hour12", matches!(c, Cycle::H11 | Cycle::H12).into()));
        }
        let c = &self.components;
        let named: [(&str, Option<&str>); 9] = [
            ("weekday", c.weekday.map(Width::name)),
            ("era", c.era.map(Width::name)),
            ("year", c.year.map(Digits2::name)),
            ("month", c.month.map(MonthStyle::name)),
            ("day", c.day.map(Digits2::name)),
            ("dayPeriod", c.day_period.map(Width::name)),
            ("hour", c.hour.map(Digits2::name)),
            ("minute", c.minute.map(Digits2::name)),
            ("second", c.second.map(Digits2::name)),
        ];
        for (k, v) in named {
            if let Some(v) = v {
                fields.push((k, text(a, v)));
            }
        }
        if let Some(f) = c.fraction {
            fields.push(("fractionalSecondDigits", f64::from(f).into()));
        }
        if let Some(z) = c.zone_name {
            fields.push(("timeZoneName", text(a, z.name())));
        }
        if let Some(d) = self.date_style {
            fields.push(("dateStyle", text(a, d.name())));
        }
        if let Some(t) = self.time_style {
            fields.push(("timeStyle", text(a, t.name())));
        }
        for (k, v) in fields {
            Tags::put(a, o, k, v)?;
        }
        Ok(o.into())
    }
}

pub(crate) struct DateTimeFormats;

impl DateTimeFormats {
    pub(crate) fn construct<'scope>(a: &mut Scope<'scope, '_>, ctor: JsObject<'scope>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let proto = Hosted::proto(a, ctor, this);
        let dtf = DateTimeFormat::new(a, args.at(0), args.at(1), Required::Any, Defaults::Date)?;
        Ok(Hosted::alloc(a, Service::DateTime(Box::new(dtf)), proto).into())
    }

    fn with<'scope, R>(a: &Scope<'scope, '_>, this: Value<'scope>, method: &str, f: impl FnOnce(&DateTimeFormat) -> R) -> Result<R, Error> {
        Hosted::with(a, this, |h| match &h.service {
            Service::DateTime(d) => Some(f(d)),
            _ => None,
        })
        .ok_or_else(|| Hosted::incompatible(a, method))
    }

    fn time<'scope>(a: &mut Scope<'scope, '_>, date: Value<'scope>) -> Result<f64, Error> {
        let t = match date.kind() {
            ValueKind::Undefined => {
                #[expect(clippy::cast_precision_loss, reason = "millisecond count is far below 2^53")]
                let now = a.platform.wall_clock().as_millis() as f64;
                now
            }
            _ => a.to_number(date)?,
        };
        if !t.is_finite() || t.abs() > MAX_TIME {
            return Err(a.throw(ErrorKind::Range, "Invalid time value"));
        }
        Ok(libm::trunc(t) + 0.0)
    }

    fn format_bound<'scope>(a: &mut Scope<'scope, '_>, f: JsObject<'scope>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let target = Hosted::slot(f, TARGET_SLOT).unwrap_or_default();
        let t = Self::time(a, args.at(0))?;
        let s = Self::with(a, target, "format", |d| d.format(t))?;
        Ok(a.string(&s).into())
    }

    pub(crate) fn format<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::with(a, this, "get Intl.DateTimeFormat.prototype.format", |_| ())?;
        Hosted::bound(a, this, Self::format_bound, 1).map(Value::from).ok_or_else(|| Hosted::incompatible(a, "format"))
    }

    pub(crate) fn format_to_parts<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::with(a, this, "Intl.DateTimeFormat.prototype.formatToParts", |_| ())?;
        let t = Self::time(a, args.at(0))?;
        let pieces = Self::with(a, this, "formatToParts", |d| d.pieces(t))?;
        pieces.array(a)
    }

    pub(crate) fn resolved_options<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let settings = Self::with(a, this, "Intl.DateTimeFormat.prototype.resolvedOptions", |d| d.settings.clone())?;
        settings.resolved(a)
    }

    pub(crate) fn supported_locales_of<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Tags::supported(a, args.at(0), args.at(1))
    }

    fn this_time<'scope>(a: &Scope<'scope, '_>, this: Value<'scope>) -> Result<f64, Error> {
        match this.as_object().map(|o| match o.borrow().kind {
            ObjectKind::Date(t) => Some(t),
            _ => None,
        }) {
            Some(Some(t)) => Ok(t),
            _ => Err(a.throw(ErrorKind::Type, "this is not a Date object.")),
        }
    }

    fn date_text<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>], required: Required, defaults: Defaults) -> Result<Value<'scope>, Error> {
        let t = Self::this_time(a, this)?;
        if t.is_nan() {
            return Ok(a.string("Invalid Date").into());
        }
        let dtf = DateTimeFormat::new(a, args.at(0), args.at(1), required, defaults)?;
        Ok(a.string(&dtf.format(t)).into())
    }

    pub(crate) fn to_locale_string<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::date_text(a, this, args, Required::Any, Defaults::All)
    }

    pub(crate) fn to_locale_date_string<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::date_text(a, this, args, Required::Date, Defaults::Date)
    }

    pub(crate) fn to_locale_time_string<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::date_text(a, this, args, Required::Time, Defaults::Time)
    }
}

impl DateTimeFormat {
    pub(crate) fn preferred_cycle(locale: &Locale) -> Option<&'static str> {
        let f = icu_datetime::NoCalendarFormatter::try_new(DateTimeFormatterPreferences::from(locale), icu_datetime::fieldsets::T::hm()).ok()?;
        let afternoon = Time::try_new(PROBE_HOUR, 0, 0, 0).ok()?;
        let pattern = f.format(&afternoon).pattern().write_to_string().into_owned();
        Self::cycle_of(&pattern, None).map(Cycle::name)
    }
}

const PROBE_HOUR: u8 = 13;
