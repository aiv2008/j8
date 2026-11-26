use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use fixed_decimal::{Decimal, FloatPrecision};
use icu_locale_core::Locale;
use icu_plurals::{PluralCategory, PluralOperands, PluralRuleType, PluralRules, PluralRulesOptions, PluralRulesPreferences, PluralRulesWithRanges};
use j8framework::{Error, ErrorKind, JsObject, Scope, Value, ValueKind};

use super::locale::{ExtKey, LocaleData, Options, Tags, choices};
use super::number::{CompactDisplay, Digits, Notation, NumberFormat};

use super::service::{Hosted, Service};
use crate::Args;

choices! {
    PluralType { Cardinal = "cardinal", Ordinal = "ordinal" }
}

const PLURAL_MAX_FRACTION: u32 = 3;
const CATEGORY_ORDER: [PluralCategory; 6] = [PluralCategory::Zero, PluralCategory::One, PluralCategory::Two, PluralCategory::Few, PluralCategory::Many, PluralCategory::Other];
const OTHER: &str = "other";

struct PluralData;

impl LocaleData for PluralData {
    const KEYS: &'static [ExtKey] = &[];

    fn default_value(_: ExtKey, _: &Locale) -> Option<String> {
        None
    }

    fn supports(_: ExtKey, _: &str, _: &Locale) -> bool {
        false
    }
}

#[derive(Clone)]
struct Settings {
    locale: String,
    kind: PluralType,
    notation: Notation,
    compact_display: Option<CompactDisplay>,
    digits: Digits,
    categories: Vec<&'static str>,
}

pub(crate) struct Plurals {
    rules: PluralRulesWithRanges<PluralRules>,
    rounding: NumberFormat,
    settings: Settings,
}

impl Plurals {
    fn name(c: PluralCategory) -> &'static str {
        match c {
            PluralCategory::Zero => "zero",
            PluralCategory::One => "one",
            PluralCategory::Two => "two",
            PluralCategory::Few => "few",
            PluralCategory::Many => "many",
            PluralCategory::Other => OTHER,
        }
    }

    fn new<'scope>(a: &mut Scope<'scope, '_>, locales: Value<'scope>, options: Value<'scope>) -> Result<Self, Error> {
        let requested = Tags::list(a, locales)?;
        let options = Options::coerce(a, options)?;
        options.matcher(a)?;
        let kind = options.choice(a, "type")?.unwrap_or(PluralType::Cardinal);
        let notation = options.choice(a, "notation")?.unwrap_or(Notation::Standard);
        let compact_display = options.choice(a, "compactDisplay")?.unwrap_or(CompactDisplay::Short);
        let digits = Digits::read(a, options, 0, PLURAL_MAX_FRACTION, notation)?;
        let resolved = Tags::resolve::<PluralData>(a, &requested, &[]);
        let locale = Locale::from(resolved.locale.id.clone());
        let mut opts = PluralRulesOptions::default();
        opts.rule_type = Some(match kind {
            PluralType::Cardinal => PluralRuleType::Cardinal,
            PluralType::Ordinal => PluralRuleType::Ordinal,
        });
        let rules = PluralRulesWithRanges::try_new(PluralRulesPreferences::from(&locale), opts).map_err(|_| a.throw(ErrorKind::Range, "Unsupported plural rules options"))?;
        let rounding = NumberFormat::rounding(a, &locale, digits, notation, compact_display)?;
        let categories = CATEGORY_ORDER.into_iter().filter(|c| rules.rules().categories().any(|k| k == *c)).map(Self::name).collect();
        let settings = Settings { locale: resolved.tag(), kind, notation, compact_display: (notation == Notation::Compact).then_some(compact_display), digits, categories };
        Ok(Self { rules, rounding, settings })
    }

    fn operands(&self, x: f64) -> Option<PluralOperands> {
        let d = Decimal::try_from_f64(x, FloatPrecision::RoundTrip).ok()?;
        let (rounded, exponent) = self.rounding.rounded(&d);
        Some(match u8::try_from(exponent) {
            Ok(e) if e > 0 => PluralOperands::from_significand_and_exponent(&rounded.absolute, e),
            _ => PluralOperands::from(&rounded),
        })
    }

    fn select(&self, x: f64) -> &'static str {
        self.operands(x).map_or(OTHER, |o| Self::name(self.rules.rules().category_for(o)))
    }

    fn select_range(&self, x: f64, y: f64) -> &'static str {
        match (self.operands(x), self.operands(y)) {
            (Some(s), Some(e)) => Self::name(self.rules.category_for_range(s, e)),
            _ => OTHER,
        }
    }
}

