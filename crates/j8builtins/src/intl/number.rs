use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};

use fixed_decimal::{Decimal, FloatPrecision, RoundingIncrement, Sign, SignDisplay as FixedSign, SignedRoundingMode, UnsignedRoundingMode};
use icu_decimal::options::{CompactDecimalFormatterOptions, DecimalFormatterOptions, GroupingStrategy};
use icu_decimal::preferences::CompactDecimalFormatterPreferences;
use icu_decimal::{CompactDecimalFormatter, DecimalFormatter, DecimalFormatterPreferences};
use icu_locale_core::Locale;
use icu_locale_core::extensions::unicode::Value as Keyword;
use icu_locale_core::preferences::extensions::unicode::keywords::NumberingSystem;
use icu_provider::prelude::*;
use j8framework::{Error, ErrorKind, Hint, JsObject, Scope, Value, ValueKind};
use writeable::Part;

use super::locale::{ExtKey, LocaleData, Options, Tags, choices};

use super::service::{Hosted, LITERAL, Pieces, Service, TARGET_SLOT};
use crate::Args;

choices! {
    Style { Decimal = "decimal", Percent = "percent", Currency = "currency", Unit = "unit" }
    CurrencyDisplay { Code = "code", Symbol = "symbol", NarrowSymbol = "narrowSymbol", Name = "name" }
    CurrencySign { Standard = "standard", Accounting = "accounting" }
    UnitDisplay { Short = "short", Narrow = "narrow", Long = "long" }
    Notation { Standard = "standard", Scientific = "scientific", Engineering = "engineering", Compact = "compact" }
    CompactDisplay { Short = "short", Long = "long" }
    RoundingMode {
        Ceil = "ceil", Floor = "floor", Expand = "expand", Trunc = "trunc",
        HalfCeil = "halfCeil", HalfFloor = "halfFloor", HalfExpand = "halfExpand", HalfTrunc = "halfTrunc", HalfEven = "halfEven",
    }
    RoundingPriority { Auto = "auto", MorePrecision = "morePrecision", LessPrecision = "lessPrecision" }
    TrailingZeroDisplay { Auto = "auto", StripIfInteger = "stripIfInteger" }
    SignDisplay { Auto = "auto", Never = "never", Always = "always", ExceptZero = "exceptZero", Negative = "negative" }
    Grouping { Min2 = "min2", Auto = "auto", Always = "always", True = "true", False = "false" }
}

const NAN: &str = "NaN";
const INFINITY: &str = "∞";
const EXPONENT: &str = "E";
const PERCENT: &str = "%";
const AFFIX_SPACE: &str = "\u{a0}";
const UNIT_MARK: &str = "\u{0}";
const PERCENT_SHIFT: i16 = 2;
const ENGINEERING_STEP: i16 = 3;

const INTEGER_DIGITS_MAX: u32 = 21;
const FRACTION_DIGITS_MAX: u32 = 100;
const SIGNIFICANT_DIGITS_MAX: u32 = 21;
const INCREMENT_MAX: u32 = 5000;
const INCREMENTS: [u32; 15] = [1, 2, 5, 10, 20, 25, 50, 100, 200, 250, 500, 1000, 2000, 2500, 5000];
const CURRENCY_DIGITS: u32 = 2;
const CURRENCY_EXCEPTIONS: [(&str, u32); 26] = [
    ("BHD", 3), ("BIF", 0), ("CLF", 4), ("CLP", 0), ("DJF", 0), ("GNF", 0), ("IQD", 3), ("ISK", 0), ("JOD", 3), ("JPY", 0), ("KMF", 0), ("KRW", 0),
    ("KWD", 3), ("LYD", 3), ("OMR", 3), ("PYG", 0), ("RWF", 0), ("TND", 3), ("UGX", 0), ("UYI", 0), ("UYW", 4), ("VND", 0), ("VUV", 0), ("XAF", 0),
    ("XOF", 0), ("XPF", 0),
];
const SANCTIONED_UNITS: [&str; 45] = [
    "acre", "bit", "byte", "celsius", "centimeter", "day", "degree", "fahrenheit", "fluid-ounce", "foot", "gallon", "gigabit", "gigabyte", "gram",
    "hectare", "hour", "inch", "kilobit", "kilobyte", "kilogram", "kilometer", "liter", "megabit", "megabyte", "meter", "microsecond", "mile",
    "mile-scandinavian", "milliliter", "millimeter", "millisecond", "minute", "month", "nanosecond", "ounce", "percent", "petabyte", "pound",
    "second", "stone", "terabit", "terabyte", "week", "yard", "year",
];
const DECIMAL_MAX_FRACTION: u32 = 3;
const COMPACT_MAX_SIGNIFICANT: u32 = 2;

