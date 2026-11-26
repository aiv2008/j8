use j8framework::ValueKind;
use alloc::format;
use alloc::borrow::Cow;
use alloc::string::String;

use j8framework::{Scope, Error, ErrorKind, Hint, JsObject, ObjectKind, Property, Slot, Value};

use crate::{Args, Installer};

struct Dates;

const MS_PER_SECOND: f64 = 1000.0;
const MS_PER_MINUTE: f64 = 60_000.0;
const MS_PER_HOUR: f64 = 3_600_000.0;
const MS_PER_DAY: f64 = 86_400_000.0;

const MAX_TIME: f64 = 8.64e15;

const MAX_YEAR: f64 = 400_000.0;

const DAYS_PER_YEAR: f64 = 365.2425;

const MONTH_START: [f64; 13] = [0.0, 31.0, 59.0, 90.0, 120.0, 151.0, 181.0, 212.0, 243.0, 273.0, 304.0, 334.0, 365.0];
const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

const UTC_NAME: &str = "Coordinated Universal Time";

struct LocalZone {
    #[cfg(feature = "intl")]
    zone: crate::intl::Zone,
}

impl LocalZone {
    #[cfg(feature = "intl")]
    fn host(a: &Scope<'_, '_>) -> Self {
        Self { zone: crate::intl::Zone::host(a.platform.time_zone()) }
    }

    #[cfg(not(feature = "intl"))]
    fn host(_: &Scope<'_, '_>) -> Self {
        Self {}
    }

    #[cfg(feature = "intl")]
    fn offset(&self, t: f64) -> f64 {
        f64::from(self.zone.offset_seconds(t)) * MS_PER_SECOND
    }

    #[cfg(not(feature = "intl"))]
    #[expect(clippy::unused_self, reason = "the intl build reads the host zone through self")]
    fn offset(&self, _: f64) -> f64 {
        0.0
    }

    #[cfg(feature = "intl")]
    fn name(&self, t: f64) -> Cow<'static, str> {
        self.zone.abbreviation(t).map_or(Cow::Borrowed(UTC_NAME), Cow::Owned)
    }

    #[cfg(not(feature = "intl"))]
    #[expect(clippy::unused_self, reason = "the intl build reads the host zone through self")]
    fn name(&self, _: f64) -> Cow<'static, str> {
        Cow::Borrowed(UTC_NAME)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Year,
    Month,
    Date,
    Day,
    Hours,
    Minutes,
    Seconds,
    Milliseconds,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Setter {
    Milliseconds,
    Seconds,
    Minutes,
    Hours,
    Date,
    Month,
    FullYear,
}

fn modulo(a: f64, b: f64) -> f64 {
    let r = a % b;
    if r < 0.0 { r + b } else { r + 0.0 }
}

impl Dates {
    fn day(t: f64) -> f64 {
        libm::floor(t / MS_PER_DAY)
    }

    fn days_in_year(y: f64) -> f64 {
        if modulo(y, 4.0) != 0.0 {
            365.0
        } else if modulo(y, 100.0) != 0.0 {
            366.0
        } else if modulo(y, 400.0) != 0.0 {
            365.0
        } else {
            366.0
        }
    }

    fn day_from_year(y: f64) -> f64 {
        365.0 * (y - 1970.0) + libm::floor((y - 1969.0) / 4.0) - libm::floor((y - 1901.0) / 100.0) + libm::floor((y - 1601.0) / 400.0)
    }

    fn year_from_time(t: f64) -> f64 {
        let mut y = libm::floor(t / (MS_PER_DAY * DAYS_PER_YEAR)) + 1970.0;
        while Self::day_from_year(y) * MS_PER_DAY > t {
            y -= 1.0;
        }
        while Self::day_from_year(y + 1.0) * MS_PER_DAY <= t {
            y += 1.0;
        }
        y
    }

    fn leap(y: f64) -> f64 {
        if Self::days_in_year(y) == 366.0 { 1.0 } else { 0.0 }
    }

    fn month_start(m: usize, y: f64) -> f64 {
        MONTH_START[m] + if m >= 2 { Self::leap(y) } else { 0.0 }
    }

