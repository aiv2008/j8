use core::fmt;

use core::sync::atomic::{AtomicU64, Ordering};

use crate::{Collect, Cow, ScopeLocal, GcQueue, Mutation};
use crate::GcSlot as Slot;

use crate::{ErrorKind, Job, JsObject, JsString, JsSymbol, Native, NativeFn, ObjectData, ObjectKind, Property, SymbolData, Value};

#[derive(Clone, Copy, Collect)]
#[collect(no_drop)]
pub struct WellKnown<'scope> {
    pub iterator: JsSymbol<'scope>,
    pub async_iterator: JsSymbol<'scope>,
    pub has_instance: JsSymbol<'scope>,
    pub to_primitive: JsSymbol<'scope>,
    pub to_string_tag: JsSymbol<'scope>,
    pub species: JsSymbol<'scope>,
    pub unscopables: JsSymbol<'scope>,
    pub is_concat_spreadable: JsSymbol<'scope>,
    pub match_: JsSymbol<'scope>,
    pub match_all: JsSymbol<'scope>,
    pub replace: JsSymbol<'scope>,
    pub search: JsSymbol<'scope>,
    pub split: JsSymbol<'scope>,
}

impl<'scope> WellKnown<'scope> {
    pub fn all(&self) -> [(&'static str, JsSymbol<'scope>); 13] {
        [
            ("iterator", self.iterator),
            ("asyncIterator", self.async_iterator),
            ("hasInstance", self.has_instance),
            ("toPrimitive", self.to_primitive),
            ("toStringTag", self.to_string_tag),
            ("species", self.species),
            ("unscopables", self.unscopables),
            ("isConcatSpreadable", self.is_concat_spreadable),
            ("match", self.match_),
            ("matchAll", self.match_all),
            ("replace", self.replace),
            ("search", self.search),
            ("split", self.split),
        ]
    }
}