const NUMBERING_SYSTEMS: [&str; 26] = [
    "adlm", "arab", "arabext", "beng", "cakm", "deva", "fullwide", "gujr", "guru", "hanidec", "khmr", "knda", "laoo", "latn", "mlym", "mong", "mtei",
    "mymr", "nkoo", "olck", "orya", "tamldec", "telu", "thai", "tibt", "vaii",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum RoundingType {
    Fraction,
    Significant,
    More,
    Less,
}

impl From<RoundingMode> for SignedRoundingMode {
    fn from(m: RoundingMode) -> Self {
        match m {
            RoundingMode::Ceil => Self::Ceil,
            RoundingMode::Floor => Self::Floor,
            RoundingMode::Expand => Self::Unsigned(UnsignedRoundingMode::Expand),
            RoundingMode::Trunc => Self::Unsigned(UnsignedRoundingMode::Trunc),
            RoundingMode::HalfCeil => Self::HalfCeil,
            RoundingMode::HalfFloor => Self::HalfFloor,
            RoundingMode::HalfExpand => Self::Unsigned(UnsignedRoundingMode::HalfExpand),
            RoundingMode::HalfTrunc => Self::Unsigned(UnsignedRoundingMode::HalfTrunc),
            RoundingMode::HalfEven => Self::Unsigned(UnsignedRoundingMode::HalfEven),
        }
    }
}

impl From<SignDisplay> for FixedSign {
    fn from(s: SignDisplay) -> Self {
        match s {
            SignDisplay::Auto => Self::Auto,
            SignDisplay::Never => Self::Never,
            SignDisplay::Always => Self::Always,
            SignDisplay::ExceptZero => Self::ExceptZero,
            SignDisplay::Negative => Self::Negative,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Digits {
    min_integer: u32,
    min_fraction: u32,
    max_fraction: u32,
    min_significant: u32,
    max_significant: u32,
    rounding: RoundingType,
    priority: RoundingPriority,
    increment: u32,
    mode: RoundingMode,
    trailing_zeros: TrailingZeroDisplay,
}

struct Rounded {
    value: Decimal,
    magnitude: i16,
}

impl Digits {
    pub(crate) fn read<'scope>(a: &mut Scope<'scope, '_>, options: Options<'scope>, min_default: u32, max_default: u32, notation: Notation) -> Result<Self, Error> {
        let min_integer = options.number(a, "minimumIntegerDigits", 1, INTEGER_DIGITS_MAX)?.unwrap_or(1);
        let mnfd = options.value(a, "minimumFractionDigits")?;
        let mxfd = options.value(a, "maximumFractionDigits")?;
        let mnsd = options.value(a, "minimumSignificantDigits")?;
        let mxsd = options.value(a, "maximumSignificantDigits")?;
        let increment = options.number(a, "roundingIncrement", 1, INCREMENT_MAX)?.unwrap_or(1);
        if !INCREMENTS.contains(&increment) {
            return Err(a.throw(ErrorKind::Range, "roundingIncrement value is out of range."));
        }
        let mode = options.choice(a, "roundingMode")?.unwrap_or(RoundingMode::HalfExpand);
        let priority = options.choice(a, "roundingPriority")?.unwrap_or(RoundingPriority::Auto);
        let trailing_zeros = options.choice(a, "trailingZeroDisplay")?.unwrap_or(TrailingZeroDisplay::Auto);
        let max_default = if increment == 1 { max_default } else { min_default };
        let undefined = |v: Value<'_>| matches!(v.kind(), ValueKind::Undefined);
        let has_sd = !undefined(mnsd) || !undefined(mxsd);
        let has_fd = !undefined(mnfd) || !undefined(mxfd);
        let (mut need_sd, mut need_fd) = (true, true);
        if priority == RoundingPriority::Auto {
            need_sd = has_sd;
            if need_sd || (!has_fd && notation == Notation::Compact) {
                need_fd = false;
            }
        }
        let (min_fraction, max_fraction, min_significant, max_significant) = (0, 0, 1, SIGNIFICANT_DIGITS_MAX);
        let mut d = Self { min_integer, min_fraction, max_fraction, min_significant, max_significant, rounding: RoundingType::Fraction, priority, increment, mode, trailing_zeros };
        if need_sd && has_sd {
            d.min_significant = Options::range(a, mnsd, "minimumSignificantDigits", 1, SIGNIFICANT_DIGITS_MAX)?.unwrap_or(1);
            d.max_significant = Options::range(a, mxsd, "maximumSignificantDigits", d.min_significant, SIGNIFICANT_DIGITS_MAX)?.unwrap_or(SIGNIFICANT_DIGITS_MAX);
        }
        if need_fd {
            if has_fd {
                let min = Options::range(a, mnfd, "minimumFractionDigits", 0, FRACTION_DIGITS_MAX)?;
                let max = Options::range(a, mxfd, "maximumFractionDigits", 0, FRACTION_DIGITS_MAX)?;
                (d.min_fraction, d.max_fraction) = match (min, max) {
                    (None, Some(max)) => (min_default.min(max), max),
                    (Some(min), None) => (min, max_default.max(min)),
                    (Some(min), Some(max)) if min > max => return Err(a.throw(ErrorKind::Range, "maximumFractionDigits value is out of range.")),
                    (min, max) => (min.unwrap_or(min_default), max.unwrap_or(max_default)),
                };
            } else {
                (d.min_fraction, d.max_fraction) = (min_default, max_default);
            }
        }
        d.rounding = match (need_sd, need_fd, priority) {
            (false, false, _) => {
                (d.min_fraction, d.max_fraction, d.min_significant, d.max_significant) = (0, 0, 1, COMPACT_MAX_SIGNIFICANT);
                d.priority = RoundingPriority::MorePrecision;
                RoundingType::More
            }
            (true, _, RoundingPriority::Auto) => RoundingType::Significant,
            (false, _, RoundingPriority::Auto) => RoundingType::Fraction,
            (_, _, RoundingPriority::MorePrecision) => RoundingType::More,
            (_, _, RoundingPriority::LessPrecision) => RoundingType::Less,
        };
        if increment != 1 {
            if d.rounding != RoundingType::Fraction {
                return Err(a.throw(ErrorKind::Type, "roundingIncrement requires fraction digit rounding"));
            }
            if d.max_fraction != d.min_fraction {
                return Err(a.throw(ErrorKind::Range, "roundingIncrement requires equal minimum and maximum fraction digits"));
            }
        }
        Ok(d)
    }

    pub(crate) fn precision_fields(&self, fields: &mut alloc::vec::Vec<(&str, Value<'_>)>) {
        fields.push(("minimumIntegerDigits", f64::from(self.min_integer).into()));
        if matches!(self.rounding, RoundingType::Fraction | RoundingType::More | RoundingType::Less) {
            fields.push(("minimumFractionDigits", f64::from(self.min_fraction).into()));
            fields.push(("maximumFractionDigits", f64::from(self.max_fraction).into()));
        }
        if matches!(self.rounding, RoundingType::Significant | RoundingType::More | RoundingType::Less) {
            fields.push(("minimumSignificantDigits", f64::from(self.min_significant).into()));
            fields.push(("maximumSignificantDigits", f64::from(self.max_significant).into()));
        }
    }

    pub(crate) fn rounding_fields<'scope>(&self, a: &Scope<'scope, '_>, fields: &mut alloc::vec::Vec<(&str, Value<'scope>)>) {
        fields.push(("roundingIncrement", f64::from(self.increment).into()));
        fields.push(("roundingMode", a.string(self.mode.name()).into()));
        fields.push(("roundingPriority", a.string(self.priority.name()).into()));
        fields.push(("trailingZeroDisplay", a.string(self.trailing_zeros.name()).into()));
    }

    fn position(n: u32) -> i16 {
        i16::try_from(n).unwrap_or(i16::MAX)
    }

    fn increment(&self) -> (i16, RoundingIncrement) {
        let (mut n, mut shift) = (self.increment, 0);
        while n.is_multiple_of(10) {
            n /= 10;
            shift += 1;
        }
        let step = match n {
            2 => RoundingIncrement::MultiplesOf2,
            5 => RoundingIncrement::MultiplesOf5,
            25 => RoundingIncrement::MultiplesOf25,
            _ => RoundingIncrement::MultiplesOf1,
        };
        (shift, step)
    }

    fn precision(&self, x: &Decimal) -> Rounded {
        let start = x.absolute.nonzero_magnitude_start();
        let mut value = x.clone();
        let position = start - Self::position(self.max_significant) + 1;
        value.round_with_mode(position, self.mode.into());
        let after = value.absolute.nonzero_magnitude_start();
        let magnitude = if value.absolute.is_zero() { position } else { after - Self::position(self.max_significant) + 1 };
        value.absolute.pad_end((after - Self::position(self.min_significant) + 1).min(0));
        Rounded { value, magnitude }
    }

    fn fixed(&self, x: &Decimal) -> Rounded {
        let mut value = x.clone();
        let position = -Self::position(self.max_fraction);
        if self.increment == 1 {
            value.round_with_mode(position, self.mode.into());
        } else {
            let (shift, step) = self.increment();
            value.round_with_mode_and_increment(position + shift, self.mode.into(), step);
        }
        value.absolute.pad_end(-Self::position(self.min_fraction));
        Rounded { value, magnitude: position }
    }

    fn round(&self, x: &Decimal) -> Decimal {
        let mut r = match self.rounding {
            RoundingType::Significant => self.precision(x).value,
            RoundingType::Fraction => self.fixed(x).value,
            RoundingType::More | RoundingType::Less => {
                let (s, f) = (self.precision(x), self.fixed(x));
                let significant_wins = (s.magnitude <= f.magnitude) == (self.rounding == RoundingType::More);
                if significant_wins { s.value } else { f.value }
            }
        };
        if self.trailing_zeros == TrailingZeroDisplay::StripIfInteger && r.absolute.nonzero_magnitude_end() >= 0 {
            r.absolute.trim_end();
        }
        r.absolute.pad_start(Self::position(self.min_integer));
        r
    }
}

pub(crate) enum Numeric {
    Finite(Decimal),
    NotANumber,
    Infinite(bool),
}

impl From<f64> for Numeric {
    fn from(x: f64) -> Self {
        if x.is_nan() {
            Self::NotANumber
        } else if x.is_infinite() {
            Self::Infinite(x < 0.0)
        } else {
            Decimal::try_from_f64(x, FloatPrecision::RoundTrip).map_or(Self::NotANumber, Self::Finite)
        }
    }
}

impl Numeric {
    pub(crate) fn read<'scope>(a: &mut Scope<'scope, '_>, v: Value<'scope>) -> Result<Self, Error> {
        let p = a.to_primitive(v, Hint::Number)?;
        match p.kind() {
            ValueKind::BigInt(b) => Ok(Decimal::try_from_str(&b.0.to_str_radix(10)).map_or(Self::NotANumber, Self::Finite)),
            ValueKind::String(s) => {
                let t = s.trim();
                let exact = if t.is_empty() { Some(Decimal::from(0u8)) } else { Decimal::try_from_str(t).ok() };
                match exact {
                    Some(d) => Ok(Self::Finite(d)),
                    None => Ok(Self::from(a.to_number(p)?)),
                }
            }
            _ => Ok(Self::from(a.to_number(p)?)),
        }
    }
}

#[derive(Clone)]
struct Settings {
    locale: String,
    numbering_system: String,
    style: Style,
    currency: Option<(String, CurrencyDisplay, CurrencySign)>,
    unit: Option<(String, UnitDisplay)>,
    notation: Notation,
    compact_display: Option<CompactDisplay>,
    digits: Digits,
    grouping: Option<Grouping>,
    sign_display: SignDisplay,
}

pub(crate) struct NumberFormat {
    decimal: DecimalFormatter,
    exponent: DecimalFormatter,
    compact: Option<CompactDecimalFormatter>,
    settings: Settings,
}

struct NumberData;

impl LocaleData for NumberData {
    const KEYS: &'static [ExtKey] = &[ExtKey::Numbering];

    fn default_value(_: ExtKey, locale: &Locale) -> Option<String> {
        Some(NumberFormat::default_numbering(locale))
    }

    fn supports(_: ExtKey, value: &str, _: &Locale) -> bool {
        NUMBERING_SYSTEMS.contains(&value)
    }
}

impl NumberFormat {
    pub(crate) fn numbering_systems() -> &'static [&'static str] {
        &NUMBERING_SYSTEMS
    }

    pub(crate) fn units() -> &'static [&'static str] {
        &SANCTIONED_UNITS
    }

    pub(crate) fn default_numbering(locale: &Locale) -> String {
        let data_locale = DataLocale::from(&locale.id);
        let req = DataRequest { id: DataIdentifierBorrowed::for_locale(&data_locale), ..DataRequest::default() };
        DataProvider::<icu_decimal::provider::DecimalSymbolsV1>::load(&icu_decimal::provider::Baked, req).map_or_else(|_| String::from("latn"), |r| String::from(r.payload.get().numsys()))
    }

    pub(crate) fn valid_numbering(s: &str) -> bool {
        super::collator::Collation::valid_type(s)
    }

    fn well_formed_currency(c: &str) -> bool {
        c.len() == 3 && c.bytes().all(|b| b.is_ascii_alphabetic())
    }

    fn well_formed_unit(u: &str) -> bool {
        let sanctioned = |s: &str| SANCTIONED_UNITS.contains(&s);
        match u.split_once("-per-") {
            Some((n, d)) => sanctioned(n) && sanctioned(d),
            None => sanctioned(u),
        }
    }

    fn currency_digits(code: &str) -> u32 {
        CURRENCY_EXCEPTIONS.iter().find(|(c, _)| *c == code).map_or(CURRENCY_DIGITS, |&(_, d)| d)
    }

    pub(crate) fn new<'scope>(a: &mut Scope<'scope, '_>, locales: Value<'scope>, options: Value<'scope>) -> Result<Self, Error> {
        let requested = Tags::list(a, locales)?;
        let options = Options::coerce(a, options)?;
        Self::with_options(a, &requested, options)
    }

    fn with_options<'scope>(a: &mut Scope<'scope, '_>, requested: &[Locale], options: Options<'scope>) -> Result<Self, Error> {
        options.matcher(a)?;
        let nu = options.text(a, "numberingSystem")?;
        if nu.as_deref().is_some_and(|n| !Self::valid_numbering(n)) {
            return Err(a.throw(ErrorKind::Range, "Invalid numberingSystem"));
        }
        let resolved = Tags::resolve::<NumberData>(a, requested, &[nu]);
        let style = options.choice(a, "style")?.unwrap_or(Style::Decimal);
        let currency = options.text(a, "currency")?;
        match &currency {
            None if style == Style::Currency => return Err(a.throw(ErrorKind::Type, "Currency code is required with currency style.")),
            Some(c) if !Self::well_formed_currency(c) => return Err(a.throw(ErrorKind::Range, &format!("Invalid currency code : {c}"))),
            _ => {}
        }
        let currency_display = options.choice(a, "currencyDisplay")?.unwrap_or(CurrencyDisplay::Symbol);
        let currency_sign = options.choice(a, "currencySign")?.unwrap_or(CurrencySign::Standard);
        let unit = options.text(a, "unit")?;
        match &unit {
            None if style == Style::Unit => return Err(a.throw(ErrorKind::Type, "Unit is required with unit style.")),
            Some(u) if !Self::well_formed_unit(u) => return Err(a.throw(ErrorKind::Range, &format!("Invalid unit argument '{u}'"))),
            _ => {}
        }
        let unit_display = options.choice(a, "unitDisplay")?.unwrap_or(UnitDisplay::Short);
        let currency = currency.filter(|_| style == Style::Currency).map(|c| (c.to_ascii_uppercase(), currency_display, currency_sign));
        let unit = unit.filter(|_| style == Style::Unit).map(|u| (u, unit_display));
        let notation = options.choice(a, "notation")?.unwrap_or(Notation::Standard);
        let (min_default, max_default) = match (style, &currency) {
            (Style::Currency, Some((code, ..))) if notation == Notation::Standard => {
                let d = Self::currency_digits(code);
                (d, d)
            }
            (Style::Percent, _) => (0, 0),
            _ => (0, DECIMAL_MAX_FRACTION),
        };
        let digits = Digits::read(a, options, min_default, max_default, notation)?;
        let compact_display: CompactDisplay = options.choice(a, "compactDisplay")?.unwrap_or(CompactDisplay::Short);
        let default_grouping = if notation == Notation::Compact { Grouping::Min2 } else { Grouping::Auto };
        let grouping = Self::grouping(a, options, default_grouping)?;
        let sign_display = options.choice(a, "signDisplay")?.unwrap_or(SignDisplay::Auto);
        let numbering_system = String::from(resolved.value(0).unwrap_or("latn"));
        let compact_display = (notation == Notation::Compact).then_some(compact_display);
        let settings = Settings { locale: resolved.tag(), numbering_system, style, currency, unit, notation, compact_display, digits, grouping, sign_display };
        Self::build(a, &resolved.locale, settings)
    }

    pub(crate) fn rounding(a: &Scope<'_, '_>, locale: &Locale, digits: Digits, notation: Notation, compact_display: CompactDisplay) -> Result<Self, Error> {
        let settings = Settings {
            locale: locale.to_string(),
            numbering_system: Self::default_numbering(locale),
            style: Style::Decimal,
            currency: None,
            unit: None,
            notation,
            compact_display: (notation == Notation::Compact).then_some(compact_display),
            digits,
            grouping: Some(Grouping::Auto),
            sign_display: SignDisplay::Auto,
        };
        Self::build(a, locale, settings)
    }

    pub(crate) fn rounded(&self, x: &Decimal) -> (Decimal, i16) {
        self.scaled(x)
    }

    fn grouping<'scope>(a: &mut Scope<'scope, '_>, options: Options<'scope>, fallback: Grouping) -> Result<Option<Grouping>, Error> {
        let v = options.value(a, "useGrouping")?;
        match v.kind() {
            ValueKind::Undefined => return Ok(Some(fallback)),
            ValueKind::Bool(true) => return Ok(Some(Grouping::Always)),
            _ if !a.to_boolean(v) => return Ok(None),
            _ => {}
        }
        let s = a.to_string(v)?;
        match Grouping::try_from(s.as_str()) {
            Ok(Grouping::True | Grouping::False) => Ok(Some(fallback)),
            Ok(g) => Ok(Some(g)),
            Err(()) => Err(a.throw(ErrorKind::Range, &format!("Value {} out of range for options property useGrouping", s.as_str()))),
        }
    }

    fn build(a: &Scope<'_, '_>, locale: &Locale, settings: Settings) -> Result<Self, Error> {
        let strategy = match settings.grouping {
            None => GroupingStrategy::Never,
            Some(Grouping::Always) => GroupingStrategy::Always,
            Some(Grouping::Min2) => GroupingStrategy::Min2,
            Some(_) => GroupingStrategy::Auto,
        };
        let mut prefs = DecimalFormatterPreferences::from(&Locale::from(locale.id.clone()));
        prefs.numbering_system = Keyword::try_from_str(&settings.numbering_system).ok().and_then(|k| NumberingSystem::try_from(&k).ok());
        let failed = |_| a.throw(ErrorKind::Range, "Unsupported number format options");
        let decimal = DecimalFormatter::try_new(prefs, DecimalFormatterOptions::from(strategy)).map_err(failed)?;
        let exponent = DecimalFormatter::try_new(prefs, DecimalFormatterOptions::from(GroupingStrategy::Never)).map_err(failed)?;
        let compact = match settings.compact_display {
            None => None,
            Some(display) => {
                let mut cprefs = CompactDecimalFormatterPreferences::from(&Locale::from(locale.id.clone()));
                cprefs.numbering_system = prefs.numbering_system;
                let copts = CompactDecimalFormatterOptions::from(strategy);
                Some(match display {
                    CompactDisplay::Short => CompactDecimalFormatter::try_new_short(cprefs, copts),
                    CompactDisplay::Long => CompactDecimalFormatter::try_new_long(cprefs, copts),
                }
                .map_err(failed)?)
            }
        };
        Ok(Self { decimal, exponent, compact, settings })
    }

    fn exponent_for(&self, magnitude: i16) -> i16 {
        match self.settings.notation {
            Notation::Standard => 0,
            Notation::Scientific => magnitude,
            Notation::Engineering => magnitude.div_euclid(ENGINEERING_STEP) * ENGINEERING_STEP,
            Notation::Compact => self.compact.as_ref().map_or(0, |c| i16::from(c.compact_exponent_for_magnitude(magnitude))),
        }
    }

    fn scaled(&self, x: &Decimal) -> (Decimal, i16) {
        if x.absolute.is_zero() {
            return (self.settings.digits.round(x), 0);
        }
        let magnitude = x.absolute.nonzero_magnitude_start();
        let exponent = self.exponent_for(magnitude);
        let mut shifted = x.clone();
        shifted.absolute.multiply_pow10(-exponent);
        let rounded = self.settings.digits.round(&shifted);
        if rounded.absolute.is_zero() || rounded.absolute.nonzero_magnitude_start() == magnitude - exponent {
            return (rounded, exponent);
        }
        let exponent = self.exponent_for(magnitude + 1);
        let mut shifted = x.clone();
        shifted.absolute.multiply_pow10(-exponent);
        (self.settings.digits.round(&shifted), exponent)
    }

    fn number_part(parts: &[&Part]) -> &'static str {
        parts.last().map_or(LITERAL, |p| match p.category {
            "decimal" => p.value,
            _ => LITERAL,
        })
    }

    fn compact_part(parts: &[&Part]) -> &'static str {
        match parts.last() {
            Some(p) if p.category == "decimal" => p.value,
            _ => "compact",
        }
    }

    fn sign(&self, value: &mut Decimal, negative: bool) {
        if negative && value.sign != Sign::Negative {
            value.sign = Sign::Negative;
        }
        value.apply_sign_display(self.settings.sign_display.into());
    }

    fn special(&self, text: &str, kind: &'static str, negative: bool) -> Pieces {
        let zero_like = kind == "nan" && self.settings.sign_display == SignDisplay::ExceptZero;
        let mut probe = Decimal::from(u8::from(!zero_like));
        self.sign(&mut probe, negative);
        let mut out = Pieces::default();
        for (k, t) in Pieces::collect(&self.decimal.format(&probe), Self::number_part).0 {
            if k == "integer" {
                out.push(kind, text);
            } else {
                out.push(k, &t);
            }
        }
        out
    }

    fn finite(&self, x: &Decimal) -> Pieces {
        let mut x = x.clone();
        if self.settings.style == Style::Percent {
            x.absolute.multiply_pow10(PERCENT_SHIFT);
        }
        let negative = x.sign == Sign::Negative;
        let (mut value, exponent) = self.scaled(&x);
        self.sign(&mut value, negative);
        match (&self.compact, self.settings.notation) {
            (Some(c), Notation::Compact) => match c.format_with_exponent(&value, u8::try_from(exponent).unwrap_or(0)) {
                Ok(w) => Self::split_compact(Pieces::collect(&w, Self::compact_part)),
                Err(_) => Pieces::collect(&self.decimal.format(&value), Self::number_part),
            },
            (_, Notation::Scientific | Notation::Engineering) => {
                let mut out = Pieces::collect(&self.decimal.format(&value), Self::number_part);
                out.push("exponentSeparator", EXPONENT);
                if exponent < 0 {
                    out.push("exponentMinusSign", "-");
                }
                let e = Decimal::from(exponent.unsigned_abs());
                let digits = Pieces::collect(&self.exponent.format(&e), Self::number_part);
                for (_, t) in digits.0 {
                    out.push("exponentInteger", &t);
                }
                out
            }
            _ => Pieces::collect(&self.decimal.format(&value), Self::number_part),
        }
    }

    fn split_compact(p: Pieces) -> Pieces {
        let mut out = Pieces::default();
        for (k, t) in p.0 {
            if k != "compact" {
                out.push(k, &t);
                continue;
            }
            let core = t.trim();
            let lead = &t[..t.len() - t.trim_start().len()];
            let tail = &t[t.trim_end().len()..];
            out.push(LITERAL, lead);
            out.push("compact", core);
            out.push(LITERAL, tail);
        }
        out
    }

    pub(crate) fn pieces(&self, x: &Numeric) -> Pieces {
        let mut out = match x {
            Numeric::NotANumber => self.special(NAN, "nan", false),
            Numeric::Infinite(negative) => self.special(INFINITY, "infinity", *negative),
            Numeric::Finite(d) => self.finite(d),
        };
        match (&self.settings.currency, &self.settings.unit) {
            (Some((code, ..)), _) => Self::prefix_currency(out, code),
            (_, Some((unit, display))) => match self.unit_pattern(unit, *display, x) {
                Some((before, after)) => {
                    let mut wrapped = Pieces::default();
                    Self::affix(&mut wrapped, &before);
                    for (k, t) in out.0 {
                        wrapped.push(k, &t);
                    }
                    Self::affix(&mut wrapped, &after);
                    wrapped
                }
                None => {
                    out.push(LITERAL, AFFIX_SPACE);
                    out.push("unit", unit);
                    out
                }
            },
            _ if self.settings.style == Style::Percent => {
                out.push("percentSign", PERCENT);
                out
            }
            _ => out,
        }
    }

    fn affix(out: &mut Pieces, text: &str) {
        let word = text.trim_matches(char::is_whitespace);
        let Some(at) = text.find(word).filter(|_| !word.is_empty()) else {
            out.push(LITERAL, text);
            return;
        };
        out.push(LITERAL, &text[..at]);
        out.push("unit", word);
        out.push(LITERAL, &text[at + word.len()..]);
    }

    fn unit_pattern(&self, unit: &str, display: UnitDisplay, x: &Numeric) -> Option<(String, String)> {
        use icu_experimental::dimension::provider::units::categorized_display_names as names;
        let width = match display {
            UnitDisplay::Short => "short",
            UnitDisplay::Narrow => "narrow",
            UnitDisplay::Long => "long",
        };
        let attribute = format!("{width}-{unit}");
        let attributes = DataMarkerAttributes::try_from_str(&attribute).ok()?;
        let locale = Tags::parse(&self.settings.locale)?;
        let data_locale = DataLocale::from(&locale.id);
        let mut metadata = DataRequestMetadata::default();
        metadata.silent = true;
        let req = DataRequest { id: DataIdentifierBorrowed::for_marker_attributes_and_locale(attributes, &data_locale), metadata };
        let baked = &icu_experimental::provider::Baked;
        macro_rules! first_loaded {
            ($($marker:ident),*) => {
                None$(.or_else(|| DataProvider::<names::$marker>::load(baked, req).ok().map(|r| r.payload.cast::<names::UnitsNamesDurationCoreV1>())))*
            };
        }
        let payload = first_loaded!(
            UnitsNamesDurationCoreV1, UnitsNamesDurationExtendedV1, UnitsNamesDurationOutlierV1,
            UnitsNamesLengthCoreV1, UnitsNamesLengthExtendedV1, UnitsNamesLengthOutlierV1,
            UnitsNamesMassCoreV1, UnitsNamesMassExtendedV1, UnitsNamesMassOutlierV1,
            UnitsNamesVolumeCoreV1, UnitsNamesVolumeExtendedV1, UnitsNamesVolumeOutlierV1,
            UnitsNamesAreaCoreV1, UnitsNamesAreaExtendedV1, UnitsNamesAreaOutlierV1
        )?;
        let rules = icu_plurals::PluralRules::try_new_cardinal(icu_plurals::PluralRulesPreferences::from(&locale)).ok()?;
        let operands = match x {
            Numeric::Finite(d) => icu_plurals::PluralOperands::from(&self.rounded(d).0),
            _ => icu_plurals::PluralOperands::from(0u8),
        };
        let filled = payload.get().get(operands, &rules).interpolate_to_string([UNIT_MARK]);
        let (before, after) = filled.split_once(UNIT_MARK)?;
        Some((String::from(before), String::from(after)))
    }

    fn prefix_currency(number: Pieces, code: &str) -> Pieces {
        let mut out = Pieces::default();
        let mut placed = false;
        for (k, t) in number.0 {
            if !placed && !matches!(k, "minusSign" | "plusSign") {
                out.push("currency", code);
                out.push(LITERAL, AFFIX_SPACE);
                placed = true;
            }
            out.push(k, &t);
        }
        out
    }

    pub(crate) fn format(&self, x: &Numeric) -> String {
        self.pieces(x).text()
    }
}

