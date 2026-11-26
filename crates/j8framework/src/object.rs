use crate::ValueKind;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::fmt;

use core::ops::{Deref, DerefMut};

use crate::{CellMut, CellRef, Collect, ScopeLocal, GcCell, GcSlot, Mutation, ValueCell};

use crate::{Scope, Backing, Error, FuncId, Key, Value};

pub type JsString<'scope> = ScopeLocal<'scope, crate::text::JsStr<'scope>>;

#[derive(Debug, Collect)]
#[collect(require_static)]
pub struct SymbolData {
    pub description: Option<Box<str>>,
    pub private: bool,
}

pub type JsSymbol<'scope> = ScopeLocal<'scope, SymbolData>;

#[derive(Debug, Collect)]
#[collect(require_static)]
pub struct BigIntData(pub num_bigint::BigInt);

pub type JsBigInt<'scope> = ScopeLocal<'scope, BigIntData>;

#[derive(Clone, Copy, Debug, Collect)]
#[collect(no_drop)]
pub struct RegExpData<'scope> {
    pub source: JsString<'scope>,
    pub flags: JsString<'scope>,
    pub compiled: ScopeLocal<'scope, crate::Static<Box<dyn core::any::Any + Send + Sync>>>,
    pub legacy: bool,
}

#[derive(Clone, Copy, Collect)]
#[collect(no_drop)]
pub struct RegExpShared<'scope>(ScopeLocal<'scope, Fast<'scope>>);

pub type JsObject<'scope> = ScopeLocal<'scope, Object<'scope>>;

pub type JsEnv<'scope> = ScopeLocal<'scope, Env<'scope>>;

pub type NativeFn = for<'scope, 'x> fn(&mut Scope<'scope, 'x>, Value<'scope>, &[Value<'scope>]) -> Result<Value<'scope>, Error>;

#[derive(Debug, Collect)]
#[collect(no_drop)]
pub struct Env<'scope> {
    slots: EnvSlots<'scope>,
    pub parent: Option<JsEnv<'scope>>,
    #[collect(require_static)]
    pub shape: Option<ShapeId>,
    pub extension: Option<JsObject<'scope>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShapeId(pub u32);

const INLINE_SLOTS: usize = 4;