impl Settings {
    fn resolved<'scope>(&self, a: &mut Scope<'scope, '_>) -> Result<Value<'scope>, Error> {
        let o = a.object();
        let mut fields: Vec<(&str, Value<'scope>)> = alloc::vec![
            ("locale", a.string(&self.locale).into()),
            ("type", a.string(self.kind.name()).into()),
            ("notation", a.string(self.notation.name()).into()),
        ];
        if let Some(c) = self.compact_display {
            fields.push(("compactDisplay", a.string(c.name()).into()));
        }
        self.digits.precision_fields(&mut fields);
        let categories: Vec<String> = self.categories.iter().map(|c| String::from(*c)).collect();
        fields.push(("pluralCategories", Tags::strings(a, &categories)));
        self.digits.rounding_fields(a, &mut fields);
        for (k, v) in fields {
            Tags::put(a, o, k, v)?;
        }
        Ok(o.into())
    }
}

pub(crate) struct PluralRulesBuiltins;

impl PluralRulesBuiltins {
    pub(crate) fn construct<'scope>(a: &mut Scope<'scope, '_>, ctor: JsObject<'scope>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        if matches!(a.new_target.kind(), ValueKind::Undefined) {
            return Err(a.throw(ErrorKind::Type, "Constructor Intl.PluralRules requires 'new'"));
        }
        let proto = Hosted::proto(a, ctor, this);
        let p = Plurals::new(a, args.at(0), args.at(1))?;
        Ok(Hosted::alloc(a, Service::Plural(Box::new(p)), proto).into())
    }

    fn with<'scope, R>(a: &Scope<'scope, '_>, this: Value<'scope>, method: &str, f: impl FnOnce(&Plurals) -> R) -> Result<R, Error> {
        Hosted::with(a, this, |h| match &h.service {
            Service::Plural(p) => Some(f(p)),
            _ => None,
        })
        .ok_or_else(|| Hosted::incompatible(a, method))
    }

    pub(crate) fn select<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::with(a, this, "Intl.PluralRules.prototype.select", |_| ())?;
        let x = a.to_number(args.at(0))?;
        let c = Self::with(a, this, "select", |p| p.select(x))?;
        Ok(a.string(c).into())
    }

    pub(crate) fn select_range<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::with(a, this, "Intl.PluralRules.prototype.selectRange", |_| ())?;
        if matches!(args.at(0).kind(), ValueKind::Undefined) || matches!(args.at(1).kind(), ValueKind::Undefined) {
            return Err(a.throw(ErrorKind::Type, "start and end must be defined"));
        }
        let x = a.to_number(args.at(0))?;
        let y = a.to_number(args.at(1))?;
        if x.is_nan() || y.is_nan() {
            return Err(a.throw(ErrorKind::Range, "start and end must be numbers"));
        }
        let c = Self::with(a, this, "selectRange", |p| p.select_range(x, y))?;
        Ok(a.string(c).into())
    }

    pub(crate) fn resolved_options<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let settings = Self::with(a, this, "Intl.PluralRules.prototype.resolvedOptions", |p| p.settings.clone())?;
        settings.resolved(a)
    }

    pub(crate) fn supported_locales_of<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Tags::supported(a, args.at(0), args.at(1))
    }
}
