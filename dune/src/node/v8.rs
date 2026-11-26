use j8::frame::{IterKind, JsObject, ObjectKind, Settled, Value, ValueKind};

pub struct V8;

#[derive(Clone, Copy)]
enum Kind {
    Ordinary,
    Arguments,
    Error,
    Array,
    Function,
    Generator,
    Promise,
    RegExp,
    BooleanObject,
    NumberObject,
    StringObject,
    SymbolObject,
    BigIntObject,
    Map,
    WeakMap,
    Set,
    WeakSet,
    MapIterator,
    SetIterator,
    Proxy,
    ArrayBuffer,
    Date,
    External,
    ModuleNamespace,
    WeakRef,
    TypedArray,
    DataView,
}

impl Kind {
    const fn name(self) -> &'static str {
        match self {
            Self::Ordinary => "Object",
            Self::Arguments => "Arguments",
            Self::Error => "Error",
            Self::Array => "Array",
            Self::Function => "Function",
            Self::Generator => "Generator",
            Self::Promise => "Promise",
            Self::RegExp => "RegExp",
            Self::BooleanObject => "BooleanObject",
            Self::NumberObject => "NumberObject",
            Self::StringObject => "StringObject",
            Self::SymbolObject => "SymbolObject",
            Self::BigIntObject => "BigIntObject",
            Self::Map => "Map",
            Self::WeakMap => "WeakMap",
            Self::Set => "Set",
            Self::WeakSet => "WeakSet",
            Self::MapIterator => "MapIterator",
            Self::SetIterator => "SetIterator",
            Self::Proxy => "Proxy",
            Self::ArrayBuffer => "ArrayBuffer",
            Self::Date => "Date",
            Self::External => "External",
            Self::ModuleNamespace => "ModuleNamespace",
            Self::WeakRef => "WeakRef",
            Self::TypedArray => "TypedArray",
            Self::DataView => "DataView",
        }
    }
}

impl From<&ObjectKind<'_>> for Kind {
    fn from(kind: &ObjectKind<'_>) -> Self {
        match kind {
            ObjectKind::Arguments(_) => Self::Arguments,
            ObjectKind::Error(_) => Self::Error,
            ObjectKind::Array => Self::Array,
            ObjectKind::Function(_) | ObjectKind::Native(_) | ObjectKind::Bound(_) | ObjectKind::Resolver { .. } | ObjectKind::NativeClosure(_) => Self::Function,
            ObjectKind::Coroutine(c) if c.promise.is_none() || c.requests.is_some() => Self::Generator,
            ObjectKind::Promise { .. } => Self::Promise,
            ObjectKind::RegExp(_) => Self::RegExp,
            ObjectKind::Primitive(v) => match v.kind() {
                ValueKind::Bool(_) => Self::BooleanObject,
                ValueKind::Number(_) => Self::NumberObject,
                ValueKind::String(_) => Self::StringObject,
                ValueKind::Symbol(_) => Self::SymbolObject,
                ValueKind::BigInt(_) => Self::BigIntObject,
                _ => Self::Ordinary,
            },
            ObjectKind::Map(c) if c.weak => Self::WeakMap,
            ObjectKind::Map(_) => Self::Map,
            ObjectKind::Set(c) if c.weak => Self::WeakSet,
            ObjectKind::Set(_) => Self::Set,
            ObjectKind::CollectionIterator { set: true, .. } => Self::SetIterator,
            ObjectKind::CollectionIterator { .. } => Self::MapIterator,
            ObjectKind::Proxy(_) => Self::Proxy,
            ObjectKind::ArrayBuffer(_) => Self::ArrayBuffer,
            ObjectKind::Date(_) => Self::Date,
            ObjectKind::Host(_) => Self::External,
            ObjectKind::ModuleNamespace(_) => Self::ModuleNamespace,
            ObjectKind::WeakRef(_) => Self::WeakRef,
            ObjectKind::TypedArray(..) => Self::TypedArray,
            ObjectKind::DataView(_) => Self::DataView,
            _ => Self::Ordinary,
        }
    }
}

#[derive(Clone, Copy)]
enum PromiseState {
    Pending,
    Fulfilled,
    Rejected,
}

impl From<PromiseState> for u32 {
    fn from(s: PromiseState) -> Self {
        match s {
            PromiseState::Pending => 0,
            PromiseState::Fulfilled => 1,
            PromiseState::Rejected => 2,
        }
    }
}

#[j8::object("v8")]
impl V8 {
    fn kind(v: Value<'_>) -> Option<&'static str> {
        v.as_object().map(|o| Kind::from(&o.borrow().kind).name())
    }

    fn promise(v: Value<'_>) -> Option<Vec<Value<'_>>> {
        let o = v.as_object()?;
        let ObjectKind::Promise { state, .. } = &o.borrow().kind else { return None };
        let (s, result) = match *state {
            Settled::Pending => (PromiseState::Pending, None),
            Settled::Fulfilled(r) => (PromiseState::Fulfilled, Some(r)),
            Settled::Rejected(r) => (PromiseState::Rejected, Some(r)),
        };
        Some(std::iter::once(Value::from(f64::from(u32::from(s)))).chain(result).collect())
    }

    fn proxy(v: Value<'_>) -> Option<Vec<Value<'_>>> {
        let o = v.as_object()?;
        let ObjectKind::Proxy(p) = &o.borrow().kind else { return None };
        Some([p.target, p.handler].into_iter().map(|x| x.map_or(Value::Null, Value::from)).collect())
    }

    fn entries(v: Value<'_>) -> Option<Vec<Value<'_>>> {
        let o = v.as_object()?;
        let kind = &o.borrow().kind;
        let (target, from, mode) = match kind {
            ObjectKind::Map(_) => (o, 0, Some(IterKind::Entries)),
            ObjectKind::Set(_) => (o, 0, Some(IterKind::Values)),
            ObjectKind::CollectionIterator { target: Some(t), index, kind, .. } => (*t, *index, Some(*kind)),
            ObjectKind::CollectionIterator { target: None, .. } => (o, 0, None),
            _ => return None,
        };
        let Some(mode) = mode else { return Some(vec![Value::from(false)]) };
        let pairs = matches!((kind, mode), (ObjectKind::Map(_), _) | (ObjectKind::CollectionIterator { .. }, IterKind::Entries));
        Some(std::iter::once(Value::from(pairs)).chain(Self::flatten(target, from, mode)).collect())
    }

    #[js(skip)]
    fn flatten(target: JsObject<'_>, from: usize, mode: IterKind) -> Vec<Value<'_>> {
        let state = target.borrow();
        let (ObjectKind::Map(c) | ObjectKind::Set(c)) = &state.kind else { return Vec::new() };
        let live = c.entries().iter().skip(from).flatten();
        match mode {
            IterKind::Keys => live.map(|&(k, _)| k).collect(),
            IterKind::Values => live.map(|&(_, v)| v).collect(),
            IterKind::Entries => live.flat_map(|&(k, v)| [k, v]).collect(),
        }
    }
}