    fn month_and_date(t: f64) -> (f64, f64) {
        let y = Self::year_from_time(t);
        let d = Self::day(t) - Self::day_from_year(y);
        let m = (0..12).rev().find(|&m| d >= Self::month_start(m, y)).unwrap_or(0);
        #[expect(clippy::cast_precision_loss, reason = "month is less than 12")]
        let mf = m as f64;
        (mf, d - Self::month_start(m, y) + 1.0)
    }

    fn field(t: f64, f: Field) -> f64 {
        match f {
            Field::Year => Self::year_from_time(t),
            Field::Month => Self::month_and_date(t).0,
            Field::Date => Self::month_and_date(t).1,
            Field::Day => modulo(Self::day(t) + 4.0, 7.0),
            Field::Hours => modulo(libm::floor(t / MS_PER_HOUR), 24.0),
            Field::Minutes => modulo(libm::floor(t / MS_PER_MINUTE), 60.0),
            Field::Seconds => modulo(libm::floor(t / MS_PER_SECOND), 60.0),
            Field::Milliseconds => modulo(t, MS_PER_SECOND),
        }
    }

    fn make_time(h: f64, m: f64, s: f64, ms: f64) -> f64 {
        if !(h.is_finite() && m.is_finite() && s.is_finite() && ms.is_finite()) {
            return f64::NAN;
        }
        libm::trunc(h) * MS_PER_HOUR + libm::trunc(m) * MS_PER_MINUTE + libm::trunc(s) * MS_PER_SECOND + libm::trunc(ms)
    }