#[derive(Debug, Collect)]
#[collect(no_drop)]
#[repr(C, u8)]
enum EnvSlots<'scope> {
    Inline { len: u8, cells: [ValueCell<'scope>; INLINE_SLOTS] },
    Boxed(Box<[ValueCell<'scope>]>),
}

impl<'scope> EnvSlots<'scope> {
    fn new(n: usize, mut init: impl FnMut(usize) -> Value<'scope>) -> Self {
        match u8::try_from(n) {
            Ok(len) if n <= INLINE_SLOTS => Self::Inline { len, cells: core::array::from_fn(|i| ValueCell::new(if i < n { init(i) } else { Value::Undefined })) },
            _ => Self::Boxed((0..n).map(|i| ValueCell::new(init(i))).collect()),
        }
    }

    #[inline]
    fn cells(&self) -> &[ValueCell<'scope>] {
        match self {
            Self::Inline { len, cells } => &cells[..usize::from(*len)],
            Self::Boxed(b) => b,
        }
    }
}

#[derive(Debug, Default)]
pub struct ContextShape {
    pub kind: ContextKind,
    pub names: Box<[ContextName]>,
}

#[derive(Debug)]
pub struct ContextName {
    pub name: SmallStr,
    pub constant: bool,
    pub lexical: bool,
    pub parameter: Option<u16>,
    pub function_name: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ContextKind {
    #[default]
    Function,
    Block,
    Catch,
    With,
    Script,
    Eval,
    Module,
}

impl ContextShape {
    pub fn slot(&self, name: &str) -> Option<(usize, &ContextName)> {
        self.names.iter().enumerate().find(|(_, n)| *n.name == *name)
    }
}

impl<'scope> Env<'scope> {
    pub const SLOTS_OFFSET: usize = core::mem::offset_of!(Env<'static>, slots);
    pub const PARENT_OFFSET: usize = core::mem::offset_of!(Env<'static>, parent);

    pub fn new(slots: Vec<Value<'scope>>, parent: Option<JsEnv<'scope>>) -> Self {
        let n = slots.len();
        let mut values = slots.into_iter();
        Self { slots: EnvSlots::new(n, |_| values.next().unwrap_or_default()), parent, shape: None, extension: None }
    }

    pub fn context(n: usize, init: Value<'scope>, parent: Option<JsEnv<'scope>>, shape: Option<ShapeId>, extension: Option<JsObject<'scope>>) -> Self {
        Self { slots: EnvSlots::new(n, |_| init), parent, shape, extension }
    }

    pub fn len(&self) -> usize {
        self.slots.cells().len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.cells().is_empty()
    }

    pub fn alloc(self, mc: &Mutation<'scope>) -> JsEnv<'scope> {
        mc.alloc(self)
    }

    #[inline]
    pub fn load(&self, i: usize) -> Value<'scope> {
        self.slots.cells()[i].get()
    }

    pub fn values(&self) -> Vec<Value<'scope>> {
        self.slots.cells().iter().map(ValueCell::get).collect()
    }

    #[inline]
    pub fn set(env: JsEnv<'scope>, mc: &Mutation<'scope>, i: usize, v: Value<'scope>) {
        env.get().slots.cells()[i].set(mc, v);
    }
}

#[derive(Clone, Copy, Debug, Collect)]
#[collect(no_drop)]
pub struct Closure<'scope> {
    pub func: FuncId,
    pub env: Option<JsEnv<'scope>>,
    pub code: Option<Code>,
}

impl<'scope> Closure<'scope> {
    pub fn new(func: FuncId, env: Option<JsEnv<'scope>>) -> Self {
        Self { func, env, code: None }
    }
}

#[derive(Debug)]
pub struct FunctionInfo {
    pub name: SmallStr,
    pub length: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Code(usize);

impl Code {
    pub fn new(handle: crate::Handle<'_, impl crate::Trace>) -> Self {
        Self(handle.id())
    }

    pub fn id(self) -> usize {
        self.0
    }

    pub unsafe fn from_id(id: usize) -> Self {
        Self(id)
    }

    fn erased<'scope>(self) -> crate::Handle<'scope, crate::Erased> {
        unsafe { paragc::Handle::from_id(self.0) }
    }

    #[inline]
    pub unsafe fn info<'e>(self) -> &'e FunctionInfo {
        unsafe { self.get() }
    }

    #[inline]
    pub unsafe fn get<'e, T>(self) -> &'e T {
        unsafe { &*self.erased().get().as_ptr().cast::<T>() }
    }
}

unsafe impl crate::Trace for Code {
    fn trace(&self, t: &mut crate::Tracer<'_>) {
        self.erased().trace(t);
    }

    fn relocate(&mut self, t: &mut crate::Tracer<'_>) {
        self.trace(t);
    }
}

#[derive(Clone, Copy, Debug, Collect)]
#[collect(no_drop)]
pub enum Slot<'scope> {
    Data(Value<'scope>),
    Accessor { get: Option<JsObject<'scope>>, set: Option<JsObject<'scope>> },
}

#[derive(Clone, Copy, Debug, Collect)]
pub struct Accessor<'scope> {
    pub get: Option<JsObject<'scope>>,
    pub set: Option<JsObject<'scope>>,
}

impl<'scope> Slot<'scope> {
    pub fn encode(self, mc: &Mutation<'scope>) -> Value<'scope> {
        match self {
            Slot::Data(v) => v,
            Slot::Accessor { get, set } => Value::accessor(mc.alloc(Accessor { get, set })),
        }
    }

    pub fn decode(v: Value<'scope>) -> Self {
        match v.as_accessor() {
            Some(a) => Slot::Accessor { get: a.get().get, set: a.get().set },
            None => Slot::Data(v),
        }
    }
}

#[derive(Clone, Copy, Debug, Collect)]
#[collect(no_drop)]
pub struct Property<'scope> {
    pub slot: Slot<'scope>,
    pub enumerable: bool,
    pub writable: bool,
    pub configurable: bool,
}

impl<'scope> Property<'scope> {
    pub fn data(v: Value<'scope>) -> Self {
        Self { slot: Slot::Data(v), enumerable: true, writable: true, configurable: true }
    }

    pub fn hidden(v: Value<'scope>) -> Self {
        Self { enumerable: false, ..Self::data(v) }
    }

    pub fn value(&self) -> Option<Value<'scope>> {
        match self.slot {
            Slot::Data(v) => Some(v),
            Slot::Accessor { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Collect)]
#[collect(require_static)]
pub struct Native {
    pub f: NativeFn,
    pub constructor: bool,
    pub forward: Forward,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Forward {
    #[default]
    No,
    Call,
    Apply,
}

impl Native {
    pub fn function(f: NativeFn) -> Self {
        Self { f, constructor: false, forward: Forward::No }
    }

    pub fn constructor(f: NativeFn) -> Self {
        Self { f, constructor: true, forward: Forward::No }
    }
}

impl fmt::Debug for Native {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Native")
    }
}

const _: () = assert!(core::mem::offset_of!(Native, f) == 0);

#[derive(Debug, Collect)]
#[collect(no_drop)]
pub struct Bound<'scope> {
    pub target: Value<'scope>,
    pub this: Value<'scope>,
    pub args: Vec<Value<'scope>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Collect)]
#[collect(require_static)]
pub enum IterKind {
    Values,
    Keys,
    Entries,
}

#[derive(Debug, Collect)]
#[collect(no_drop)]
pub enum IteratorState<'scope> {
    Array { target: Option<Value<'scope>>, index: usize, kind: IterKind },
    String { s: JsString<'scope>, pos: usize },
}

#[derive(Debug, Collect)]
#[collect(no_drop)]
pub struct Suspended<'scope> {
    pub closure: Value<'scope>,
    pub params: Vec<Value<'scope>>,
    pub regs: Vec<Value<'scope>>,
    pub context: Option<JsEnv<'scope>>,
    pub continuation: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Collect)]
#[collect(require_static)]
pub enum CoroutineState {
    Suspended,
    Running,
    Completed,
}

#[derive(Debug, Collect)]
#[collect(no_drop)]
pub struct Coroutine<'scope> {
    pub state: CoroutineState,
    pub frame: Suspended<'scope>,
    pub mode: crate::Resume,
    pub input: Value<'scope>,
    pub waiting: Waiting<'scope>,
    pub settlement: Settlement<'scope>,
    pub promise: Option<JsObject<'scope>>,
    pub requests: Option<alloc::collections::VecDeque<Request<'scope>>>,
}

impl<'scope> Coroutine<'scope> {
    pub fn new(closure: Value<'scope>, promise: Option<JsObject<'scope>>, async_generator: bool) -> Self {
        Self {
            state: CoroutineState::Running,
            frame: Suspended { closure, params: Vec::new(), regs: Vec::new(), context: None, continuation: 0 },
            mode: crate::Resume::Next,
            input: Value::Undefined,
            waiting: Waiting::Nothing,
            settlement: Settlement::Nothing,
            promise,
            requests: async_generator.then(alloc::collections::VecDeque::new),
        }
    }
}

#[derive(Clone, Copy, Debug, Collect)]
#[collect(no_drop)]
pub enum Waiting<'scope> {
    Nothing,
    Await(Value<'scope>),
    Yield(Value<'scope>),
    Return,
    Rethrow,
}

#[derive(Clone, Copy, Debug, Collect)]
#[collect(no_drop)]
pub enum Settlement<'scope> {
    Nothing,
    Resolved(Value<'scope>),
    Rejected(Value<'scope>),
}

#[derive(Clone, Copy, Debug, Collect)]
#[collect(no_drop)]
pub struct Request<'scope> {
    pub mode: crate::Resume,
    pub value: Value<'scope>,
    pub promise: JsObject<'scope>,
}

#[derive(Clone, Copy, Debug, Collect)]
#[collect(no_drop)]
pub enum Settled<'scope> {
    Pending,
    Fulfilled(Value<'scope>),
    Rejected(Value<'scope>),
}

#[derive(Clone, Copy, Debug, Collect)]
#[collect(no_drop)]
pub enum Reaction<'scope> {
    Then { on_fulfilled: Value<'scope>, on_rejected: Value<'scope>, capability: Option<Capability<'scope>> },
    Await { coroutine: JsObject<'scope> },
    Host { hook: HostHook<'scope> },
}

pub struct HostReaction(pub Box<HostReactionFn>);

pub type HostReactionFn = dyn for<'g, 'x> FnOnce(&mut crate::Scope<'g, 'x>, Value<'g>, bool) -> Result<(), crate::Error> + Send + Sync;

impl HostReaction {
    pub fn new(f: impl for<'g, 'x> FnOnce(&mut crate::Scope<'g, 'x>, Value<'g>, bool) -> Result<(), crate::Error> + Send + Sync + 'static) -> Self {
        Self(Box::new(f))
    }
}

impl core::fmt::Debug for HostReaction {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("HostReaction")
    }
}

pub type HostHook<'scope> = ScopeLocal<'scope, crate::Static<j8lockfree::AtomicRefCell<Option<HostReaction>>>>;

#[derive(Clone, Copy, Debug, Collect)]
#[collect(no_drop)]
pub struct Capability<'scope> {
    pub promise: Value<'scope>,
    pub resolve: Value<'scope>,
    pub reject: Value<'scope>,
}

#[derive(Clone, Copy, Debug, Collect)]
#[collect(no_drop)]
pub enum Job<'scope> {
    Reaction { reaction: Reaction<'scope>, value: Value<'scope>, rejected: bool },
    Thenable { promise: JsObject<'scope>, thenable: Value<'scope>, then: Value<'scope> },
    Import {
        #[collect(require_static)]
        module: u32,
        promise: JsObject<'scope>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MapKey {
    Undefined,
    Null,
    Bool(bool),
    Number(u64),
    String(SmallStr),
    BigInt(Box<str>),
    Ref(usize),
}

impl From<Value<'_>> for MapKey {
    fn from(v: Value<'_>) -> Self {
        match v.kind() {
            ValueKind::Undefined | ValueKind::Uninit => MapKey::Undefined,
            ValueKind::Null => MapKey::Null,
            ValueKind::Bool(b) => MapKey::Bool(b),
            ValueKind::Number(x) if x.is_nan() => MapKey::Number(f64::NAN.to_bits()),
            ValueKind::Number(x) => MapKey::Number((x + 0.0).to_bits()),
            ValueKind::String(s) => MapKey::String(crate::SmallStr::from(s.as_str())),
            ValueKind::BigInt(b) => MapKey::BigInt(alloc::format!("{}", b.0).into()),
            ValueKind::Symbol(s) => MapKey::Ref(ScopeLocal::as_ptr(s) as usize),
            ValueKind::Object(o) => MapKey::Ref(ScopeLocal::as_ptr(o).cast::<u8>() as usize),
        }
    }
}

#[derive(Debug)]
pub struct Collection<'scope> {
    entries: Vec<Option<(Value<'scope>, Value<'scope>)>>,
    index: crate::Static<hashbrown::HashMap<MapKey, usize, core::hash::BuildHasherDefault<rustc_hash::FxHasher>>>,
    pub size: usize,
    pub weak: bool,
}

unsafe impl crate::Trace for Collection<'_> {
    fn trace(&self, t: &mut crate::Tracer<'_>) {
        self.entries.trace(t);
    }

    fn relocate(&mut self, t: &mut crate::Tracer<'_>) {
        for (i, entry) in self.entries.iter_mut().enumerate() {
            let Some((k, v)) = entry else { continue };
            let old = matches!(k.kind(), ValueKind::Object(_) | ValueKind::Symbol(_)).then(|| MapKey::from(*k));
            crate::Trace::relocate(k, t);
            crate::Trace::relocate(v, t);
            if let Some(old) = old {
                let new = MapKey::from(*k);
                if new != old {
                    self.index.0.remove(&old);
                    self.index.0.insert(new, i);
                }
            }
        }
    }
}

impl<'scope> Collection<'scope> {
    pub fn new(weak: bool) -> Self {
        Self { entries: Vec::new(), index: crate::Static(hashbrown::HashMap::default()), size: 0, weak }
    }

    pub fn entries(&self) -> &[Option<(Value<'scope>, Value<'scope>)>] {
        &self.entries
    }

    pub fn get(&self, key: Value<'scope>) -> Option<Value<'scope>> {
        let i = *self.index.0.get(&MapKey::from(key))?;
        self.entries[i].map(|(_, v)| v)
    }

    pub fn has(&self, key: Value<'scope>) -> bool {
        self.index.0.contains_key(&MapKey::from(key))
    }

    pub fn set(&mut self, key: Value<'scope>, value: Value<'scope>) {
        let key = match key.kind() {
            ValueKind::Number(x) => Value::Number(x + 0.0),
            k => k.into(),
        };
        match self.index.0.entry(MapKey::from(key)) {
            hashbrown::hash_map::Entry::Occupied(e) => {
                let i = *e.get();
                self.entries[i] = Some((key, value));
            }
            hashbrown::hash_map::Entry::Vacant(e) => {
                e.insert(self.entries.len());
                self.entries.push(Some((key, value)));
                self.size += 1;
            }
        }
    }

    pub fn delete(&mut self, key: Value<'scope>) -> bool {
        match self.index.0.remove(&MapKey::from(key)) {
            Some(i) => {
                self.entries[i] = None;
                self.size -= 1;
                true
            }
            None => false,
        }
    }

    pub fn clear(&mut self) {
        self.entries.iter_mut().for_each(|e| *e = None);
        self.index.0.clear();
        self.size = 0;
    }
}

pub type ClosureFn = for<'scope, 'x> fn(&mut Scope<'scope, 'x>, JsObject<'scope>, Value<'scope>, &[Value<'scope>]) -> Result<Value<'scope>, Error>;

#[derive(Collect)]
#[collect(no_drop)]
pub struct NativeClosure<'scope> {
    #[collect(require_static)]
    pub f: ClosureFn,
    pub slots: Vec<Value<'scope>>,
    #[collect(require_static)]
    pub constructor: bool,
}

impl fmt::Debug for NativeClosure<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeClosure").field("slots", &self.slots.len()).finish()
    }
}

#[derive(Clone, Copy, Debug, Collect)]
#[collect(no_drop)]
pub struct ProxyData<'scope> {
    pub target: Option<JsObject<'scope>>,
    pub handler: Option<JsObject<'scope>>,
    pub callable: bool,
    pub constructor: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Collect)]
#[collect(require_static)]
pub enum HelperKind {
    Map,
    Filter,
    Take,
    Drop,
    FlatMap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Collect)]
#[collect(require_static)]
pub enum HelperState {
    Start,
    Suspended,
    Running,
    Done,
}

#[derive(Clone, Copy, Debug, Collect)]
#[collect(no_drop)]
pub struct IteratorHelper<'scope> {
    pub kind: HelperKind,
    pub state: HelperState,
    pub underlying: crate::IterRecord<'scope>,
    pub f: Value<'scope>,
    pub remaining: f64,
    pub counter: f64,
    pub inner: Option<crate::IterRecord<'scope>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Collect)]
#[collect(require_static)]
pub enum ElementKind {
    Int8,
    Uint8,
    Uint8Clamped,
    Int16,
    Uint16,
    Int32,
    Uint32,
    Float16,
    Float32,
    Float64,
    BigInt64,
    BigUint64,
}

impl ElementKind {
    pub const ALL: [ElementKind; 12] = [
        ElementKind::Int8,
        ElementKind::Uint8,
        ElementKind::Uint8Clamped,
        ElementKind::Int16,
        ElementKind::Uint16,
        ElementKind::Int32,
        ElementKind::Uint32,
        ElementKind::Float16,
        ElementKind::Float32,
        ElementKind::Float64,
        ElementKind::BigInt64,
        ElementKind::BigUint64,
    ];

    pub fn size(self) -> usize {
        match self {
            ElementKind::Int8 | ElementKind::Uint8 | ElementKind::Uint8Clamped => 1,
            ElementKind::Int16 | ElementKind::Uint16 | ElementKind::Float16 => 2,
            ElementKind::Int32 | ElementKind::Uint32 | ElementKind::Float32 => 4,
            ElementKind::Float64 | ElementKind::BigInt64 | ElementKind::BigUint64 => 8,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ElementKind::Int8 => "Int8Array",
            ElementKind::Uint8 => "Uint8Array",
            ElementKind::Uint8Clamped => "Uint8ClampedArray",
            ElementKind::Int16 => "Int16Array",
            ElementKind::Uint16 => "Uint16Array",
            ElementKind::Int32 => "Int32Array",
            ElementKind::Uint32 => "Uint32Array",
            ElementKind::Float16 => "Float16Array",
            ElementKind::Float32 => "Float32Array",
            ElementKind::Float64 => "Float64Array",
            ElementKind::BigInt64 => "BigInt64Array",
            ElementKind::BigUint64 => "BigUint64Array",
        }
    }

    pub fn view_name(self) -> &'static str {
        match self {
            ElementKind::Int8 => "Int8",
            ElementKind::Uint8 | ElementKind::Uint8Clamped => "Uint8",
            ElementKind::Int16 => "Int16",
            ElementKind::Uint16 => "Uint16",
            ElementKind::Int32 => "Int32",
            ElementKind::Uint32 => "Uint32",
            ElementKind::Float16 => "Float16",
            ElementKind::Float32 => "Float32",
            ElementKind::Float64 => "Float64",
            ElementKind::BigInt64 => "BigInt64",
            ElementKind::BigUint64 => "BigUint64",
        }
    }

    pub fn is_bigint(self) -> bool {
        matches!(self, ElementKind::BigInt64 | ElementKind::BigUint64)
    }
}

#[derive(Debug, Collect)]
#[collect(require_static)]
pub struct BufferData {
    pub bytes: Option<Backing>,
    pub max: Option<usize>,
    pub shared: bool,
}

#[derive(Clone, Copy, Debug, Collect)]
#[collect(no_drop)]
pub struct ViewData<'scope> {
    pub buffer: JsObject<'scope>,
    pub offset: usize,
    pub length: Option<usize>,
}