impl Settings {
    fn resolved<'scope>(&self, a: &mut Scope<'scope, '_>) -> Result<Value<'scope>, Error> {
        let o = a.object();
        let text = |a: &mut Scope<'scope, '_>, s: &str| -> Value<'scope> { a.string(s).into() };
        let d = &self.digits;
        let mut fields: alloc::vec::Vec<(&str, Value<'scope>)> = alloc::vec![
            ("locale", text(a, &self.locale)),
            ("numberingSystem", text(a, &self.numbering_system)),
            ("style", text(a, self.style.name())),
        ];
        if let Some((code, display, sign)) = &self.currency {
            fields.push(("currency", text(a, code)));
            fields.push(("currencyDisplay", text(a, display.name())));
            fields.push(("currencySign", text(a, sign.name())));
        }
        if let Some((unit, display)) = &self.unit {
            fields.push(("unit", text(a, unit)));
            fields.push(("unitDisplay", text(a, display.name())));
        }
        d.precision_fields(&mut fields);
        let grouping = self.grouping.map_or(Value::from(false), |g| text(a, g.name()));
        fields.push(("useGrouping", grouping));
        fields.push(("notation", text(a, self.notation.name())));
        if let Some(c) = self.compact_display {
            fields.push(("compactDisplay", text(a, c.name())));
        }
        fields.push(("signDisplay", text(a, self.sign_display.name())));
        d.rounding_fields(a, &mut fields);
        for (k, v) in fields {
            Tags::put(a, o, k, v)?;
        }
        Ok(o.into())
    }
}