    fn make_day(year: f64, month: f64, date: f64) -> f64 {
        if !(year.is_finite() && month.is_finite() && date.is_finite()) {
            return f64::NAN;
        }
        let (y, m, dt) = (libm::trunc(year), libm::trunc(month), libm::trunc(date));
        let ym = y + libm::floor(m / 12.0);
        if ym.abs() > MAX_YEAR {
            return f64::NAN;
        }
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "reduced modulo to [0, 12)")]
        let mn = modulo(m, 12.0) as usize;
        Self::day_from_year(ym) + Self::month_start(mn, ym) + dt - 1.0
    }

    fn make_date(day: f64, time: f64) -> f64 {
        if !(day.is_finite() && time.is_finite()) {
            return f64::NAN;
        }
        let tv = day * MS_PER_DAY + time;
        if tv.is_finite() { tv } else { f64::NAN }
    }

    fn time_clip(t: f64) -> f64 {
        if !t.is_finite() || t.abs() > MAX_TIME { f64::NAN } else { libm::trunc(t) + 0.0 }
    }

    fn full_year(y: f64) -> f64 {
        if y.is_nan() {
            return y;
        }
        let t = libm::trunc(y);
        if (0.0..=99.0).contains(&t) { 1900.0 + t } else { t }
    }

    fn now(a: &Scope<'_, '_>) -> f64 {
        let d = a.platform.wall_clock();
        #[expect(clippy::cast_precision_loss, reason = "millisecond count is far below 2^53")]
        let ms = d.as_millis() as f64;
        ms
    }

    fn this_time<'scope>(a: &Scope<'scope, '_>, this: Value<'scope>) -> Result<(JsObject<'scope>, f64), Error> {
        match this.kind() {
            ValueKind::Object(o) => match o.borrow().kind {
                ObjectKind::Date(t) => Ok((o, t)),
                _ => Err(a.throw(ErrorKind::Type, "this is not a Date object.")),
            },
            _ => Err(a.throw(ErrorKind::Type, "this is not a Date object.")),
        }
    }

    fn year_text(y: f64) -> String {
        #[expect(clippy::cast_possible_truncation, reason = "year is within ±275760")]
        let yi = y as i64;
        if yi >= 0 { format!("{yi:04}") } else { format!("-{:04}", -yi) }
    }

    fn int(x: f64) -> i64 {
        #[expect(clippy::cast_possible_truncation, reason = "date fields are all small integers")]
        let i = x as i64;
        i
    }

    fn slot(x: f64) -> usize {
        usize::try_from(Self::int(x)).unwrap_or(0)
    }

    fn date_part(t: f64) -> String {
        let wd = WEEKDAYS[Self::slot(Self::field(t, Field::Day)) % 7];
        let (m, d) = Self::month_and_date(t);
        format!("{wd} {} {:02} {}", MONTHS[Self::slot(m) % 12], Self::int(d), Self::year_text(Self::year_from_time(t)))
    }

    fn time_part(local: f64, offset: f64, name: &str) -> String {
        let minutes = Self::int(offset / MS_PER_MINUTE);
        let sign = if minutes < 0 { '-' } else { '+' };
        format!(
            "{:02}:{:02}:{:02} GMT{sign}{:02}{:02} ({name})",
            Self::int(Self::field(local, Field::Hours)),
            Self::int(Self::field(local, Field::Minutes)),
            Self::int(Self::field(local, Field::Seconds)),
            minutes.abs() / 60,
            minutes.abs() % 60,
        )
    }

    fn zoned_time_part(a: &Scope<'_, '_>, t: f64) -> String {
        let zone = LocalZone::host(a);
        let offset = zone.offset(t);
        Self::time_part(t + offset, offset, &zone.name(t))
    }

    fn to_date_string_of(a: &Scope<'_, '_>, t: f64) -> String {
        if t.is_nan() {
            return String::from("Invalid Date");
        }
        let zone = LocalZone::host(a);
        let offset = zone.offset(t);
        let local = t + offset;
        format!("{} {}", Self::date_part(local), Self::time_part(local, offset, &zone.name(t)))
    }

    fn local(a: &Scope<'_, '_>, t: f64) -> f64 {
        t + LocalZone::host(a).offset(t)
    }

    fn utc(a: &Scope<'_, '_>, local: f64) -> f64 {
        if !local.is_finite() {
            return local;
        }
        let zone = LocalZone::host(a);
        let guess = local - zone.offset(local);
        local - zone.offset(guess)
    }

    fn utc_string(t: f64) -> String {
        if t.is_nan() {
            return String::from("Invalid Date");
        }
        let wd = WEEKDAYS[Self::slot(Self::field(t, Field::Day)) % 7];
        let (m, d) = Self::month_and_date(t);
        format!(
            "{wd}, {:02} {} {} {:02}:{:02}:{:02} GMT",
            Self::int(d),
            MONTHS[Self::slot(m) % 12],
            Self::year_text(Self::year_from_time(t)),
            Self::int(Self::field(t, Field::Hours)),
            Self::int(Self::field(t, Field::Minutes)),
            Self::int(Self::field(t, Field::Seconds))
        )
    }

    fn iso_string(t: f64) -> String {
        let y = Self::int(Self::year_from_time(t));
        let year = if (0..=9999).contains(&y) {
            format!("{y:04}")
        } else if y < 0 {
            format!("-{:06}", -y)
        } else {
            format!("+{y:06}")
        };
        let (m, d) = Self::month_and_date(t);
        format!(
            "{year}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            Self::int(m) + 1,
            Self::int(d),
            Self::int(Self::field(t, Field::Hours)),
            Self::int(Self::field(t, Field::Minutes)),
            Self::int(Self::field(t, Field::Seconds)),
            Self::int(Self::field(t, Field::Milliseconds))
        )
    }

    fn parse(a: &Scope<'_, '_>, s: &str) -> f64 {
        match Self::parse_iso(s).or_else(|| Self::parse_text(s)) {
            Some((t, true)) => Self::time_clip(t),
            Some((t, false)) => Self::time_clip(Self::utc(a, t)),
            None => f64::NAN,
        }
    }

    fn digits(b: &[u8], at: &mut usize, n: usize) -> Option<f64> {
        let part = b.get(*at..*at + n)?;
        if !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        *at += n;
        Some(part.iter().fold(0.0, |acc, &c| acc * 10.0 + f64::from(c - b'0')))
    }

    fn parse_iso(s: &str) -> Option<(f64, bool)> {
        let b = s.as_bytes();
        let mut i = 0;
        let year = match b.first()? {
            sign @ (b'+' | b'-') => {
                i = 1;
                let y = Self::digits(b, &mut i, 6)?;
                if *sign == b'-' {
                    if y == 0.0 {
                        return None;
                    }
                    -y
                } else {
                    y
                }
            }
            _ => Self::digits(b, &mut i, 4)?,
        };
        let (mut month, mut day) = (1.0, 1.0);
        if b.get(i) == Some(&b'-') {
            i += 1;
            month = Self::digits(b, &mut i, 2)?;
            if b.get(i) == Some(&b'-') {
                i += 1;
                day = Self::digits(b, &mut i, 2)?;
            }
        }
        if !(1.0..=12.0).contains(&month) {
            return None;
        }
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "checked to be in [1, 12]")]
        let mi = month as usize - 1;
        let days = Self::month_start(mi + 1, year) - Self::month_start(mi, year);
        if !(1.0..=days).contains(&day) {
            return None;
        }
        let (mut h, mut mi_, mut sec, mut ms, mut offset) = (0.0, 0.0, 0.0, 0.0, 0.0);
        let mut absolute = true;
        if i < b.len() {
            absolute = false;
            if b[i] != b'T' {
                return None;
            }
            i += 1;
            h = Self::digits(b, &mut i, 2)?;
            if b.get(i) != Some(&b':') {
                return None;
            }
            i += 1;
            mi_ = Self::digits(b, &mut i, 2)?;
            if b.get(i) == Some(&b':') {
                i += 1;
                sec = Self::digits(b, &mut i, 2)?;
                if b.get(i) == Some(&b'.') {
                    i += 1;
                    let start = i;
                    while b.get(i).is_some_and(u8::is_ascii_digit) {
                        i += 1;
                    }
                    if i == start {
                        return None;
                    }
                    let mut j = start;
                    ms = Self::digits(b, &mut j, (i - start).min(3))?;
                    ms *= [100.0, 10.0, 1.0][(i - start).min(3) - 1];
                }
            }
            match b.get(i) {
                Some(b'Z') => {
                    i += 1;
                    absolute = true;
                }
                Some(sign @ (b'+' | b'-')) => {
                    absolute = true;
                    let neg = *sign == b'-';
                    i += 1;
                    let oh = Self::digits(b, &mut i, 2)?;
                    if b.get(i) != Some(&b':') {
                        return None;
                    }
                    i += 1;
                    let om = Self::digits(b, &mut i, 2)?;
                    if oh > 23.0 || om > 59.0 {
                        return None;
                    }
                    offset = (oh * 60.0 + om) * MS_PER_MINUTE * if neg { -1.0 } else { 1.0 };
                }
                _ => {}
            }
            if i != b.len() {
                return None;
            }
            if h > 24.0 || mi_ > 59.0 || sec > 59.0 || (h == 24.0 && (mi_ > 0.0 || sec > 0.0 || ms > 0.0)) {
                return None;
            }
        }
        let t = Self::make_date(Self::make_day(year, month - 1.0, day), Self::make_time(h, mi_, sec, ms));
        Some((t - offset, absolute))
    }

    fn parse_text(s: &str) -> Option<(f64, bool)> {
        let s = s.split('(').next()?.trim();
        let words: alloc::vec::Vec<&str> = s.split(|c: char| c.is_whitespace() || c == ',').filter(|w| !w.is_empty()).collect();
        let mut month = None;
        let mut nums = alloc::vec::Vec::new();
        let mut time = None;
        let mut offset = 0.0;
        let mut absolute = false;
        for w in words {
            if let Some(m) = MONTHS.iter().position(|m| w.len() >= 3 && m.eq_ignore_ascii_case(&w[..3])) {
                #[expect(clippy::cast_precision_loss, reason = "month is less than 12")]
                let mf = m as f64;
                month = Some(mf);
            } else if WEEKDAYS.iter().any(|d| w.len() >= 3 && d.eq_ignore_ascii_case(&w[..3])) {
            } else if w.contains(':') {
                let parts: alloc::vec::Vec<f64> = w.split(':').map(|p| p.parse::<f64>().ok()).collect::<Option<_>>()?;
                time = Some(parts);
            } else if let Some(rest) = w.strip_prefix("GMT").or_else(|| w.strip_prefix("UTC")).or_else(|| w.strip_prefix('Z')) {
                absolute = true;
                if !rest.is_empty() {
                    let (neg, digits) = match rest.as_bytes()[0] {
                        b'+' => (false, &rest[1..]),
                        b'-' => (true, &rest[1..]),
                        _ => return None,
                    };
                    let v: f64 = digits.parse().ok()?;
                    let mins = libm::floor(v / 100.0) * 60.0 + v % 100.0;
                    offset = mins * MS_PER_MINUTE * if neg { -1.0 } else { 1.0 };
                }
            } else {
                nums.push(w.parse::<f64>().ok()?);
            }
        }
        let month = month?;
        let [a, b] = nums[..] else { return None };
        let (day, year) = if a > 31.0 { (b, a) } else { (a, b) };
        let t = time.unwrap_or_default();
        let (h, m, sec) = (t.first().copied().unwrap_or(0.0), t.get(1).copied().unwrap_or(0.0), t.get(2).copied().unwrap_or(0.0));
        Some((Self::make_date(Self::make_day(year, month, day), Self::make_time(h, m, sec, 0.0)) - offset, absolute))
    }

    fn date<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let new_target = a.new_target;
        if new_target == Value::Undefined {
            let s = Self::to_date_string_of(a, Self::now(a));
            return Ok(a.string(&s).into());
        }
        let tv = match args {
            [] => Self::now(a),
            [v] => {
                let existing = match v.kind() {
                    ValueKind::Object(o) => match o.borrow().kind {
                        ObjectKind::Date(t) => Some(t),
                        _ => None,
                    },
                    _ => None,
                };
                match existing {
                    Some(t) => t,
                    None => match (a.to_primitive(*v, Hint::Default)?).kind() {
                        ValueKind::String(s) => Self::parse(a, &s),
                        p => Self::time_clip(a.to_number(Value::from(p))?),
                    },
                }
            }
            _ => {
                let local = Self::from_parts(a, args)?;
                Self::time_clip(Self::utc(a, local))
            }
        };
        Ok(a.create_from_ctor(new_target, a.context.date_proto, ObjectKind::Date(tv))?.into())
    }

    fn from_parts<'scope>(a: &mut Scope<'scope, '_>, args: &[Value<'scope>]) -> Result<f64, Error> {
        let defaults = [f64::NAN, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0];
        let mut n = defaults;
        for (i, slot) in n.iter_mut().enumerate() {
            if let Some(&v) = args.get(i) {
                *slot = a.to_number(v)?;
            }
        }
        let y = Self::full_year(n[0]);
        Ok(Self::make_date(Self::make_day(y, n[1], n[2]), Self::make_time(n[3], n[4], n[5], n[6])))
    }

    fn date_now<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Ok(Self::now(a).into())
    }

    fn date_parse<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let s = a.to_string(args.at(0))?;
        Ok(Self::parse(a, &s).into())
    }

    fn date_utc<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let y = a.to_number(args.at(0))?;
        let mut n = [y, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0];
        for (i, slot) in n.iter_mut().enumerate().skip(1) {
            if let Some(&v) = args.get(i) {
                *slot = a.to_number(v)?;
            }
        }
        let y = Self::full_year(n[0]);
        Ok(Self::time_clip(Self::make_date(Self::make_day(y, n[1], n[2]), Self::make_time(n[3], n[4], n[5], n[6]))).into())
    }

    fn get<'scope, const F: u8, const UTC: bool>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (_, t) = Self::this_time(a, this)?;
        if t.is_nan() {
            return Ok(f64::NAN.into());
        }
        let t = if UTC { t } else { Self::local(a, t) };
        let f = [Field::Year, Field::Month, Field::Date, Field::Day, Field::Hours, Field::Minutes, Field::Seconds, Field::Milliseconds][usize::from(F)];
        Ok(Self::field(t, f).into())
    }

    fn get_time<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Ok(Self::this_time(a, this)?.1.into())
    }

    fn get_timezone_offset<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (_, t) = Self::this_time(a, this)?;
        Ok(if t.is_nan() { f64::NAN } else { (t - Self::local(a, t)) / MS_PER_MINUTE }.into())
    }

    fn get_year<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (_, t) = Self::this_time(a, this)?;
        Ok(if t.is_nan() { f64::NAN } else { Self::year_from_time(Self::local(a, t)) - 1900.0 }.into())
    }

    fn store<'scope>(a: &Scope<'scope, '_>, o: JsObject<'scope>, t: f64) -> Value<'scope> {
        let v = Self::time_clip(t);
        o.borrow_mut(a.mc).kind = ObjectKind::Date(v);
        v.into()
    }

    fn set_time<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, _) = Self::this_time(a, this)?;
        let t = a.to_number(args.at(0))?;
        Ok(Self::store(a, o, t))
    }

    fn set<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>], which: Setter, utc: bool) -> Result<Value<'scope>, Error> {
        let (o, t) = Self::this_time(a, this)?;
        let count = match which {
            Setter::Milliseconds | Setter::Date => 1,
            Setter::Seconds | Setter::Month => 2,
            Setter::Minutes | Setter::FullYear => 3,
            Setter::Hours => 4,
        };
        let mut given = [f64::NAN; 4];
        let mut present = 0;
        for (i, slot) in given.iter_mut().enumerate().take(count) {
            if i == 0 || i < args.len() {
                *slot = a.to_number(args.at(i))?;
                present = i + 1;
            }
        }
        let t = match (t.is_nan(), which) {
            (true, Setter::FullYear) => 0.0,
            (true, _) => return Ok(f64::NAN.into()),
            (false, _) if utc => t,
            (false, _) => Self::local(a, t),
        };
        let pick = |i: usize, f: Field| if i < present { given[i] } else { Self::field(t, f) };
        let (y, m) = (Self::field(t, Field::Year), Self::field(t, Field::Month));
        let (h, mi, s) = (Self::field(t, Field::Hours), Self::field(t, Field::Minutes), Self::field(t, Field::Seconds));
        let day = Self::day(t);
        let within = modulo(t, MS_PER_DAY);
        let nt = match which {
            Setter::Milliseconds => Self::make_date(day, Self::make_time(h, mi, s, given[0])),
            Setter::Seconds => Self::make_date(day, Self::make_time(h, mi, given[0], pick(1, Field::Milliseconds))),
            Setter::Minutes => Self::make_date(day, Self::make_time(h, given[0], pick(1, Field::Seconds), pick(2, Field::Milliseconds))),
            Setter::Hours => Self::make_date(day, Self::make_time(given[0], pick(1, Field::Minutes), pick(2, Field::Seconds), pick(3, Field::Milliseconds))),
            Setter::Date => Self::make_date(Self::make_day(y, m, given[0]), within),
            Setter::Month => Self::make_date(Self::make_day(y, given[0], pick(1, Field::Date)), within),
            Setter::FullYear => Self::make_date(Self::make_day(given[0], pick(1, Field::Month), pick(2, Field::Date)), within),
        };
        let nt = if utc { nt } else { Self::utc(a, nt) };
        Ok(Self::store(a, o, nt))
    }

    fn set_year<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (o, t) = Self::this_time(a, this)?;
        let y = a.to_number(args.at(0))?;
        if y.is_nan() {
            return Ok(Self::store(a, o, f64::NAN));
        }
        let t = if t.is_nan() { 0.0 } else { Self::local(a, t) };
        let yyyy = Self::full_year(y);
        let d = Self::make_day(yyyy, Self::field(t, Field::Month), Self::field(t, Field::Date));
        let local = Self::make_date(d, modulo(t, MS_PER_DAY));
        Ok(Self::store(a, o, Self::utc(a, local)))
    }

    fn text<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, f: fn(&Scope<'_, '_>, f64) -> String) -> Result<Value<'scope>, Error> {
        let (_, t) = Self::this_time(a, this)?;
        let s = if t.is_nan() { String::from("Invalid Date") } else { f(a, t) };
        Ok(a.string(&s).into())
    }

    fn to_string<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::text(a, this, Self::to_date_string_of)
    }

    fn to_date_string<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::text(a, this, |a, t| Self::date_part(Self::local(a, t)))
    }

    locale_method!(to_locale_string, crate::intl::DateTimeFormats::to_locale_string, Self::to_string);

    locale_method!(to_locale_date_string, crate::intl::DateTimeFormats::to_locale_date_string, Self::to_date_string);

    locale_method!(to_locale_time_string, crate::intl::DateTimeFormats::to_locale_time_string, Self::to_time_string);

    fn to_time_string<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::text(a, this, Self::zoned_time_part)
    }

    fn to_utc_string<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Self::text(a, this, |_, t| Self::utc_string(t))
    }

    fn to_iso_string<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let (_, t) = Self::this_time(a, this)?;
        if t.is_nan() {
            return Err(a.throw(ErrorKind::Range, "Invalid time value"));
        }
        Ok(a.string(&Self::iso_string(t)).into())
    }

    fn to_json<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let o = a.to_object(this)?;
        let tv = a.to_primitive(o.into(), Hint::Number)?;
        if let Some(x) = tv.as_number()
            && !x.is_finite()
        {
            return Ok(Value::Null);
        }
        let f = a.get_named(o.into(), "toISOString")?;
        if !a.is_callable(f) {
            return Err(a.throw(ErrorKind::Type, "toISOString is not a function"));
        }
        a.call(f, o.into(), &[])
    }

    fn value_of<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Ok(Self::this_time(a, this)?.1.into())
    }

    fn to_primitive<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        if this.as_object().is_none() {
            return Err(a.throw(ErrorKind::Type, "Date.prototype[Symbol.toPrimitive] called on a non-object"));
        }
        let order = match args.at(0).kind() {
            ValueKind::String(s) if &**s == "string" || &**s == "default" => ["toString", "valueOf"],
            ValueKind::String(s) if &**s == "number" => ["valueOf", "toString"],
            _ => return Err(a.throw(ErrorKind::Type, "Invalid hint")),
        };
        for name in order {
            let f = a.get_named(this, name)?;
            if a.is_callable(f) {
                let r = a.call(f, this, &[])?;
                if r.as_object().is_none() {
                    return Ok(r);
                }
            }
        }
        Err(a.throw(ErrorKind::Type, "Cannot convert object to primitive value"))
    }
}

