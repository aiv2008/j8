use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use j8::frame::{Scope, Error, Value};

const UNARMED: u64 = u64::MAX;
const MS_PER_S: f64 = 1e3;

#[derive(j8::Trace)]
pub struct Timer {
    #[collect(require_static)]
    armed: AtomicU64,
}

impl Default for Timer {
    fn default() -> Self {
        Self { armed: AtomicU64::new(UNARMED) }
    }
}

#[j8::class]
impl Timer {
    #[js(constructor)]
    fn new() -> Self {
        Self::default()
    }

    fn wait<'scope>(&self, a: &mut Scope<'scope, '_>, ms: f64, keep_alive: Option<bool>) -> Result<Value<'scope>, Error> {
        let (promise, id) = a.after(Duration::from_secs_f64(ms.max(0.0) / MS_PER_S), keep_alive.unwrap_or(true))?;
        let previous = self.armed.swap(id, Ordering::AcqRel);
        if previous != UNARMED {
            a.cancel_after(previous);
        }
        Ok(promise.into())
    }

    fn cancel(&self, a: &mut Scope<'_, '_>) {
        let id = self.armed.swap(UNARMED, Ordering::AcqRel);
        if id != UNARMED {
            a.cancel_after(id);
        }
    }
}
