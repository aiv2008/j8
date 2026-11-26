mod collator;
mod datetime;
mod displaynames;
mod durationformat;
mod ilocale;
mod listformat;
mod locale;
mod number;
mod plural;
mod relativetime;
mod segmenter;
mod service;
mod zone;

use alloc::string::String;
use alloc::vec::Vec;

use j8framework::{Error, ErrorKind, JsObject, NativeClosure, ObjectData, ObjectKind, Property, Scope, Slot, Value};

use crate::{Args, Installer};
use collator::Collation;
pub(crate) use collator::{Cases, Collators};
use datetime::DateTimeFormat;
pub(crate) use datetime::DateTimeFormats;
use listformat::ListFormats;
use ilocale::Locales;
use displaynames::DisplayNamesBuiltins;
use durationformat::DurationFormats;
use locale::Tags;
use relativetime::RelativeTimeFormats;
use segmenter::{ITERATOR_PROTO_SLOT, SEGMENTS_PROTO_SLOT, Segmenters};
use number::NumberFormat;
pub(crate) use number::NumberFormats;
use plural::PluralRulesBuiltins;
pub(crate) use zone::Zone;

struct Intl;

impl Intl {
    fn get_canonical_locales<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let list: Vec<String> = Tags::list(a, args.at(0))?.iter().map(alloc::string::ToString::to_string).collect();
        Ok(Tags::strings(a, &list))
    }


    fn supported_values_of<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let key = a.to_string(args.at(0))?;
        let mut sorted: Vec<String> = match key.as_str() {
            "calendar" => DateTimeFormat::calendars().iter().map(|s| String::from(*s)).collect(),
            "collation" => Collation::types().iter().map(|s| String::from(*s)).collect(),
            "numberingSystem" => NumberFormat::numbering_systems().iter().map(|s| String::from(*s)).collect(),
            "unit" => NumberFormat::units().iter().map(|s| String::from(*s)).collect(),
            "timeZone" => Zone::names(),
            "currency" => Vec::new(),
            _ => return Err(a.throw(ErrorKind::Range, &alloc::format!("Invalid key : {}", key.as_str()))),
        };
        sorted.sort();
        Ok(Tags::strings(a, &sorted))
    }
}