#[cfg(feature = "intl")]
pub(crate) struct Civil {
    pub(crate) year: i32,
    pub(crate) month: u8,
    pub(crate) day: u8,
    pub(crate) hour: u8,
    pub(crate) minute: u8,
    pub(crate) second: u8,
    pub(crate) millisecond: u16,
}

#[cfg(feature = "intl")]
impl From<f64> for Civil {
    fn from(t: f64) -> Self {
        let small = |f: Field| u8::try_from(Dates::int(Dates::field(t, f))).unwrap_or(0);
        let (m, d) = Dates::month_and_date(t);
        Self {
            year: i32::try_from(Dates::int(Dates::year_from_time(t))).unwrap_or(0),
            month: u8::try_from(Dates::int(m) + 1).unwrap_or(1),
            day: u8::try_from(Dates::int(d)).unwrap_or(1),
            hour: small(Field::Hours),
            minute: small(Field::Minutes),
            second: small(Field::Seconds),
            millisecond: u16::try_from(Dates::int(Dates::field(t, Field::Milliseconds))).unwrap_or(0),
        }
    }
}

macro_rules! setters {
    ($($name:ident, $utc:ident => $which:ident),* $(,)?) => {
        impl Dates {
            $(fn $name<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
                Self::set(a, this, args, Setter::$which, false)
            }

            fn $utc<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
                Self::set(a, this, args, Setter::$which, true)
            })*
        }
    };
}