#[derive(Clone, Debug, Collect)]
#[collect(no_drop)]
pub struct ArgumentsMap<'scope> {
    pub env: JsEnv<'scope>,
    pub slots: Vec<Option<u16>>,
}

pub unsafe trait HostObject<'scope>: Send + Sync + 'scope {
    fn trace(&self, t: &mut crate::Tracer<'_>);

    fn relocate(&mut self, t: &mut crate::Tracer<'_>);

    fn find(&self, brand: core::any::TypeId) -> Option<core::ptr::NonNull<()>>;

    fn find_mut(&mut self, brand: core::any::TypeId) -> Option<core::ptr::NonNull<()>>;
}

pub struct Host<'scope>(Box<dyn HostObject<'scope> + 'scope>);

unsafe impl crate::Trace for Host<'_> {
    fn trace(&self, t: &mut crate::Tracer<'_>) {
        self.0.trace(t);
    }

    fn relocate(&mut self, t: &mut crate::Tracer<'_>) {
        self.0.relocate(t);
    }
}

impl fmt::Debug for Host<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Host")
    }
}

impl<'scope> Host<'scope> {
    pub fn new(v: impl HostObject<'scope>) -> Self {
        Self(Box::new(v))
    }

    pub fn find(&self, brand: core::any::TypeId) -> Option<core::ptr::NonNull<()>> {
        self.0.find(brand)
    }