impl<'scope> Installer<'_, 'scope> {
    fn service(&self, intl: JsObject<'scope>, name: &str, construct: j8framework::ClosureFn, supported: Option<j8framework::NativeFn>, tag: &str, length: u32) -> JsObject<'scope> {
        let a = self.context;
        let proto = self.plain();
        let closure = NativeClosure { f: construct, slots: alloc::vec![proto.into()], constructor: true };
        let ctor = ObjectData { kind: ObjectKind::NativeClosure(closure), proto: Some(a.function_proto), ..ObjectData::default() }.alloc(self.mc);
        a.name_function(self.mc, ctor, name, Some(length));
        ctor.borrow_mut(self.mc).define("prototype", Property { slot: Slot::Data(proto.into()), enumerable: false, writable: false, configurable: false });
        self.set(proto, "constructor", ctor.into());
        self.set(intl, name, ctor.into());
        if let Some(supported) = supported {
            self.method(ctor, "supportedLocalesOf", supported, 1);
        }
        let t = self.text(tag);
        proto.borrow_mut(self.mc).define_symbol(a.symbols.to_string_tag, Property { slot: Slot::Data(t), enumerable: false, writable: false, configurable: true });
        proto
    }

    pub(crate) fn intl(&mut self) {
        let r = self.context;
        let intl = self.plain();
        self.set(r.global, "Intl", intl.into());
        let tag = self.text("Intl");
        intl.borrow_mut(self.mc).define_symbol(r.symbols.to_string_tag, Property { slot: Slot::Data(tag), enumerable: false, writable: false, configurable: true });
        self.method(intl, "getCanonicalLocales", Intl::get_canonical_locales, 1);
        self.method(intl, "supportedValuesOf", Intl::supported_values_of, 1);

        let collator = self.service(intl, "Collator", Collators::construct, Some(Collators::supported_locales_of), "Intl.Collator", 0);
        self.getter(collator, "compare", Collators::compare);
        self.method(collator, "resolvedOptions", Collators::resolved_options, 0);

        let number = self.service(intl, "NumberFormat", NumberFormats::construct, Some(NumberFormats::supported_locales_of), "Intl.NumberFormat", 0);
        self.getter(number, "format", NumberFormats::format);
        self.method(number, "formatToParts", NumberFormats::format_to_parts, 1);
        self.method(number, "resolvedOptions", NumberFormats::resolved_options, 0);

        let date = self.service(intl, "DateTimeFormat", DateTimeFormats::construct, Some(DateTimeFormats::supported_locales_of), "Intl.DateTimeFormat", 0);
        self.getter(date, "format", DateTimeFormats::format);
        self.method(date, "formatToParts", DateTimeFormats::format_to_parts, 1);
        self.method(date, "resolvedOptions", DateTimeFormats::resolved_options, 0);

        let plural = self.service(intl, "PluralRules", PluralRulesBuiltins::construct, Some(PluralRulesBuiltins::supported_locales_of), "Intl.PluralRules", 0);
        self.method(plural, "select", PluralRulesBuiltins::select, 1);
        self.method(plural, "selectRange", PluralRulesBuiltins::select_range, 2);
        self.method(plural, "resolvedOptions", PluralRulesBuiltins::resolved_options, 0);

        let list = self.service(intl, "ListFormat", ListFormats::construct, Some(ListFormats::supported_locales_of), "Intl.ListFormat", 0);
        self.method(list, "format", ListFormats::format, 1);
        self.method(list, "formatToParts", ListFormats::format_to_parts, 1);
        self.method(list, "resolvedOptions", ListFormats::resolved_options, 0);

        let segmenter = self.service(intl, "Segmenter", Segmenters::construct, Some(Segmenters::supported_locales_of), "Intl.Segmenter", 0);
        let segments = self.plain();
        self.method(segments, "containing", Segmenters::containing, 1);
        let iterator = ObjectData { proto: Some(r.iterator_proto), ..ObjectData::default() }.alloc(self.mc);
        self.method(iterator, "next", Segmenters::next, 0);
        let t = self.text("Segmenter String Iterator");
        iterator.borrow_mut(self.mc).define_symbol(r.symbols.to_string_tag, Property { slot: Slot::Data(t), enumerable: false, writable: false, configurable: true });
        let at = r.named_native(self.mc, Segmenters::iterator, "[Symbol.iterator]", 0);
        segments.borrow_mut(self.mc).define_symbol(r.symbols.iterator, Property::hidden(at.into()));
        let mut slots = alloc::vec![Value::Undefined; 2];
        slots[SEGMENTS_PROTO_SLOT] = segments.into();
        slots[ITERATOR_PROTO_SLOT] = iterator.into();
        let segment = ObjectData { kind: ObjectKind::NativeClosure(NativeClosure { f: Segmenters::segment, slots, constructor: false }), proto: Some(r.function_proto), ..ObjectData::default() }.alloc(self.mc);
        r.name_function(self.mc, segment, "segment", Some(1));
        self.set(segmenter, "segment", segment.into());
        self.method(segmenter, "resolvedOptions", Segmenters::resolved_options, 0);

        let relative = self.service(intl, "RelativeTimeFormat", RelativeTimeFormats::construct, Some(RelativeTimeFormats::supported_locales_of), "Intl.RelativeTimeFormat", 0);
        self.method(relative, "format", RelativeTimeFormats::format, 2);
        self.method(relative, "formatToParts", RelativeTimeFormats::format_to_parts, 2);
        self.method(relative, "resolvedOptions", RelativeTimeFormats::resolved_options, 0);

        let names = self.service(intl, "DisplayNames", DisplayNamesBuiltins::construct, Some(DisplayNamesBuiltins::supported_locales_of), "Intl.DisplayNames", 2);
        self.method(names, "of", DisplayNamesBuiltins::of, 1);
        self.method(names, "resolvedOptions", DisplayNamesBuiltins::resolved_options, 0);

        let duration = self.service(intl, "DurationFormat", DurationFormats::construct, Some(DurationFormats::supported_locales_of), "Intl.DurationFormat", 0);
        self.method(duration, "format", DurationFormats::format, 1);
        self.method(duration, "formatToParts", DurationFormats::format_to_parts, 1);
        self.method(duration, "resolvedOptions", DurationFormats::resolved_options, 0);

        self.intl_locale(intl);
    }

    fn intl_locale(&self, intl: JsObject<'scope>) {
        let r = self.context;
        let locale = self.service(intl, "Locale", Locales::construct, None, "Intl.Locale", 1);
        for (name, f) in [("maximize", Locales::maximize as j8framework::ClosureFn), ("minimize", Locales::minimize)] {
            let c = ObjectData { kind: ObjectKind::NativeClosure(NativeClosure { f, slots: alloc::vec![locale.into()], constructor: false }), proto: Some(r.function_proto), ..ObjectData::default() }.alloc(self.mc);
            r.name_function(self.mc, c, name, Some(0));
            self.set(locale, name, c.into());
        }
        let methods: [(&str, j8framework::NativeFn); 9] = [
            ("toString", Locales::to_string),
            ("toJSON", Locales::to_json),
            ("getCalendars", Locales::get_calendars),
            ("getCollations", Locales::get_collations),
            ("getHourCycles", Locales::get_hour_cycles),
            ("getNumberingSystems", Locales::get_numbering_systems),
            ("getTimeZones", Locales::get_time_zones),
            ("getTextInfo", Locales::get_text_info),
            ("getWeekInfo", Locales::get_week_info),
        ];
        for (name, f) in methods {
            self.method(locale, name, f, 0);
        }
        let getters: [(&str, j8framework::NativeFn); 12] = [
            ("baseName", Locales::base_name),
            ("calendar", Locales::calendar),
            ("caseFirst", Locales::case_first),
            ("collation", Locales::collation),
            ("firstDayOfWeek", Locales::first_day_of_week),
            ("hourCycle", Locales::hour_cycle),
            ("numeric", Locales::numeric),
            ("numberingSystem", Locales::numbering_system),
            ("language", Locales::language),
            ("script", Locales::script),
            ("region", Locales::region),
            ("variants", Locales::variants),
        ];
        for (name, f) in getters {
            self.getter(locale, name, f);
        }
    }
}
