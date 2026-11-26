use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{self, Write};

use crate::{Code, Collect, Error, JsObject, JsString, ObjectKind, Scope, Value, ValueKind};

#[derive(Clone, Copy, Debug, Collect)]
#[collect(no_drop)]
pub struct Captured {
    pub code: Code,
    pub at: u32,
    pub constructor: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CallSite {
    pub function: Box<str>,
    pub script: Box<str>,
    pub line: u32,
    pub column: u32,
    pub constructor: bool,
}

impl CallSite {
    const ANONYMOUS_SCRIPT: &str = "<anonymous>";

    fn write_location(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.script.is_empty() { Self::ANONYMOUS_SCRIPT } else { &self.script })?;
        if self.line != 0 {
            write!(f, ":{}:{}", self.line, self.column)?;
        }
        Ok(())
    }
}

impl fmt::Display for CallSite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.function.is_empty() {
            return self.write_location(f);
        }
        if self.constructor {
            f.write_str("new ")?;
        }
        write!(f, "{} (", self.function)?;
        self.write_location(f)?;
        f.write_str(")")
    }
}

const STACK: &str = "stack";
const STACK_TRACE_LIMIT: &str = "stackTraceLimit";
const FRAME: &str = "\n    at ";

impl<'scope> Scope<'scope, '_> {
    pub fn stack_trace(&self, limit: usize) -> Vec<CallSite> {
        self.invoke.capture(self, limit).iter().map(|c| self.invoke.call_site(self, c)).collect()
    }

    pub fn capture(&self) -> Vec<Captured> {
        self.invoke.capture(self, self.stack_trace_limit())
    }

    pub fn stack_trace_limit(&self) -> usize {
        let limit = self.context.error_ctor.borrow().get_own(self.mc, STACK_TRACE_LIMIT).and_then(|p| p.value());
        match limit.and_then(Value::as_number) {
            #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "a positive limit, saturating")]
            Some(n) if n > 0.0 => n as usize,
            _ => 0,
        }
    }

    pub fn stack_frames(&self, frames: &[Captured]) -> String {
        let mut text = String::new();
        for c in frames {
            let _ = write!(text, "{FRAME}{}", self.invoke.call_site(self, c));
        }
        text
    }

    pub fn error_stack(&mut self, error: JsObject<'scope>) -> Result<JsString<'scope>, Error> {
        let header = self.error_text(error.into())?;
        let frames = match &error.borrow().kind {
            ObjectKind::Error(frames) if !frames.is_empty() => self.stack_frames(frames),
            _ => return Ok(header),
        };
        Ok(self.string(&alloc::format!("{}{frames}", &**header)))
    }

    pub fn stack_text(&mut self, thrown: Value<'scope>) -> Result<JsString<'scope>, Error> {
        if thrown.as_object().is_some()
            && let Some(s) = self.get_named(thrown, STACK)?.as_string()
        {
            return Ok(s);
        }
        self.to_string(thrown)
    }

    pub fn error_text(&mut self, error: Value<'scope>) -> Result<JsString<'scope>, Error> {
        let name = match (self.get_named(error, "name")?).kind() {
            ValueKind::Undefined => self.string("Error"),
            v => self.to_string(Value::from(v))?,
        };
        let msg = match (self.get_named(error, "message")?).kind() {
            ValueKind::Undefined => self.string(""),
            v => self.to_string(Value::from(v))?,
        };
        Ok(match (name.is_empty(), msg.is_empty()) {
            (_, true) => name,
            (true, false) => msg,
            (false, false) => self.string(&alloc::format!("{}: {}", &**name, &**msg)),
        })
    }
}