setters! {
    set_milliseconds, set_utc_milliseconds => Milliseconds,
    set_seconds, set_utc_seconds => Seconds,
    set_minutes, set_utc_minutes => Minutes,
    set_hours, set_utc_hours => Hours,
    set_date, set_utc_date => Date,
    set_month, set_utc_month => Month,
    set_full_year, set_utc_full_year => FullYear,
}

impl Installer<'_, '_> {
    pub(crate) fn date(&mut self) {
        let r = self.context;
        let proto = r.date_proto;
        let ctor = self.constructor("Date", Dates::date, proto, 7);
        self.method(ctor, "now", Dates::date_now, 0);
        self.method(ctor, "parse", Dates::date_parse, 1);
        self.method(ctor, "UTC", Dates::date_utc, 7);
        let getters: [(&str, j8framework::NativeFn, j8framework::NativeFn); 8] = [
            ("FullYear", Dates::get::<0, false>, Dates::get::<0, true>),
            ("Month", Dates::get::<1, false>, Dates::get::<1, true>),
            ("Date", Dates::get::<2, false>, Dates::get::<2, true>),
            ("Day", Dates::get::<3, false>, Dates::get::<3, true>),
            ("Hours", Dates::get::<4, false>, Dates::get::<4, true>),
            ("Minutes", Dates::get::<5, false>, Dates::get::<5, true>),
            ("Seconds", Dates::get::<6, false>, Dates::get::<6, true>),
            ("Milliseconds", Dates::get::<7, false>, Dates::get::<7, true>),
        ];
        for (name, local, utc) in getters {
            self.method(proto, &alloc::format!("get{name}"), local, 0);
            self.method(proto, &alloc::format!("getUTC{name}"), utc, 0);
        }
        let setters: [(&str, j8framework::NativeFn, j8framework::NativeFn, u32); 7] = [
            ("Milliseconds", Dates::set_milliseconds, Dates::set_utc_milliseconds, 1),
            ("Seconds", Dates::set_seconds, Dates::set_utc_seconds, 2),
            ("Minutes", Dates::set_minutes, Dates::set_utc_minutes, 3),
            ("Hours", Dates::set_hours, Dates::set_utc_hours, 4),
            ("Date", Dates::set_date, Dates::set_utc_date, 1),
            ("Month", Dates::set_month, Dates::set_utc_month, 2),
            ("FullYear", Dates::set_full_year, Dates::set_utc_full_year, 3),
        ];
        for (name, local, utc, n) in setters {
            self.method(proto, &alloc::format!("set{name}"), local, n);
            self.method(proto, &alloc::format!("setUTC{name}"), utc, n);
        }
        let methods: [(&str, j8framework::NativeFn, u32); 15] = [
            ("getTime", Dates::get_time, 0),
            ("getTimezoneOffset", Dates::get_timezone_offset, 0),
            ("getYear", Dates::get_year, 0),
            ("setTime", Dates::set_time, 1),
            ("setYear", Dates::set_year, 1),
            ("toString", Dates::to_string, 0),
            ("toDateString", Dates::to_date_string, 0),
            ("toTimeString", Dates::to_time_string, 0),
            ("toISOString", Dates::to_iso_string, 0),
            ("toUTCString", Dates::to_utc_string, 0),
            ("toJSON", Dates::to_json, 1),
            ("toLocaleString", Dates::to_locale_string, 0),
            ("toLocaleDateString", Dates::to_locale_date_string, 0),
            ("toLocaleTimeString", Dates::to_locale_time_string, 0),
            ("valueOf", Dates::value_of, 0),
        ];
        for (name, f, n) in methods {
            self.method(proto, name, f, n);
        }
        let utc = proto.borrow().own(self.mc, "toUTCString").unwrap_or_default();
        self.set(proto, "toGMTString", utc);
        let tp = r.named_native(self.mc, Dates::to_primitive, "[Symbol.toPrimitive]", 1);
        proto.borrow_mut(self.mc).define_symbol(r.symbols.to_primitive, Property { slot: Slot::Data(tp.into()), enumerable: false, writable: false, configurable: true });
    }
}
