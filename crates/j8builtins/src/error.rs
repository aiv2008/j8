use j8framework::{Scope, Error, ErrorKind, Key, ObjectKind, Property, Slot, Value};

use crate::helpers::Helpers;
use crate::{Args, Installer};

const STACK_TRACE_LIMIT: f64 = 10.0;

const STACK: &str = "stack";

const ERROR_PROTO: &str = "Cannot assign to this property of Error.prototype";

struct Errors;

impl Errors {
    fn construct<'scope, const K: usize>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let new_target = a.new_target;
        let kind = ErrorKind::ALL[K];
        let all = args;
        let args = if kind == ErrorKind::Aggregate { all.get(1..).unwrap_or(&[]) } else { all };
        let proto = a.proto_from_ctor(new_target, a.context.error_proto(kind))?;
        let e = a.error(kind, "");
        e.borrow_mut(a.mc).set_proto(Some(proto));
        e.borrow_mut(a.mc).remove("message");
        if args.at(0) != Value::Undefined {
            let m = a.to_string(args.at(0))?;
            e.borrow_mut(a.mc).define("message", j8framework::Property::hidden(m.into()));
        }
        let options = args.at(1);
        if let Some(opts) = options.as_object()
            && a.has_key(opts, j8framework::Key::Str("cause"))?
        {
            let cause = a.get_named(options, "cause")?;
            e.borrow_mut(a.mc).define("cause", j8framework::Property::hidden(cause));
        }
        if kind == ErrorKind::Aggregate {
            let list = a.to_list(all.at(0))?;
            let arr = a.array(list);
            e.borrow_mut(a.mc).define("errors", j8framework::Property::hidden(arr.into()));
        }
        Ok(e.into())
    }

    fn to_string<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        if this.as_object().is_none() {
            return Err(a.throw(ErrorKind::Type, "Error.prototype.toString called on non-object"));
        }
        Ok(a.error_text(this)?.into())
    }

    fn capture_stack_trace<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let target = args.at(0);
        let Some(o) = target.as_object() else { return Err(a.throw(ErrorKind::Type, "Invalid argument")) };
        let header = a.error_text(target)?;
        let text = a.string(&alloc::format!("{}{}", &**header, a.stack_frames(&a.capture())));
        o.borrow_mut(a.mc).define(STACK, Property::hidden(text.into()));
        Ok(Value::Undefined)
    }

    fn stack_get<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let Some(o) = this.as_object() else { return Err(a.throw(ErrorKind::Type, "Error.prototype.stack getter called on non-object")) };
        if !matches!(o.borrow().kind, ObjectKind::Error(_)) {
            return Ok(Value::Undefined);
        }
        Ok(a.error_stack(o)?.into())
    }

    fn stack_set<'scope>(a: &mut Scope<'scope, '_>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        if this.as_object().is_none() {
            return Err(a.throw(ErrorKind::Type, "Error.prototype.stack setter called on non-object"));
        }
        let v = args.at(0);
        if v.as_string().is_none() {
            return Err(a.throw(ErrorKind::Type, "Error.prototype.stack must be set to a string"));
        }
        Helpers::ignoring_setter(a, this, (a.context.error_proto(ErrorKind::Error), ERROR_PROTO), Key::Str(STACK), v)
    }
}

impl Installer<'_, '_> {
    pub(crate) fn errors(&mut self) {
        let ctors: [j8framework::NativeFn; ErrorKind::ALL.len()] = [
            Errors::construct::<0>,
            Errors::construct::<1>,
            Errors::construct::<2>,
            Errors::construct::<3>,
            Errors::construct::<4>,
            Errors::construct::<5>,
            Errors::construct::<6>,
            Errors::construct::<7>,
        ];
        let mut base = None;
        for (kind, f) in ErrorKind::ALL.into_iter().zip(ctors) {
            let proto = self.context.error_proto(kind);
            let length = if kind == ErrorKind::Aggregate { 2 } else { 1 };
            let ctor = match kind {
                ErrorKind::Error => self.adopt_constructor(self.context.error_ctor, kind.name(), f, proto, length),
                _ => self.constructor(kind.name(), f, proto, length),
            };
            match base {
                None => {
                    self.method(ctor, "captureStackTrace", Errors::capture_stack_trace, 2);
                    ctor.borrow_mut(self.mc).define("stackTraceLimit", Property::data(Value::from(STACK_TRACE_LIMIT)));
                    base = Some(ctor);
                }
                Some(b) => ctor.set_proto(self.mc, Some(b)),
            }
            self.set(proto, "name", self.text(kind.name()));
            self.set(proto, "message", self.text(""));
        }
        self.method(self.context.error_proto(ErrorKind::Error), "toString", Errors::to_string, 0);
        let get = self.context.named_native(self.mc, Errors::stack_get, &alloc::format!("get {STACK}"), 0);
        let set = self.context.named_native(self.mc, Errors::stack_set, &alloc::format!("set {STACK}"), 1);
        let accessor = Property { slot: Slot::Accessor { get: Some(get), set: Some(set) }, enumerable: false, writable: false, configurable: true };
        self.context.error_proto(ErrorKind::Error).borrow_mut(self.mc).define(STACK, accessor);
    }
}