#[derive(Collect)]
#[collect(no_drop)]
pub struct Context<'scope> {
    pub global: JsObject<'scope>,
    pub object_proto: JsObject<'scope>,
    pub function_proto: JsObject<'scope>,
    pub array_proto: JsObject<'scope>,
    pub string_proto: JsObject<'scope>,
    pub chars: alloc::vec::Vec<crate::JsString<'scope>>,
    pub empty: crate::JsString<'scope>,
    pub number_proto: JsObject<'scope>,
    pub boolean_proto: JsObject<'scope>,
    pub error_protos: [JsObject<'scope>; ErrorKind::ALL.len()],
    pub symbol_proto: JsObject<'scope>,
    pub iterator_proto: JsObject<'scope>,
    pub array_iterator_proto: JsObject<'scope>,
    pub string_iterator_proto: JsObject<'scope>,
    pub generator_proto: JsObject<'scope>,
    pub generator_function_proto: JsObject<'scope>,
    pub async_function_proto: JsObject<'scope>,
    pub async_generator_function_proto: JsObject<'scope>,
    pub async_iterator_proto: JsObject<'scope>,
    pub async_generator_proto: JsObject<'scope>,
    pub async_from_sync_iterator_proto: JsObject<'scope>,
    pub promise_proto: JsObject<'scope>,
    pub promise_ctor: JsObject<'scope>,
    pub error_ctor: JsObject<'scope>,
    pub regexp_proto: JsObject<'scope>,
    pub regexp_ctor: JsObject<'scope>,
    pub eval: JsObject<'scope>,
    pub bigint_proto: JsObject<'scope>,
    pub map_proto: JsObject<'scope>,
    pub set_proto: JsObject<'scope>,
    pub weakmap_proto: JsObject<'scope>,
    pub weakset_proto: JsObject<'scope>,
    pub collection_iterator_proto: JsObject<'scope>,
    pub set_iterator_proto: JsObject<'scope>,
    pub iterator_helper_proto: JsObject<'scope>,
    pub wrap_iterator_proto: JsObject<'scope>,
    pub date_proto: JsObject<'scope>,
    pub weak_ref_proto: JsObject<'scope>,
    pub finalization_registry_proto: JsObject<'scope>,
    pub regexp_string_iterator_proto: JsObject<'scope>,
    pub array_buffer_proto: JsObject<'scope>,
    pub shared_array_buffer_proto: JsObject<'scope>,
    pub data_view_proto: JsObject<'scope>,
    pub typed_array_ctor: JsObject<'scope>,
    pub typed_array_proto: JsObject<'scope>,
    pub typed_ctors: [JsObject<'scope>; 12],
    pub typed_protos: [JsObject<'scope>; 12],
    pub script_scope: JsObject<'scope>,
    pub symbols: WellKnown<'scope>,
    pub modules: Cow<'scope, alloc::vec::Vec<crate::ModuleRecord<'scope>>>,
    pub symbol_registry: Cow<'scope, alloc::vec::Vec<(JsString<'scope>, JsSymbol<'scope>)>>,
    pub random_state: AtomicU64,
    pub jobs: GcQueue<'scope, Job<'scope>>,
    pub rejections: GcQueue<'scope, JsObject<'scope>>,
    pub templates: Cow<'scope, alloc::collections::BTreeMap<(u32, u32), JsObject<'scope>>>,
    pub string_constants: Cow<'scope, alloc::collections::BTreeMap<(u32, u32), JsString<'scope>>>,
    pub script_contexts: Cow<'scope, Vec<crate::JsEnv<'scope>>>,
    pub regexp_cache: [Slot<'scope, RegExpCompiled<'scope>>; RegExpCompiled::BUCKETS],
    pub regexp_literals: [Slot<'scope, RegExpLiteralEntry<'scope>>; RegExpCompiled::BUCKETS],
    pub regexp_pristine: Cow<'scope, Option<RegExpPristine<'scope>>>,
    pub regexp_literal: Cow<'scope, crate::Static<Option<RegExpLiteral>>>,
    pub fast_builtins: Cow<'scope, crate::Static<FastTable>>,
    pub builtin_nexts: Cow<'scope, [Option<JsObject<'scope>>; 2]>,
    pub indexed_cache: AtomicU64,
    pub async_evaluation_order: AtomicU64,
}

#[derive(Clone, Collect)]
#[collect(no_drop)]
pub struct RegExpPristine<'scope> {
    pub shape: crate::ShapeRef<'scope>,
    pub last_index: crate::ShapeRef<'scope>,
    pub words: [(u16, Value<'scope>); 10],
    pub constructor: (u16, Value<'scope>),
    pub methods: [JsObject<'scope>; 5],
    pub species: JsObject<'scope>,
}

pub type RegExpLiteral = for<'scope, 'a> fn(&crate::Scope<'scope, 'a>, &str, u16) -> Result<Value<'scope>, crate::Error>;

#[derive(Clone, Collect)]
#[collect(no_drop)]
pub struct RegExpLiteralEntry<'scope> {
    #[collect(require_static)]
    pub ptr: usize,
    #[collect(require_static)]
    pub len: usize,
    #[collect(require_static)]
    pub flags: u16,
    pub compiled: RegExpCompiled<'scope>,
    pub shared: crate::object::RegExpShared<'scope>,
}

pub type Fast0 = for<'scope, 'x> fn(&mut crate::Scope<'scope, 'x>, Value<'scope>) -> Option<Value<'scope>>;

pub type Fast1 = for<'scope, 'x> fn(&mut crate::Scope<'scope, 'x>, Value<'scope>, Value<'scope>) -> Option<Value<'scope>>;

pub type Fast2 = for<'scope, 'x> fn(&mut crate::Scope<'scope, 'x>, Value<'scope>, Value<'scope>, Value<'scope>) -> Option<Value<'scope>>;

#[derive(Clone, Copy)]
pub enum FastCall {
    A0(Fast0),
    A1(Fast1),
    A2(Fast2),
}

impl FastCall {
    pub fn arity(self) -> u8 {
        match self {
            Self::A0(_) => 0,
            Self::A1(_) => 1,
            Self::A2(_) => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FastOut {
    Value,
    Number,
    Bool,
}

#[derive(Clone, Copy)]
pub struct FastBuiltin {
    pub native: NativeFn,
    pub call: FastCall,
    pub out: FastOut,
    pub inline_push: bool,
}

#[derive(Clone, Copy)]
pub struct FastTable {
    len: u8,
    entries: [Option<FastBuiltin>; Self::CAP],
}

impl Default for FastTable {
    fn default() -> Self {
        Self { len: 0, entries: [None; Self::CAP] }
    }
}

impl FastTable {
    pub const CAP: usize = 24;

    pub fn push(&mut self, entry: FastBuiltin) {
        let i = usize::from(self.len);
        assert!(i < Self::CAP, "fast builtin table is full");
        self.entries[i] = Some(entry);
        self.len += 1;
    }

    pub fn get(&self, index: usize) -> Option<FastBuiltin> {
        self.entries.get(index).copied().flatten()
    }

    pub fn iter(&self) -> impl Iterator<Item = (u32, FastBuiltin)> + '_ {
        let n = usize::from(self.len);
        self.entries[..n].iter().enumerate().filter_map(|(i, e)| e.map(|e| (u32::try_from(i).unwrap_or(0), e)))
    }
}

#[derive(Clone, Collect)]
#[collect(no_drop)]
pub struct RegExpCompiled<'scope> {
    pub source: JsString<'scope>,
    pub flags: JsString<'scope>,
    pub compiled: ScopeLocal<'scope, crate::Static<alloc::boxed::Box<dyn core::any::Any + Send + Sync>>>,
}

impl RegExpCompiled<'_> {
    pub const BUCKETS: usize = 64;
    const FNV: (u64, u64) = (0xcbf2_9ce4_8422_2325, 0x0100_0000_01b3);

    pub fn bucket(source: &str, flags: &str) -> usize {
        let h = source.bytes().chain([0]).chain(flags.bytes()).fold(Self::FNV.0, |h, b| (h ^ u64::from(b)).wrapping_mul(Self::FNV.1));
        usize::try_from(h % Self::BUCKETS as u64).unwrap_or(0)
    }
}

impl fmt::Debug for Context<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Context")
    }
}

impl<'scope> Context<'scope> {
    fn new(mc: &Mutation<'scope>) -> Self {
        let object_proto = ObjectData::default().alloc(mc);
        let derived = |proto: JsObject<'scope>| ObjectData { proto: Some(proto), ..ObjectData::default() }.alloc(mc);
        let error = derived(object_proto);
        let error_protos = ErrorKind::ALL.map(|k| if k == ErrorKind::Error { error } else { derived(error) });
        let symbol = |d: &str| mc.alloc(SymbolData { description: Some(alloc::format!("Symbol.{d}").into()), private: false });
        let iterator_proto = derived(object_proto);
        let async_iterator_proto = derived(object_proto);
        let function_proto = derived(object_proto);
        let placeholder = |proto: JsObject<'scope>| ObjectData { kind: ObjectKind::Native(Native::function(Self::no_regexp)), proto: Some(proto), ..ObjectData::default() }.alloc(mc);
        let typed_array_ctor = placeholder(function_proto);
        let typed_array_proto = derived(object_proto);
        let array_proto = derived(object_proto);
        object_proto.get().mark_watched();
        array_proto.get().mark_watched();
        Self {
            date_proto: derived(object_proto),
            weak_ref_proto: derived(object_proto),
            finalization_registry_proto: derived(object_proto),
            regexp_string_iterator_proto: derived(iterator_proto),
            array_buffer_proto: derived(object_proto),
            shared_array_buffer_proto: derived(object_proto),
            data_view_proto: derived(object_proto),
            typed_array_ctor,
            typed_array_proto,
            typed_ctors: [(); 12].map(|()| placeholder(typed_array_ctor)),
            typed_protos: [(); 12].map(|()| derived(typed_array_proto)),
            symbol_proto: derived(object_proto),
            iterator_proto,
            array_iterator_proto: derived(iterator_proto),
            string_iterator_proto: derived(iterator_proto),
            generator_proto: derived(iterator_proto),
            async_iterator_proto,
            generator_function_proto: derived(function_proto),
            async_function_proto: derived(function_proto),
            async_generator_function_proto: derived(function_proto),
            async_generator_proto: derived(async_iterator_proto),
            async_from_sync_iterator_proto: derived(async_iterator_proto),
            promise_proto: derived(object_proto),
            promise_ctor: placeholder(function_proto),
            error_ctor: placeholder(function_proto),
            regexp_proto: derived(object_proto),
            regexp_ctor: ObjectData { kind: ObjectKind::Native(Native::function(Self::no_regexp)), ..ObjectData::default() }.alloc(mc),
            eval: ObjectData { kind: ObjectKind::Native(Native::function(Self::no_eval)), proto: Some(function_proto), ..ObjectData::default() }.alloc(mc),
            bigint_proto: derived(object_proto),
            map_proto: derived(object_proto),
            set_proto: derived(object_proto),
            weakmap_proto: derived(object_proto),
            weakset_proto: derived(object_proto),
            collection_iterator_proto: derived(iterator_proto),
            set_iterator_proto: derived(iterator_proto),
            iterator_helper_proto: derived(iterator_proto),
            wrap_iterator_proto: derived(iterator_proto),
            script_scope: Self::wide(ObjectData::default().alloc(mc)),
            symbols: WellKnown {
                iterator: symbol("iterator"),
                async_iterator: symbol("asyncIterator"),
                has_instance: symbol("hasInstance"),
                to_primitive: symbol("toPrimitive"),
                to_string_tag: symbol("toStringTag"),
                species: symbol("species"),
                unscopables: symbol("unscopables"),
                is_concat_spreadable: symbol("isConcatSpreadable"),
                match_: symbol("match"),
                match_all: symbol("matchAll"),
                replace: symbol("replace"),
                search: symbol("search"),
                split: symbol("split"),
            },
            jobs: GcQueue::new(mc),
            rejections: GcQueue::new(mc),
            regexp_cache: core::array::from_fn(|_| Slot::default()),
            regexp_literals: core::array::from_fn(|_| Slot::default()),
            regexp_pristine: Cow::new(mc, None),
            regexp_literal: Cow::new(mc, crate::Static(None)),
            fast_builtins: Cow::new(mc, crate::Static(FastTable::default())),
            builtin_nexts: Cow::new(mc, [None, None]),
            indexed_cache: AtomicU64::new(0),
            async_evaluation_order: AtomicU64::new(0),
            symbol_registry: Cow::new(mc, alloc::vec::Vec::new()),
            modules: Cow::new(mc, alloc::vec::Vec::new()),
            random_state: AtomicU64::new(0),
            templates: Cow::new(mc, alloc::collections::BTreeMap::new()),
            string_constants: Cow::new(mc, alloc::collections::BTreeMap::new()),
            script_contexts: Cow::new(mc, Vec::new()),
            global: Self::wide(derived(object_proto)),
            object_proto,
            function_proto,
            array_proto,
            string_proto: derived(object_proto),
            chars: (0u8..128).map(|c| crate::JsStr::alloc_known(mc, alloc::string::String::from(char::from(c)).as_str(), true)).collect(),
            empty: crate::JsStr::alloc_known(mc, "", true),
            number_proto: derived(object_proto),
            boolean_proto: derived(object_proto),
            error_protos,
        }
    }

    fn wide(o: JsObject<'scope>) -> JsObject<'scope> {
        o.get().widen();
        o
    }

    fn no_regexp<'g>(a: &mut crate::Scope<'g, '_>, _: Value<'g>, _: &[Value<'g>]) -> Result<Value<'g>, crate::Error> {
        Err(a.throw(ErrorKind::Syntax, "regular expressions are not available in this realm"))
    }

    fn no_eval<'g>(a: &mut crate::Scope<'g, '_>, _: Value<'g>, _: &[Value<'g>]) -> Result<Value<'g>, crate::Error> {
        Err(a.throw(ErrorKind::Type, "eval is not available in this realm"))
    }

    pub fn error_proto(&self, kind: ErrorKind) -> JsObject<'scope> {
        self.error_protos[kind as usize]
    }

    pub fn native(&self, mc: &Mutation<'scope>, f: NativeFn) -> JsObject<'scope> {
        ObjectData { kind: ObjectKind::Native(Native::function(f)), proto: Some(self.function_proto), ..ObjectData::default() }.alloc(mc)
    }

    pub fn name_function(&self, mc: &Mutation<'scope>, f: JsObject<'scope>, name: &str, length: Option<u32>) {
        let prop = |v: Value<'scope>| Property { slot: crate::Slot::Data(v), enumerable: false, writable: false, configurable: true };
        let d = f.borrow_mut(mc);
        if let Some(n) = length {
            d.define("length", prop(f64::from(n).into()));
        }
        d.define("name", prop(Value::String(crate::JsStr::alloc(mc, name))));
    }

    pub fn named_native(&self, mc: &Mutation<'scope>, f: NativeFn, name: &str, length: u32) -> JsObject<'scope> {
        let func = self.native(mc, f);
        self.name_function(mc, func, name, Some(length));
        func
    }

    pub fn symbol_method(&self, mc: &Mutation<'scope>, target: JsObject<'scope>, key: JsSymbol<'scope>, f: NativeFn, length: u32) -> JsObject<'scope> {
        let func = self.named_native(mc, f, &alloc::format!("[{}]", key.description.as_deref().unwrap_or("")), length);
        target.borrow_mut(mc).define_symbol(key, Property::hidden(Value::Object(func)));
        func
    }

    pub fn method(&self, mc: &Mutation<'scope>, target: JsObject<'scope>, name: &str, f: NativeFn, length: u32) {
        let func = self.named_native(mc, f, name, length);
        target.borrow_mut(mc).define(name, Property::hidden(Value::Object(func)));
    }

    pub fn add_fast(&self, mc: &Mutation<'scope>, native: NativeFn, call: FastCall, out: FastOut) {
        self.add_fast_at(mc, native, call, out, false);
    }

    pub fn add_fast_inline(&self, mc: &Mutation<'scope>, native: NativeFn, call: FastCall, out: FastOut) {
        self.add_fast_at(mc, native, call, out, true);
    }

    fn add_fast_at(&self, mc: &Mutation<'scope>, native: NativeFn, call: FastCall, out: FastOut, inline_push: bool) {
        self.fast_builtins.update_with(mc, |table| {
            let mut table = *table;
            table.push(FastBuiltin { native, call, out, inline_push });
            (table, ())
        });
    }

    pub fn indexed_cache_addr(&self) -> u64 {
        u64::try_from(core::ptr::from_ref(&self.indexed_cache).addr()).expect("a context field address fits in a machine word")
    }

    pub fn indexed_fast(&self) -> bool {
        let stamp = crate::store::indexed_stamp();
        let cached = self.indexed_cache.load(Ordering::Relaxed);
        if stamp != 0 && stamp < 1 << 63 && cached & ((1 << 63) - 1) == stamp {
            return cached >> 63 != 0;
        }
        self.indexed_refill(stamp)
    }

    #[cold]
    #[inline(never)]
    fn indexed_refill(&self, stamp: u64) -> bool {
        let clean = self.array_proto.get().anchors_plain_arrays(self.object_proto);
        if stamp != 0 && stamp < 1 << 63 && crate::store::indexed_stamp() == stamp {
            self.indexed_cache.store(stamp | (u64::from(clean) << 63), Ordering::Relaxed);
        }
        clean
    }
}

pub trait Intrinsics {
    fn install<'scope>(&self, mc: &Mutation<'scope>, context: &Context<'scope>);
}

impl<'scope> Context<'scope> {
    pub fn create(mc: &Mutation<'scope>, intrinsics: &dyn Intrinsics) -> ScopeLocal<'scope, Self> {
        let context = mc.alloc(Self::new(mc));
        intrinsics.install(mc, context.get());
        context
    }
}