    pub fn find_mut(&mut self, brand: core::any::TypeId) -> Option<core::ptr::NonNull<()>> {
        self.0.find_mut(brand)
    }
}

pub type HostData<'scope> = ScopeLocal<'scope, crate::GcCell<'scope, Host<'scope>>>;

#[derive(Debug, Default, Collect)]
#[collect(no_drop)]
pub enum ObjectKind<'scope> {
    #[default]
    Ordinary,
    Arguments(Option<ArgumentsMap<'scope>>),
    Error(Vec<crate::Captured>),
    Array,
    Function(Closure<'scope>),
    Native(Native),
    Bound(Box<Bound<'scope>>),
    Iterator(IteratorState<'scope>),
    Coroutine(Box<Coroutine<'scope>>),
    Promise { state: Settled<'scope>, reactions: Vec<Reaction<'scope>>, handled: bool },
    Resolver { promise: JsObject<'scope>, reject: bool, done: ScopeLocal<'scope, core::sync::atomic::AtomicBool> },
    RegExp(RegExpData<'scope>),
    Primitive(Value<'scope>),
    Map(Box<Collection<'scope>>),
    Set(Box<Collection<'scope>>),
    CollectionIterator { target: Option<JsObject<'scope>>, index: usize, kind: IterKind, set: bool },
    NativeClosure(NativeClosure<'scope>),
    Proxy(ProxyData<'scope>),
    IteratorHelper(Box<IteratorHelper<'scope>>),
    WrappedIterator(crate::IterRecord<'scope>),
    AccessorPair { get: Option<JsObject<'scope>>, set: Option<JsObject<'scope>> },
    AsyncFromSyncIterator(crate::IterRecord<'scope>),
    ArrayBuffer(Box<BufferData>),
    Date(f64),
    Host(HostData<'scope>),
    ModuleNamespace(Vec<(JsString<'scope>, crate::ResolvedBinding<'scope>)>),
    WeakRef(Value<'scope>),
    FinalizationRegistry { cleanup: Value<'scope>, cells: Vec<(Value<'scope>, Value<'scope>, Value<'scope>)> },
    RegExpStringIterator { matcher: JsObject<'scope>, string: JsString<'scope>, global: bool, unicode: bool, done: bool },
    TypedArray(ElementKind, ViewData<'scope>),
    DataView(ViewData<'scope>),
}

impl ObjectKind<'_> {
    pub fn has_ordinary_properties(&self) -> bool {
        !(matches!(self, ObjectKind::Proxy(_) | ObjectKind::Array | ObjectKind::TypedArray(..) | ObjectKind::ModuleNamespace(_) | ObjectKind::Arguments(_))
            || matches!(self, ObjectKind::Primitive(v) if matches!(v.kind(), ValueKind::String(_))))
    }

    pub fn is_callable(&self) -> bool {
        matches!(self, ObjectKind::Function(_) | ObjectKind::Native(_) | ObjectKind::Bound(_) | ObjectKind::Resolver { .. } | ObjectKind::NativeClosure(_) | ObjectKind::Proxy(ProxyData { callable: true, .. }))
    }
}

pub use crate::text::{HiddenSurrogates, JsStr, Pieces, SmallStr, Units};

#[derive(Collect)]
#[collect(no_drop)]
#[expect(clippy::box_collection, reason = "boxed so an empty one is a single pointer (a Vec itself is three words)")]
pub struct Few<T>(Option<Box<Vec<T>>>);

impl<T> Default for Few<T> {
    fn default() -> Self {
        Self(None)
    }
}

impl<T> core::ops::Deref for Few<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        self.0.as_deref().map_or(&[], |v| v)
    }
}

impl<T> core::ops::DerefMut for Few<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        self.0.as_deref_mut().map_or(&mut [], |v| v)
    }
}

impl<T> Few<T> {
    pub fn push(&mut self, t: T) {
        self.0.get_or_insert_default().push(t);
    }

    pub fn remove(&mut self, i: usize) -> T {
        self.0.as_mut().expect("an index into a non-empty list").remove(i)
    }

    pub fn retain(&mut self, keep: impl FnMut(&T) -> bool) {
        if let Some(v) = &mut self.0 {
            v.retain(keep);
        }
    }

    pub fn pop(&mut self) -> Option<T> {
        self.0.as_mut()?.pop()
    }

    pub fn into_vec(self) -> Vec<T> {
        self.0.map_or_else(Vec::new, |v| *v)
    }
}

impl<T> From<Vec<T>> for Few<T> {
    fn from(v: Vec<T>) -> Self {
        Self((!v.is_empty()).then(|| Box::new(v)))
    }
}

#[derive(Default)]
pub struct ObjectData<'scope> {
    pub kind: ObjectKind<'scope>,
    pub capacity: usize,
    pub elements: Vec<Value<'scope>>,
    pub symbols: Few<(JsSymbol<'scope>, Property<'scope>)>,
    pub privates: Few<(JsSymbol<'scope>, Property<'scope>)>,
    pub proto: Option<JsObject<'scope>>,
    pub sparse_length: u32,
    pub non_extensible: bool,
    pub length_readonly: bool,
}

impl<'scope> ObjectData<'scope> {
    pub fn ordinary(mc: &Mutation<'scope>, proto: Option<JsObject<'scope>>, capacity: usize) -> JsObject<'scope> {
        mc.alloc(Object {
            proto: GcSlot::from(proto),
            call: GcSlot::default(),
            props: crate::Props::with_capacity(mc, capacity),
            elements: crate::ValueArray::default(),
            class: crate::IsoCell::new(Class::ORDINARY | Class::PLAIN | Class::UNIT_ORDINARY),
            non_extensible: crate::IsoCell::new(false),
            length_readonly: crate::IsoCell::new(false),
            flags: crate::IsoCell::new(0),
            sparse_length: crate::IsoCell::new(0),
            shape_root: crate::HandleSlot::default(),
            data: GcCell::empty(),
        })
    }

    pub fn alloc(self, mc: &Mutation<'scope>) -> JsObject<'scope> {
        let Self { kind, capacity, elements, symbols, privates, proto, sparse_length, non_extensible, length_readonly } = self;
        let state = ObjectState { kind, symbols, privates };
        let unit = Class::unit(&state);
        let class = Class::of(&state.kind) | unit.unwrap_or(0) | if state.symbols.is_empty() { 0 } else { Class::SYMBOLS };
        let call = Object::call_of(mc, &state.kind);
        let o = mc.alloc(Object {
            proto: GcSlot::from(proto),
            call: GcSlot::from(call),
            props: crate::Props::with_capacity(mc, capacity),
            elements: crate::ValueArray::default(),
            class: crate::IsoCell::new(class),
            non_extensible: crate::IsoCell::new(non_extensible),
            length_readonly: crate::IsoCell::new(length_readonly),
            flags: crate::IsoCell::new(0),
            sparse_length: crate::IsoCell::new(sparse_length),
            shape_root: crate::HandleSlot::default(),
            data: if unit.is_some() { GcCell::empty() } else { GcCell::new(mc, state) },
        });
        if !elements.is_empty() {
            o.elements.replace(mc, &elements);
        }
        o
    }
}

#[derive(Default, Collect)]
pub struct ObjectState<'scope> {
    pub kind: ObjectKind<'scope>,
    pub symbols: Few<(JsSymbol<'scope>, Property<'scope>)>,
    pub privates: Few<(JsSymbol<'scope>, Property<'scope>)>,
}

impl<'scope> ObjectState<'scope> {
    pub fn get_own_symbol(&self, key: JsSymbol<'scope>) -> Option<Property<'scope>> {
        self.symbols.iter().find_map(|&(k, p)| ScopeLocal::ptr_eq(k, key).then_some(p))
    }

    pub fn define_symbol(&mut self, key: JsSymbol<'scope>, prop: Property<'scope>) {
        match self.symbols.iter_mut().find(|(k, _)| ScopeLocal::ptr_eq(*k, key)) {
            Some((_, p)) => *p = prop,
            None => self.symbols.push((key, prop)),
        }
    }

}

impl fmt::Debug for ObjectState<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            ObjectKind::Ordinary | ObjectKind::Arguments(_) | ObjectKind::AccessorPair { .. } => "Object",
            ObjectKind::Error(_) => "Error",
            ObjectKind::Array => "Array",
            ObjectKind::Function(_) | ObjectKind::Native(_) | ObjectKind::Bound(_) | ObjectKind::Resolver { .. } | ObjectKind::NativeClosure(_) => "Function",
            ObjectKind::Proxy(_) => "Proxy",
            ObjectKind::ArrayBuffer(_) => "ArrayBuffer",
            ObjectKind::Date(_) => "Date",
            ObjectKind::Host(_) => "Host",
            ObjectKind::ModuleNamespace(_) => "Module",
            ObjectKind::WeakRef(_) => "WeakRef",
            ObjectKind::FinalizationRegistry { .. } => "FinalizationRegistry",
            ObjectKind::RegExpStringIterator { .. } => "RegExp String Iterator",
            ObjectKind::TypedArray(..) => "TypedArray",
            ObjectKind::DataView(_) => "DataView",
            ObjectKind::Iterator(_) | ObjectKind::CollectionIterator { .. } | ObjectKind::IteratorHelper(_) | ObjectKind::WrappedIterator(_) | ObjectKind::AsyncFromSyncIterator(_) => "Iterator",
            ObjectKind::Coroutine(_) => "Generator",
            ObjectKind::Promise { .. } => "Promise",
            ObjectKind::RegExp(_) => "RegExp",
            ObjectKind::Primitive(_) => "Primitive",
            ObjectKind::Map(_) => "Map",
            ObjectKind::Set(_) => "Set",
        };
        f.write_str(kind)
    }
}

#[derive(Collect)]
#[repr(C, u8)]
enum Fast<'scope> {
    Closure(Closure<'scope>),
    Native(Native),
    RegExp(RegExpData<'scope>),
}

struct Class;

impl Class {
    const ARRAY: u8 = 1;
    const ORDINARY: u8 = 1 << 1;
    const PLAIN: u8 = 1 << 2;
    const CALLABLE: u8 = 1 << 3;
    const SYMBOLS: u8 = 1 << 6;
    const WIDE: u8 = 1 << 7;
    const UNIT_SHIFT: u32 = 4;
    const UNIT_MASK: u8 = 0b11 << Self::UNIT_SHIFT;
    const UNIT_ORDINARY: u8 = 1 << Self::UNIT_SHIFT;
    const UNIT_ERROR: u8 = 2 << Self::UNIT_SHIFT;
    const UNIT_DERIVED: u8 = 3 << Self::UNIT_SHIFT;

    fn unit(state: &ObjectState<'_>) -> Option<u8> {
        if !state.symbols.is_empty() || !state.privates.is_empty() {
            return None;
        }
        match &state.kind {
            ObjectKind::Ordinary => Some(Self::UNIT_ORDINARY),
            ObjectKind::Error(frames) if frames.is_empty() => Some(Self::UNIT_ERROR),
            ObjectKind::Array | ObjectKind::Function(_) | ObjectKind::Native(_) | ObjectKind::RegExp(_) => Some(Self::UNIT_DERIVED),
            _ => None,
        }
    }

    fn unit_state<'scope>(class: u8, fast: Option<&Fast<'scope>>) -> ObjectState<'scope> {
        let kind = match (class & Self::UNIT_MASK, fast) {
            (Self::UNIT_ERROR, _) => ObjectKind::Error(Vec::new()),
            (Self::UNIT_DERIVED, Some(Fast::Closure(c))) => ObjectKind::Function(*c),
            (Self::UNIT_DERIVED, Some(Fast::Native(n))) => ObjectKind::Native(*n),
            (Self::UNIT_DERIVED, Some(Fast::RegExp(r))) => ObjectKind::RegExp(*r),
            (Self::UNIT_DERIVED, None) => ObjectKind::Array,
            _ => ObjectKind::Ordinary,
        };
        ObjectState { kind, ..ObjectState::default() }
    }

    fn of(kind: &ObjectKind<'_>) -> u8 {
        let bit = |b: bool, v: u8| if b { v } else { 0 };
        bit(matches!(kind, ObjectKind::Array), Self::ARRAY)
            | bit(kind.has_ordinary_properties(), Self::ORDINARY)
            | bit(!matches!(kind, ObjectKind::Proxy(_) | ObjectKind::TypedArray(..) | ObjectKind::ModuleNamespace(_)), Self::PLAIN)
            | bit(kind.is_callable(), Self::CALLABLE)
    }
}

#[crate::heap]
pub struct Object<'scope> {
    #[mutable(set = write_proto)]
    proto: Option<JsObject<'scope>>,
    call: GcSlot<'scope, Fast<'scope>>,
    props: crate::Props<'scope>,
    elements: crate::ValueArray<'scope>,
    class: crate::IsoCell<u8>,
    #[mutable]
    non_extensible: bool,
    #[mutable]
    length_readonly: bool,
    #[mutable]
    flags: u8,
    #[mutable]
    sparse_length: u32,
    shape_root: crate::HandleSlot<'scope, crate::Shape<'scope>>,
    data: GcCell<'scope, ObjectState<'scope>>,
}

const _: () = assert!(!core::mem::needs_drop::<Object<'static>>(), "objects need no destructor: sweeping skips them");
const _: () = assert!(core::mem::size_of::<Object<'static>>() == 200);
const _: () = assert!(core::mem::offset_of!(Object<'static>, proto) == 0);
const _: () = assert!(core::mem::offset_of!(Object<'static>, elements) == 136);
const _: () = assert!(core::mem::offset_of!(Object<'static>, sparse_length) == 192);
const _: () = assert!(core::mem::offset_of!(Object<'static>, class) == 196);
const _: () = assert!(core::mem::offset_of!(Object<'static>, non_extensible) == 197);
const _: () = assert!(core::mem::offset_of!(Object<'static>, length_readonly) == 198);
const _: () = assert!(core::mem::offset_of!(Object<'static>, flags) == 199);
const _: () = assert!(Class::ARRAY == 1, "compiled code tests the array bit as bit 32 of the word at the sparse length");

impl fmt::Debug for Object<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.props.entries().into_iter().map(|(k, _)| k)).finish()?;
        fmt::Debug::fmt(&self.data, f)
    }
}

pub struct ObjRef<'a, 'scope> {
    obj: &'a Object<'scope>,
    data: core::cell::OnceCell<StateRef<'a, 'scope>>,
}

enum StateRef<'a, 'scope> {
    Cell(CellRef<'a, ObjectState<'scope>>),
    Unit(core::mem::ManuallyDrop<ObjectState<'scope>>),
}

impl<'scope> Deref for ObjRef<'_, 'scope> {
    type Target = ObjectState<'scope>;

    fn deref(&self) -> &ObjectState<'scope> {
        match self.data.get_or_init(|| self.obj.state_ref()) {
            StateRef::Cell(c) => c,
            StateRef::Unit(u) => u,
        }
    }
}

pub struct ObjMut<'a, 'scope> {
    obj: &'a Object<'scope>,
    mc: &'a Mutation<'scope>,
    data: core::cell::OnceCell<CellMut<'a, ObjectState<'scope>>>,
}

impl<'scope> ObjMut<'_, 'scope> {
    fn cell(&self) -> &CellMut<'_, ObjectState<'scope>> {
        self.data.get_or_init(|| {
            let (class, fast) = (self.obj.class(), self.obj.fast());
            self.obj.data.try_borrow_mut_or(self.mc, || Class::unit_state(class, fast)).expect("object state already borrowed")
        })
    }
}

impl<'scope> Deref for ObjMut<'_, 'scope> {
    type Target = ObjectState<'scope>;

    fn deref(&self) -> &ObjectState<'scope> {
        self.cell()
    }
}

impl<'scope> DerefMut for ObjMut<'_, 'scope> {
    fn deref_mut(&mut self) -> &mut ObjectState<'scope> {
        self.cell();
        self.data.get_mut().expect("just borrowed")
    }
}

impl Drop for ObjMut<'_, '_> {
    fn drop(&mut self) {
        if let Some(d) = self.data.take() {
            let old = self.obj.class();
            let new = Class::of(&d.kind) | (old & Class::WIDE) | if d.symbols.is_empty() { 0 } else { Class::SYMBOLS };
            self.obj.class.set(new);
            if (old ^ new) & !(Class::WIDE | Class::SYMBOLS) != 0 {
                self.obj.bump_if_watched();
            }
            let call = Object::call_of(self.mc, &d.kind);
            drop(d);
            if call.is_some() || self.obj.call.get().is_some() {
                self.obj.call.set(self.mc.gc_scope(), call);
            }
        }
    }
}

macro_rules! forward_reads {
    ($($guard:ident)*) => {$(
        impl<'a, 'scope> $guard<'a, 'scope> {
            pub fn object(&self) -> &'a Object<'scope> {
                self.obj
            }
            pub fn proto(&self) -> Option<JsObject<'scope>> {
                self.obj.proto()
            }
            pub fn props(&self) -> &'a crate::Props<'scope> {
                &self.obj.props
            }
            pub fn elements(&self) -> &'a crate::ValueArray<'scope> {
                &self.obj.elements
            }
            pub fn non_extensible(&self) -> bool {
                self.obj.non_extensible()
            }
            pub fn length_readonly(&self) -> bool {
                self.obj.length_readonly()
            }
            pub fn digit_keys(&self) -> bool {
                self.obj.digit_keys()
            }
            pub fn sparse_length(&self) -> u32 {
                self.obj.sparse_length()
            }
            pub fn array_length(&self) -> Option<u32> {
                self.obj.array_length()
            }
            pub fn sparse_elements(&self) -> Vec<(u32, Value<'scope>)> {
                self.obj.sparse_elements()
            }
            pub fn prop_at(&self, mc: &Mutation<'scope>, at: usize, key: &str) -> Option<Property<'scope>> {
                self.obj.prop_at(mc, at, key)
            }
            pub fn position(&self, key: &str) -> Option<usize> {
                self.obj.position(key)
            }
            pub fn get_own(&self, mc: &Mutation<'scope>, key: &str) -> Option<Property<'scope>> {
                self.obj.get_own(mc, key)
            }
            pub fn own(&self, mc: &Mutation<'scope>, key: &str) -> Option<Value<'scope>> {
                self.obj.own(mc, key)
            }
            pub fn root_shape(&self, mc: &Mutation<'scope>) -> crate::ShapeRef<'scope> {
                self.obj.root_shape(mc)
            }
        }
    )*};
}
forward_reads!(ObjRef ObjMut);

impl<'scope> ObjRef<'_, 'scope> {
    pub fn own_slot(&self, mc: &Mutation<'scope>, key: Key<'_, 'scope>) -> Option<Property<'scope>> {
        match key {
            Key::Str(k) => self.obj.own_str(mc, k),
            Key::Sym(s) => self.get_own_symbol(s),
        }
    }
}

impl<'scope> ObjMut<'_, 'scope> {
    pub fn own_slot(&mut self, key: Key<'_, 'scope>) -> Option<Property<'scope>> {
        match key {
            Key::Str(k) => self.obj.own_str(self.mc, k),
            Key::Sym(s) => self.deref_mut().get_own_symbol(s),
        }
    }

    pub fn set_proto(&self, proto: Option<JsObject<'scope>>) {
        self.obj.set_proto(self.mc, proto);
    }

    pub fn set_non_extensible(&self, v: bool) {
        self.obj.non_extensible.set(v);
    }

    pub fn set_length_readonly(&self, v: bool) {
        self.obj.length_readonly.set(v);
    }

    pub fn write_index(&self, i: u32, value: Value<'scope>) -> bool {
        self.obj.write_index(self.mc, i, value)
    }

    pub fn fill_index(&self, from: u32, to: u32, value: Value<'scope>) -> bool {
        self.obj.fill_index(self.mc, from, to, value)
    }

    pub fn spill(&self, from: usize) {
        self.obj.spill(self.mc, from);
    }

    pub fn set_array_length(&self, n: u32) {
        self.obj.set_array_length(self.mc, n);
    }

    pub fn set_at(&self, at: usize, value: Value<'scope>) {
        self.obj.set_at(self.mc, at, value);
    }

    pub fn push_to(&self, to: crate::ShapeRef<'scope>, slot: Slot<'scope>) {
        self.obj.push_to(self.mc, to, slot);
    }

    pub fn set(&self, key: &str, value: Value<'scope>) {
        self.obj.set(self.mc, key, value);
    }

    pub fn define(&self, key: &str, prop: Property<'scope>) {
        self.obj.define(self.mc, key, prop);
    }

    pub fn remove(&self, key: &str) -> bool {
        self.obj.remove(self.mc, key)
    }

    pub fn store(&mut self, key: Key<'_, 'scope>, prop: Property<'scope>) {
        match key {
            Key::Sym(s) => self.deref_mut().define_symbol(s, prop),
            Key::Str(k) => self.obj.store_str(self.mc, k, prop),
        }
    }

    pub fn unstore(&mut self, key: Key<'_, 'scope>) {
        match key {
            Key::Sym(s) => self.deref_mut().symbols.retain(|(k, _)| !ScopeLocal::ptr_eq(*k, s)),
            Key::Str(k) => self.obj.unstore_str(self.mc, k),
        }
    }

    pub fn ordinary_put(&mut self, key: Key<'_, 'scope>, value: Value<'scope>) -> Option<bool> {
        if !self.obj.has_ordinary_properties() {
            return None;
        }
        match key {
            Key::Str(k) => self.obj.put_str(self.mc, k, value),
            Key::Sym(s) => {
                let non_extensible = self.obj.non_extensible();
                let d: &mut ObjectState<'scope> = self;
                match d.symbols.iter_mut().find(|(k, _)| ScopeLocal::ptr_eq(*k, s)).map(|(_, p)| (p.writable, &mut p.slot)) {
                    Some((true, Slot::Data(v))) => {
                        *v = value;
                        Some(true)
                    }
                    Some((false, Slot::Data(_))) => Some(false),
                    Some(_) => None,
                    None if non_extensible => Some(false),
                    None => {
                        d.define_symbol(s, Property::data(value));
                        Some(true)
                    }
                }
            }
        }
    }
}

impl<'scope> Object<'scope> {
    pub const PROTO_OFFSET: usize = core::mem::offset_of!(Object<'static>, proto);
    pub const CALL_OFFSET: usize = core::mem::offset_of!(Object<'static>, call);
    pub const PROPS_OFFSET: usize = core::mem::offset_of!(Object<'static>, props);
    pub const CLASS_OFFSET: usize = core::mem::offset_of!(Object<'static>, class);
    pub const PLAIN_BIT: u8 = Class::PLAIN;
    pub const FAST_NATIVE_TAG: u8 = 1;
    pub const FAST_NATIVE_FN: usize = 8;

    pub fn borrow(&self) -> ObjRef<'_, 'scope> {
        ObjRef { obj: self, data: core::cell::OnceCell::new() }
    }

    pub fn try_borrow(&self) -> Option<ObjRef<'_, 'scope>> {
        let r = self.borrow();
        let d = if self.data.has_value() { StateRef::Cell(self.data.try_borrow()?) } else { StateRef::Unit(core::mem::ManuallyDrop::new(Class::unit_state(self.class(), self.fast()))) };
        let _ = r.data.set(d);
        Some(r)
    }

    fn state_ref(&self) -> StateRef<'_, 'scope> {
        if self.data.has_value() { StateRef::Cell(self.data.borrow()) } else { StateRef::Unit(core::mem::ManuallyDrop::new(Class::unit_state(self.class(), self.fast()))) }
    }

    pub fn borrow_mut<'a>(&'a self, mc: &'a Mutation<'scope>) -> ObjMut<'a, 'scope> {
        ObjMut { obj: self, mc, data: core::cell::OnceCell::new() }
    }

    fn call_of(mc: &Mutation<'scope>, kind: &ObjectKind<'scope>) -> Option<ScopeLocal<'scope, Fast<'scope>>> {
        match kind {
            ObjectKind::Function(c) => Some(mc.alloc(Fast::Closure(*c))),
            ObjectKind::Native(n) => Some(mc.alloc(Fast::Native(*n))),
            ObjectKind::RegExp(r) => Some(mc.alloc(Fast::RegExp(*r))),
            _ => None,
        }
    }

    fn fast(&self) -> Option<&'scope Fast<'scope>> {
        self.call.get().map(ScopeLocal::get)
    }

    #[inline]
    pub fn native(&self) -> Option<Native> {
        match self.fast()? {
            Fast::Native(n) => Some(*n),
            _ => None,
        }
    }

    #[inline]
    pub fn regexp(&self) -> Option<RegExpData<'scope>> {
        match self.fast()? {
            Fast::RegExp(r) => Some(*r),
            _ => None,
        }
    }

    #[inline]
    pub fn has_symbols(&self) -> bool {
        self.class() & Class::SYMBOLS != 0
    }

    #[inline]
    pub fn closure(&self) -> Option<Closure<'scope>> {
        match self.fast()? {
            Fast::Closure(c) => Some(*c),
            _ => None,
        }
    }

    pub fn set_proto(&self, mc: &Mutation<'scope>, proto: Option<JsObject<'scope>>) {
        self.write_proto(mc, proto);
        self.bump_if_watched();
    }

    #[inline]
    pub fn props(&self) -> &crate::Props<'scope> {
        &self.props
    }

    #[inline]
    pub fn elements(&self) -> &crate::ValueArray<'scope> {
        &self.elements
    }

    pub const FLAG_DIGIT: u8 = 1;
    pub const FLAG_WATCHED: u8 = 2;
    pub const SPARSE_OFFSET: usize = core::mem::offset_of!(Object<'static>, sparse_length);
    pub const FLAGS_OFFSET: usize = core::mem::offset_of!(Object<'static>, flags);
    pub const ELEMENTS_OFFSET: usize = core::mem::offset_of!(Object<'static>, elements);

    pub fn digit_keys(&self) -> bool {
        self.flags() & Self::FLAG_DIGIT != 0
    }

    pub fn watched(&self) -> bool {
        self.flags() & Self::FLAG_WATCHED != 0
    }

    pub fn mark_watched(&self) {
        self.flags.set(self.flags() | Self::FLAG_WATCHED);
    }

    fn note_digit(&self) {
        if self.flags() & Self::FLAG_DIGIT == 0 {
            self.flags.set(self.flags() | Self::FLAG_DIGIT);
            self.bump_if_watched();
        }
    }

    fn bump_if_watched(&self) {
        if self.watched() {
            crate::store::bump_indexed();
        }
    }

    pub fn anchors_plain_arrays(&self, root: JsObject<'scope>) -> bool {
        let quiet = |o: &Object<'scope>| o.sparse_length() == 0 && !o.digit_keys();
        self.is_array() && quiet(self) && self.proto().is_some_and(|p| ScopeLocal::ptr_eq(p, root)) && root.has_ordinary_properties() && quiet(&root) && root.proto().is_none()
    }

    fn class(&self) -> u8 {
        self.class.get()
    }

    #[inline]
    pub fn is_array(&self) -> bool {
        self.class() & Class::ARRAY != 0
    }

    #[inline]
    pub fn has_ordinary_properties(&self) -> bool {
        self.class() & Class::ORDINARY != 0
    }

    #[inline]
    pub fn is_plain(&self) -> bool {
        self.class() & Class::PLAIN != 0
    }

    #[inline]
    pub fn is_callable(&self) -> bool {
        self.class() & Class::CALLABLE != 0
    }

    fn raise_sparse_length(&self, n: u32) {
        let old = self.sparse_length.get();
        if n > old {
            self.sparse_length.set(n);
            if old == 0 {
                self.bump_if_watched();
            }
        }
    }

    pub fn array_length(&self) -> Option<u32> {
        self.is_array().then(|| u32::try_from(self.elements.len()).unwrap_or(u32::MAX).max(self.sparse_length()))
    }

    pub fn write_index(&self, mc: &Mutation<'scope>, i: u32, value: Value<'scope>) -> bool {
        if !self.is_array() {
            return false;
        }
        let (at, n) = (i as usize, self.elements.len());
        if at < n {
            self.elements.set(mc, at, value);
            return true;
        }
        if at == n && self.can_append(i) {
            self.elements.push(mc, value);
            return true;
        }
        false
    }

    pub fn fill_index(&self, mc: &Mutation<'scope>, from: u32, to: u32, value: Value<'scope>) -> bool {
        let n = self.elements.len();
        if !self.is_array() || from as usize > n || to == u32::MAX {
            return false;
        }
        if to as usize > n && !self.can_append(u32::try_from(n).unwrap_or(u32::MAX)) {
            return false;
        }
        self.elements.fill(mc, from as usize, to as usize, value);
        true
    }

    fn can_append(&self, i: u32) -> bool {
        !self.non_extensible() && !self.length_readonly() && (self.sparse_length() <= i || !self.digit_keys()) && self.no_indexed_protos()
    }

    pub fn no_indexed_protos(&self) -> bool {
        let mut cur = self.proto();
        while let Some(p) = cur {
            let indexed = if p.is_array() {
                !p.elements.is_empty()
            } else if p.has_ordinary_properties() {
                false
            } else {
                !matches!(p.borrow().kind, ObjectKind::Arguments(_))
            };
            if indexed || p.sparse_length() > 0 || p.digit_keys() {
                return false;
            }
            cur = p.proto();
        }
        true
    }

    pub fn spill(&self, mc: &Mutation<'scope>, from: usize) {
        let n = self.elements.len();
        if !self.is_array() || from >= n {
            return;
        }
        let moved: Vec<Value<'scope>> = (from..n).map(|i| self.elements.get(i).unwrap_or_default()).collect();
        self.elements.truncate(mc, from);
        for (i, v) in (from..).zip(moved) {
            self.push_prop(mc, &alloc::format!("{i}"), Property::data(v));
        }
        self.raise_sparse_length(u32::try_from(n).unwrap_or(u32::MAX));
    }

    pub fn sparse_elements(&self) -> Vec<(u32, Value<'scope>)> {
        let dense = self.elements.len();
        if !self.is_array() || self.sparse_length() as usize <= dense {
            return Vec::new();
        }
        let mut out: Vec<(u32, Value<'scope>)> = self
            .props
            .entries()
            .into_iter()
            .filter_map(|(k, p)| match (crate::ops::index_of_str(&k), p.slot) {
                (Some(i), Slot::Data(v)) if i as usize >= dense => Some((i, v)),
                _ => None,
            })
            .collect();
        out.sort_unstable_by_key(|&(i, _)| i);
        out
    }

    pub fn set_array_length(&self, mc: &Mutation<'scope>, n: u32) {
        if !self.is_array() {
            return;
        }
        let len = n as usize;
        self.elements.truncate(mc, len);
        let dense = self.elements.len();
        if self.sparse_length() > 0 {
            self.props.retain(mc, |k| crate::ops::index_of_str(k).is_none_or(|i| i < n));
        }
        let prev = self.sparse_length();
        let next = if len > dense { n } else { 0 };
        self.sparse_length.set(next);
        if (prev == 0) != (next == 0) {
            self.bump_if_watched();
        }
    }

    pub fn own_str(&self, mc: &Mutation<'scope>, k: &str) -> Option<Property<'scope>> {
        if self.is_array()
            && let Some(i) = crate::ops::index_of_str(k)
            && let Some(v) = self.elements.get(i as usize)
        {
            return Some(Property::data(v));
        }
        self.get_own(mc, k)
    }

    pub fn own_slot(&self, mc: &Mutation<'scope>, key: Key<'_, 'scope>) -> Option<Property<'scope>> {
        match key {
            Key::Str(k) => self.own_str(mc, k),
            Key::Sym(_) if !self.has_symbols() => None,
            Key::Sym(s) => self.borrow().get_own_symbol(s),
        }
    }

    fn put_str(&self, mc: &Mutation<'scope>, k: &str, value: Value<'scope>) -> Option<bool> {
        match self.props.position(k) {
            Some(i) => match (self.props.attrs(i).writable(), self.props.slot(i)) {
                (true, Slot::Data(_)) => {
                    self.props.set_slot(mc, i, Slot::Data(value));
                    Some(true)
                }
                (false, Slot::Data(_)) => Some(false),
                _ => None,
            },
            None if self.non_extensible() => Some(false),
            None => {
                self.store_str(mc, k, Property::data(value));
                Some(true)
            }
        }
    }

    pub fn store_str(&self, mc: &Mutation<'scope>, k: &str, prop: Property<'scope>) {
        let plain = prop.enumerable && prop.writable && prop.configurable;
        let index = if self.is_array() { crate::ops::index_of_str(k) } else { None };
        if let Some(i) = index {
            let (at, n) = (i as usize, self.elements.len());
            let stored = self.get_own(mc, k).is_some();
            match (prop.slot, plain) {
                (Slot::Data(v), true) if at < n => return self.elements.set(mc, at, v),
                (Slot::Data(v), true) if at == n && !stored => return self.elements.push(mc, v),
                _ if at < n => self.spill(mc, at),
                _ => {}
            }
        }
        self.define(mc, k, prop);
        if let Some(i) = index {
            self.raise_sparse_length(i.saturating_add(1));
        }
    }

    pub fn unstore_str(&self, mc: &Mutation<'scope>, k: &str) {
        if self.is_array()
            && let Some(i) = crate::ops::index_of_str(k)
            && (i as usize) < self.elements.len()
        {
            self.spill(mc, i as usize);
        }
        if let Some(i) = self.position(k) {
            self.props.remove(mc, i);
        }
    }

    pub fn prop_at(&self, mc: &Mutation<'scope>, at: usize, key: &str) -> Option<Property<'scope>> {
        let p = self.props.get_if(at, key)?;
        Some(self.resolved(mc, at, p))
    }

    #[inline]
    fn resolved(&self, mc: &Mutation<'scope>, i: usize, mut p: Property<'scope>) -> Property<'scope> {
        if let Slot::Data(v) = p.slot
            && let Some(l) = v.as_lazy()
        {
            let v = l.resolve(self, mc);
            self.props.set_slot(mc, i, Slot::Data(v));
            p.slot = Slot::Data(v);
        }
        p
    }

    pub fn set_at(&self, mc: &Mutation<'scope>, at: usize, value: Value<'scope>) {
        if at < self.props.len() {
            self.props.set_slot(mc, at, Slot::Data(value));
        }
    }

    pub fn position(&self, key: &str) -> Option<usize> {
        self.props.position(key)
    }

    fn push_prop(&self, mc: &Mutation<'scope>, key: &str, prop: Property<'scope>) {
        if key.as_bytes().first().is_some_and(u8::is_ascii_digit) {
            self.note_digit();
        }
        let proto = self.proto();
        self.props.push(mc, key, prop, self.class() & Class::WIDE != 0, || proto.map(|p| p.get().root_shape(mc)));
    }

    pub fn widen(&self) {
        self.class.set(self.class.get() | Class::WIDE);
    }

    pub fn root_shape(&self, mc: &Mutation<'scope>) -> crate::ShapeRef<'scope> {
        match self.shape_root.get() {
            Some(h) => h,
            None => {
                let h = crate::Shape::empty(mc);
                self.shape_root.set(Some(h));
                h
            }
        }
    }

    pub fn push_to(&self, mc: &Mutation<'scope>, to: crate::ShapeRef<'scope>, slot: Slot<'scope>) {
        if to.digit_keys() {
            self.note_digit();
        }
        self.props.push_to(mc, to, slot);
    }

    pub fn fill(&self, mc: &Mutation<'scope>, to: crate::ShapeRef<'scope>, slots: impl IntoIterator<Item = Slot<'scope>>) {
        if to.digit_keys() {
            self.note_digit();
        }
        self.props.fill(mc, to, slots);
    }

    pub fn share_regexp(mc: &Mutation<'scope>, data: RegExpData<'scope>) -> RegExpShared<'scope> {
        RegExpShared(mc.alloc(Fast::RegExp(data)))
    }

    pub fn regexp_with(mc: &Mutation<'scope>, proto: JsObject<'scope>, shared: RegExpShared<'scope>) -> JsObject<'scope> {
        let data = match ScopeLocal::get(shared.0) {
            Fast::RegExp(r) => *r,
            _ => unreachable!("a shared regexp body"),
        };
        let state = ObjectState { kind: ObjectKind::RegExp(data), ..ObjectState::default() };
        let class = Class::of(&state.kind) | Class::unit(&state).unwrap_or(0);
        mc.alloc(Object {
            proto: GcSlot::from(Some(proto)),
            call: GcSlot::from(Some(shared.0)),
            props: crate::Props::default(),
            elements: crate::ValueArray::default(),
            class: crate::IsoCell::new(class),
            non_extensible: crate::IsoCell::new(false),
            length_readonly: crate::IsoCell::new(false),
            flags: crate::IsoCell::new(0),
            sparse_length: crate::IsoCell::new(0),
            shape_root: paragc::HandleSlot::default(),
            data: GcCell::empty(),
        })
    }

    pub fn get_own(&self, mc: &Mutation<'scope>, key: &str) -> Option<Property<'scope>> {
        self.props.lookup(key).map(|(i, p)| self.resolved(mc, i, p))
    }

    pub fn own(&self, mc: &Mutation<'scope>, key: &str) -> Option<Value<'scope>> {
        self.get_own(mc, key).and_then(|p| p.value())
    }

    pub fn set(&self, mc: &Mutation<'scope>, key: &str, value: Value<'scope>) {
        match self.position(key) {
            Some(i) => {
                if self.props.attrs(i).writable() {
                    self.props.set_slot(mc, i, Slot::Data(value));
                }
            }
            None => self.push_prop(mc, key, Property::data(value)),
        }
    }

    pub fn define(&self, mc: &Mutation<'scope>, key: &str, prop: Property<'scope>) {
        match self.position(key) {
            Some(i) => self.props.replace(mc, i, prop),
            None => self.push_prop(mc, key, prop),
        }
    }

    pub fn remove(&self, mc: &Mutation<'scope>, key: &str) -> bool {
        match self.position(key) {
            Some(i) if self.props.attrs(i).configurable() => {
                self.props.remove(mc, i);
                true
            }
            Some(_) => false,
            None => true,
        }
    }
}

impl<'scope> From<ObjectKind<'scope>> for ObjectData<'scope> {
    fn from(kind: ObjectKind<'scope>) -> Self {
        Self { kind, ..Self::default() }
    }
}