pub(crate) struct NumberFormats;

impl NumberFormats {
    pub(crate) fn construct<'scope>(a: &mut Scope<'scope, '_>, ctor: JsObject<'scope>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let proto = Hosted::proto(a, ctor, this);
        let nf = NumberFormat::new(a, args.at(0), args.at(1))?;
        Ok(Hosted::alloc(a, Service::Number(Box::new(nf)), proto).into())
    }

    fn with<'scope, R>(a: &Scope<'scope, '_>, this: Value<'scope>, method: &str, f: impl FnOnce(&NumberFormat) -> R) -> Result<R, Error> {
        Hosted::with(a, this, |h| match &h.service {
            Service::Number(n) => Some(f(n)),
            _ => None,
        })
        .ok_or_else(|| Hosted::incompatible(a, method))
    }

    fn format_bound<'scope>(a: &mut Scope<'scope, '_>, f: JsObject<'scope>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let target = Hosted::slot(f, TARGET_SLOT).unwrap_or_default();
        let x = Numeric::read(a, args.at(0))?;
        let s = Self::with(a, target, "format", |n| n.format(&x))?;
        Ok(a.string(&s).into())
    }

    pub(crate) fn format<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::with(a, this, "get Intl.NumberFormat.prototype.format", |_| ())?;
        Hosted::bound(a, this, Self::format_bound, 1).map(Value::from).ok_or_else(|| Hosted::incompatible(a, "format"))
    }

    pub(crate) fn format_to_parts<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::with(a, this, "Intl.NumberFormat.prototype.formatToParts", |_| ())?;
        let x = Numeric::read(a, args.at(0))?;
        let pieces = Self::with(a, this, "formatToParts", |n| n.pieces(&x))?;
        pieces.array(a)
    }

    pub(crate) fn resolved_options<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let settings = Self::with(a, this, "Intl.NumberFormat.prototype.resolvedOptions", |n| n.settings.clone())?;
        settings.resolved(a)
    }

    pub(crate) fn supported_locales_of<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Tags::supported(a, args.at(0), args.at(1))
    }

    pub(crate) fn to_locale_string<'scope>(a: &mut Scope<'scope, '_>, x: &Numeric, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let nf = NumberFormat::new(a, args.at(0), args.at(1))?;
        Ok(a.string(&nf.format(x)).into())
    }

    pub(crate) fn number_to_locale_string<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let ValueKind::Number(x) = this.unwrapped().kind() else {
            return Err(a.throw(ErrorKind::Type, "Number.prototype.toLocaleString requires that 'this' be a Number"));
        };
        Self::to_locale_string(a, &Numeric::from(x), args)
    }

    pub(crate) fn bigint_to_locale_string<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let x = match this.unwrapped().kind() {
            ValueKind::BigInt(b) => Decimal::try_from_str(&b.0.to_string()).map_or(Numeric::NotANumber, Numeric::Finite),
            _ => return Err(a.throw(ErrorKind::Type, "BigInt.prototype.toLocaleString requires that 'this' be a BigInt")),
        };
        Self::to_locale_string(a, &x, args)
    }
}
