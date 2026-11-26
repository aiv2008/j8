use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use moremath::{Executor, ThreadMulti};
use j8framework::{Platform, Task};

const LOCALE_VARS: [&str; 3] = ["LC_ALL", "LC_MESSAGES", "LANG"];

const ZONEINFO: &str = "zoneinfo/";

const LOCALTIME: &str = "/etc/localtime";

pub struct DefaultPlatform {
    pool: ThreadMulti,
    start: Instant,
    host: Host,
}

struct Host {
    locale: Option<String>,
    time_zone: Option<String>,
}

impl Host {
    fn detect() -> Self {
        Self { locale: Self::locale(), time_zone: Self::time_zone() }
    }

    fn locale() -> Option<String> {
        let posix = LOCALE_VARS.iter().find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))?;
        let tag = posix.split(['.', '@']).next().unwrap_or_default().replace('_', "-");
        (!tag.is_empty() && tag != "C" && tag != "POSIX").then_some(tag)
    }

    fn time_zone() -> Option<String> {
        let named = |path: &str| path.rsplit_once(ZONEINFO).map_or_else(|| path.to_owned(), |(_, name)| name.to_owned());
        match std::env::var("TZ") {
            Ok(tz) if !tz.is_empty() => Some(named(tz.trim_start_matches(':'))),
            _ => std::fs::read_link(LOCALTIME).ok().and_then(|p| p.to_str().and_then(|p| p.rsplit_once(ZONEINFO)).map(|(_, name)| name.to_owned())),
        }
    }
}

impl Default for DefaultPlatform {
    fn default() -> Self {
        Self { pool: ThreadMulti::default(), start: Instant::now(), host: Host::detect() }
    }
}

impl From<NonZeroUsize> for DefaultPlatform {
    fn from(n: NonZeroUsize) -> Self {
        Self { pool: ThreadMulti::from(n), start: Instant::now(), host: Host::detect() }
    }
}

impl j8framework::Pool for DefaultPlatform {
    fn threads(&self) -> usize {
        Executor::threads(&self.pool)
    }

    fn run(&self, tasks: Vec<Task<'_>>) {
        self.pool.execute(tasks);
    }
}

impl Platform for DefaultPlatform {
    fn now(&self) -> Duration {
        self.start.elapsed()
    }

    fn wall_clock(&self) -> Duration {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default()
    }

    fn load_module(&self, referrer: &str, specifier: &str) -> Option<(String, String)> {
        let base = std::path::Path::new(referrer).parent().unwrap_or_else(|| std::path::Path::new("."));
        let path = dunce::canonicalize(base.join(specifier)).ok()?;
        let text = std::fs::read_to_string(&path).ok()?;
        Some((path.display().to_string(), text))
    }

    fn locale(&self) -> Option<String> {
        self.host.locale.clone()
    }

    fn time_zone(&self) -> Option<String> {
        self.host.time_zone.clone()
    }

    fn print(&self, line: &str) {
        println!("{line}");
    }
}
