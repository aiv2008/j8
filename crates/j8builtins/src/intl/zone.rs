use alloc::format;
use alloc::string::String;

use icu_time::TimeZone as ZoneId;

const UTC: &str = "UTC";
const SECONDS_PER_MINUTE: i32 = 60;
const MINUTES_PER_HOUR: i32 = 60;
const HOURS_MAX: i32 = 23;

#[derive(Clone)]
pub(crate) enum Zone {
    Utc,
    Offset(i32),
    Named(String, jiff::tz::TimeZone),
}

impl Zone {
    fn offset_minutes(s: &str) -> Option<i32> {
        let (sign, rest) = match s.as_bytes().first()? {
            b'+' => (1, &s[1..]),
            b'-' => (-1, &s[1..]),
            _ => return None,
        };
        let digits: String = match rest.len() {
            2 | 4 => String::from(rest),
            5 if rest.as_bytes()[2] == b':' => format!("{}{}", &rest[..2], &rest[3..]),
            _ => return None,
        };
        if !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let hours: i32 = digits[..2].parse().ok()?;
        let minutes: i32 = digits.get(2..).filter(|m| !m.is_empty()).map_or(Some(0), |m| m.parse().ok())?;
        (hours <= HOURS_MAX && minutes < MINUTES_PER_HOUR).then_some(sign * (hours * MINUTES_PER_HOUR + minutes))
    }

    pub(crate) fn parse(name: &str) -> Option<Self> {
        if let Some(m) = Self::offset_minutes(name) {
            return Some(Self::Offset(m));
        }
        if name.eq_ignore_ascii_case(UTC) {
            return Some(Self::Utc);
        }
        if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"/_-+".contains(&b)) {
            return None;
        }
        let tz = jiff::tz::db().get(name).ok()?;
        let canonical = tz.iana_name().map_or_else(|| String::from(name), String::from);
        Some(Self::Named(canonical, tz))
    }

    pub(crate) fn names() -> alloc::vec::Vec<String> {
        let mut names: alloc::vec::Vec<String> = jiff::tz::db().available().map(|n| String::from(n.as_str())).filter(|n| n != "Factory").collect();
        names.push(String::from(UTC));
        names.sort();
        names.dedup();
        names
    }

    pub(crate) fn host(platform_zone: Option<String>) -> Self {
        platform_zone.and_then(|z| Self::parse(&z)).unwrap_or(Self::Utc)
    }

    pub(crate) fn identifier(&self) -> String {
        match self {
            Self::Utc => String::from(UTC),
            Self::Offset(m) => {
                let sign = if *m < 0 { '-' } else { '+' };
                format!("{sign}{:02}:{:02}", m.abs() / MINUTES_PER_HOUR, m.abs() % MINUTES_PER_HOUR)
            }
            Self::Named(name, _) => name.clone(),
        }
    }

    pub(crate) fn offset_seconds(&self, ms: f64) -> i32 {
        match self {
            Self::Utc => 0,
            Self::Offset(m) => m * SECONDS_PER_MINUTE,
            Self::Named(_, tz) => {
                #[expect(clippy::cast_possible_truncation, reason = "time values are within ±8.64e15")]
                let ms = ms as i64;
                jiff::Timestamp::from_millisecond(ms).map_or(0, |t| tz.to_offset(t).seconds())
            }
        }
    }

    pub(crate) fn abbreviation(&self, t: f64) -> Option<String> {
        match self {
            Self::Utc => None,
            Self::Offset(_) => Some(self.identifier()),
            Self::Named(_, tz) => {
                #[expect(clippy::cast_possible_truncation, reason = "time values are within ±8.64e15")]
                let ms = t as i64;
                jiff::Timestamp::from_millisecond(ms).ok().map(|ts| String::from(tz.to_offset_info(ts).abbreviation()))
            }
        }
    }

    pub(crate) fn id(&self) -> ZoneId {
        match self {
            Self::Utc => ZoneId::from_iana_id("Etc/UTC"),
            Self::Offset(_) => ZoneId::UNKNOWN,
            Self::Named(name, _) => ZoneId::from_iana_id(name),
        }
    }
}
