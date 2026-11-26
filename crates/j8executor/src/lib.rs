#![cfg_attr(tail_calls, feature(explicit_tail_calls))]
#![cfg_attr(tail_calls, expect(incomplete_features, reason = "explicit tail calls are what threaded dispatch needs"))]
extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::ptr::NonNull;

use j8bytecode::{Bytecode, Constant, Context, Decoded, FunctionKind, Intrinsic, LiteralValue, Module, NativeContextIndex, Operands, OperandValue, Register, Runtime, ScopeInfo, ScopeType, SharedFunction};
use j8compiler::Compile;
use j8framework::{Scope, BinaryOp, Closure, Code, ShapeKey, SmallStr, Collect, CompareOp, ContextKind, ContextName, ContextShape, ShapeId, Env, Error, ErrorKind, FuncId, FunctionInfo, ScopeLocal, Lazy, Invoke, JsEnv, JsObject, Numbers, ObjectData, ObjectKind, Property, Resume, Slot, Step, Value, ValueKind};
use j8lockfree::SegVec;

mod cache;
mod debug;
mod ic;
mod jit;
mod module;
mod threaded;
mod event_loop;
mod interrupt;
use module::EVAL_FILE;

pub use cache::CodeCache;
pub use debug::{Action, Breakpoint, BreakpointId, Debugger, PauseOnExceptions, Paused, Reason, ScopeKind, ScopeView};
pub use event_loop::{EventLoop, EventLoopSet, Ticket, CHECK, SLICE};
pub use interrupt::Interrupter;
pub use jit::{DeoptFrame, Entry, FrameSlots, Function as JitFunction, GlobalFeedback, Insn as JitInsn, PropertyFeedback, Reg as JitReg, ScopeLayout, StringFeedback, Tier, Tiered};

const STACK_LIMIT: usize = 10_000;

pub struct Depth<'e> {
    cell: &'e Cell<usize>,
    saved: usize,
}

impl Drop for Depth<'_> {
    fn drop(&mut self) {
        self.cell.set(self.saved);
    }
}

const REGEXP_FLAGS: [(u8, u16); 8] = [(b'd', 1 << 7), (b'g', 1), (b'i', 1 << 1), (b'm', 1 << 2), (b's', 1 << 5), (b'u', 1 << 4), (b'v', 1 << 8), (b'y', 1 << 3)];

const SCRIPT_THIS: &str = "this";

const SET_FUNCTION_NAME: OperandValue = 1;

const RESUME_RETHROW: f64 = 3.0;

const FRAMES_RESERVED: usize = 16;

const SPARE_FRAMES: usize = 8;

const CHECK_STEPS: usize = 1024;

const TIER_CALLS: u32 = 64;

struct Insn {
    handler: Cell<threaded::Handler>,
    bytecode: Bytecode,
    operands: Operands,
    start: u32,
    target: u32,
}

#[repr(C)]
struct Loaded {
    info: FunctionInfo,
    base: u32,
    id: FuncId,
    calls: Cell<u32>,
    call_ics: Box<[Cell<u32>]>,
    native_ics: Box<[Cell<u64>]>,
    tier: Cell<usize>,
    func: SharedFunction,
    insns: Box<[Insn]>,
    ics: Box<[ic::Ic]>,
    globals: Box<[ic::GlobalIc]>,
    literals: Box<[Cell<Option<ShapeKey>>]>,
    function_shape: Cell<Option<ShapeKey>>,
    prototype_shape: Cell<Option<ShapeKey>>,
    this: Cell<usize>,
    unit: Cell<Option<Code>>,
    jit_shapes: RefCell<Vec<ShapeKey>>,
    jit_codes: RefCell<Vec<Code>>,
    shapes: Box<[Option<(ShapeId, u32)>]>,
    scope: Option<ShapeId>,
    extension: bool,
    constructs: bool,
    class_constructor: bool,
    derived: bool,
    generator: bool,
    asynchronous: bool,
    no_prototype: bool,
    wraps_this: bool,
    keeps_args: bool,
    instance_size: Cell<u8>,
    prototype: ic::Ic,
}

impl Loaded {
    fn note_instance(&self, o: JsObject<'_>) {
        self.instance_size.set(u8::try_from(o.borrow().props().len()).unwrap_or(u8::MAX));
    }

    fn code(&self) -> Code {
        unsafe { Code::from_id(self.this.get()) }
    }

    fn new(base: u32, id: FuncId, func: SharedFunction, table: &SegVec<ContextShape>) -> Self {
        let bc = &func.bytecode;
        let call_ics = bc.feedback.iter().map(|_| Cell::new(0)).collect();
        let native_ics = bc.feedback.iter().map(|_| Cell::new(0)).collect();
        let ics = bc.feedback.iter().map(|_| ic::Ic::default()).collect();
        let globals = bc.feedback.iter().map(|_| ic::GlobalIc::default()).collect();
        let shapes = bc.constants.iter().map(|c| match c {
            Constant::Scope(s) => Some((Self::add_shape(table, s), u32::try_from(Self::shape(s).names.len()).expect("fewer than 2^32 slots"))),
            _ => None,
        }).collect();
        let literals = bc.constants.iter().map(|_| Cell::new(None)).collect();
        let scope = func.scope.as_ref().map(|s| Self::add_shape(table, s));
        let extension = func.scope.as_ref().is_some_and(|s| s.has_extension);
        let k = func.kind;
        use FunctionKind as F;
        let generator = matches!(k, F::GeneratorFunction | F::AsyncGeneratorFunction | F::ConciseGeneratorMethod | F::StaticConciseGeneratorMethod | F::AsyncConciseGeneratorMethod | F::StaticAsyncConciseGeneratorMethod);
        let asynchronous = matches!(k, F::AsyncFunction | F::AsyncArrowFunction | F::AsyncConciseMethod | F::StaticAsyncConciseMethod | F::AsyncGeneratorFunction | F::AsyncConciseGeneratorMethod | F::StaticAsyncConciseGeneratorMethod);
        let class_constructor = matches!(k, F::BaseConstructor | F::DerivedConstructor | F::DefaultBaseConstructor | F::DefaultDerivedConstructor);
        let derived = matches!(k, F::DerivedConstructor | F::DefaultDerivedConstructor);
        let normal = matches!(k, F::NormalFunction);
        let insns = Self::decode(bc);
        let keeps_args = insns.iter().any(|d| matches!(d.bytecode, Bytecode::CreateRestParameter | Bytecode::CreateMappedArguments | Bytecode::CreateUnmappedArguments | Bytecode::ConstructForwardAllArgs));
        Self {
            base,
            id,
            calls: Cell::new(0),
            call_ics,
            native_ics,
            tier: Cell::new(0),
            insns,
            ics,
            globals,
            literals,
            info: FunctionInfo { name: SmallStr::from(&*func.name), length: func.length },
            function_shape: Cell::new(None),
            prototype_shape: Cell::new(None),
            this: Cell::new(0),
            unit: Cell::new(None),
            jit_shapes: RefCell::new(Vec::new()),
            jit_codes: RefCell::new(Vec::new()),
            shapes,
            scope,
            extension,
            constructs: !generator && !asynchronous && (normal || matches!(k, F::BaseConstructor | F::DefaultBaseConstructor)),
            class_constructor,
            derived,
            generator,
            asynchronous,
            no_prototype: !normal && !generator,
            keeps_args,
            wraps_this: !func.strict && !matches!(k, F::ArrowFunction | F::AsyncArrowFunction),
            instance_size: Cell::new(0),
            prototype: ic::Ic::default(),
            func,
        }
    }

    fn decode(bc: &j8bytecode::BytecodeArray) -> Box<[Insn]> {
        let decoded: Vec<Decoded> = j8bytecode::Decoder::from(&bc.code[..]).map(|d| d.expect("compiled bytecode decodes")).collect();
        let index = |byte: usize| u32::try_from(decoded.partition_point(|d| d.offset < byte)).unwrap_or(u32::MAX);
        decoded
            .iter()
            .map(|d| {
                let target = match d.bytecode {
                    Bytecode::JumpLoop => index(d.start() - usize::try_from(d.operands[0]).unwrap_or(0)),
                    b if b.is_jump_constant() => match bc.constants.get(usize::try_from(d.operands[0]).unwrap_or(usize::MAX)) {
                        Some(Constant::Offset(o)) => index(d.start() + *o as usize),
                        _ => 0,
                    },
                    b if b.is_jump() => index(d.start() + usize::try_from(d.operands[0]).unwrap_or(0)),
                    _ => 0,
                };
                let mut operands = d.operands;
                for (v, t) in operands.iter_mut().zip(d.bytecode.operand_types()) {
                    if t.is_register() {
                        *v = Self::register(*v, bc.parameter_count);
                    }
                }
                let bytecode = if d.bytecode.is_short_star() {
                    operands = Operands::from(&[OperandValue::from(d.bytecode.short_star_register().0)][..]);
                    Bytecode::Star
                } else {
                    d.bytecode
                };
                Insn { handler: Cell::new(Insn::handler(bytecode)), bytecode, operands, start: u32::try_from(d.start()).unwrap_or(u32::MAX), target }
            })
            .chain(core::iter::once(Insn { handler: Cell::new(Insn::handler(Bytecode::Illegal)), bytecode: Bytecode::Illegal, operands: Operands::default(), start: u32::try_from(bc.code.len()).unwrap_or(u32::MAX), target: 0 }))
            .collect()
    }

    const SPECIAL: OperandValue = OperandValue::MIN / 2;
    const SPECIAL_LIMIT: OperandValue = Self::SPECIAL / 2;

    fn register(operand: OperandValue, parameter_count: u16) -> OperandValue {
        let o = i32::try_from(operand).unwrap_or(0);
        if o <= Register::REGISTER_FILE_FROM_FP {
            OperandValue::from(Register::from(o).0)
        } else if o >= Register::FIRST_PARAM_FROM_FP {
            OperandValue::from(o - Register::FIRST_PARAM_FROM_FP) - OperandValue::from(parameter_count) - Self::below(Frame::HEADER)
        } else if o == Register::CONTEXT_FROM_FP {
            -Self::below(Frame::CONTEXT)
        } else if o == Register::FUNCTION_FROM_FP {
            -Self::below(Frame::CLOSURE)
        } else {
            Self::SPECIAL + operand
        }
    }

    fn below(slot: usize) -> OperandValue {
        OperandValue::try_from(slot).unwrap_or(0)
    }

    fn index_of(&self, byte: usize) -> usize {
        self.insns.partition_point(|d| (d.start as usize) < byte)
    }

    #[inline]
    fn scope_shape(&self, k: OperandValue) -> Option<(ShapeId, u32)> {
        self.shapes.get(usize::try_from(k).ok()?).copied().flatten()
    }

    #[inline]
    fn block_context<'scope>(&self, a: &Scope<'scope, '_>, k: OperandValue, parent: Option<JsEnv<'scope>>) -> Value<'scope> {
        let (shape, n) = self.scope_shape(k).map_or((None, 0), |(s, n)| (Some(s), n as usize));
        Value::context(Env::context(n, Value::Undefined, parent, shape, None).alloc(a.mc))
    }

    fn add_shape(table: &SegVec<ContextShape>, s: &ScopeInfo) -> ShapeId {
        ShapeId(u32::try_from(table.push(Self::shape(s))).expect("fewer than 2^32 context shapes"))
    }

    fn shape(s: &ScopeInfo) -> ContextShape {
        let kind = match s.scope_type {
            Some(ScopeType::FunctionScope) | None => ContextKind::Function,
            Some(ScopeType::CatchScope) => ContextKind::Catch,
            Some(ScopeType::WithScope) => ContextKind::With,
            Some(ScopeType::ScriptScope) => ContextKind::Script,
            Some(ScopeType::EvalScope) => ContextKind::Eval,
            Some(ScopeType::ModuleScope) => ContextKind::Module,
            Some(_) => ContextKind::Block,
        };
        let names = s.variables.iter().map(|v| ContextName {
            name: (&*v.name).into(),
            constant: v.mode == j8bytecode::VariableMode::Const,
            lexical: matches!(v.mode, j8bytecode::VariableMode::Let | j8bytecode::VariableMode::Const) && v.role != j8bytecode::VariableRole::FunctionName,
            parameter: match v.role {
                j8bytecode::VariableRole::Parameter(p) => Some(p),
                _ => None,
            },
            function_name: v.role == j8bytecode::VariableRole::FunctionName,
        }).collect();
        ContextShape { kind, names }
    }
}

#[derive(Clone, Copy, Collect)]
struct Frame {
    func: FuncId,
    loaded: LoadedRef,
    pc: usize,
    insn: usize,
    params: usize,
    base: usize,
    argc: usize,
}

#[derive(Clone, Copy, Collect)]
struct LoadedRef(Code);

impl From<&Loaded> for LoadedRef {
    fn from(l: &Loaded) -> Self {
        Self(l.code())
    }
}

impl LoadedRef {
    unsafe fn get<'e>(self) -> &'e Loaded {
        unsafe { self.0.get() }
    }
}

j8framework::paragc::brand!(LoadedBrand = for<'heap, 'scope> Loaded);

unsafe impl Send for Loaded {}

unsafe impl Sync for Loaded {}

unsafe impl j8framework::Trace for Loaded {
    fn trace(&self, t: &mut j8framework::Tracer<'_>) {
        self.ics.iter().for_each(|ic| ic.trace_keys(t));
        self.globals.iter().for_each(|ic| ic.trace_keys(t));
        self.prototype.trace_keys(t);
        let cells = self.literals.iter().chain([&self.function_shape, &self.prototype_shape]);
        cells.filter_map(Cell::get).for_each(|k| k.trace(t));
        if let Some(u) = self.unit.get() {
            u.trace(t);
        }
        self.jit_shapes.borrow().iter().for_each(|k| k.trace(t));
        self.jit_codes.borrow().iter().for_each(|c| c.trace(t));
    }

    fn relocate(&mut self, t: &mut j8framework::Tracer<'_>) {
        self.trace(t);
    }
}

struct Unit(Box<[Code]>);

unsafe impl j8framework::Trace for Unit {
    fn trace(&self, t: &mut j8framework::Tracer<'_>) {
        self.0.iter().for_each(|c| c.trace(t));
    }

    fn relocate(&mut self, t: &mut j8framework::Tracer<'_>) {
        self.trace(t);
    }
}

impl Frame {
    const CONTEXT: usize = 1;
    const CLOSURE: usize = 2;
    const NEW_TARGET: usize = 3;
    const NEW_THIS: usize = 4;
    const ARGS: usize = 5;
    const HEADER: usize = 5;

    #[inline]
    fn slot<'scope>(&self, regs: &[Value<'scope>], k: usize) -> Value<'scope> {
        regs[self.base - k]
    }

    #[inline]
    fn set_slot<'scope>(&self, regs: &mut [Value<'scope>], k: usize, v: Value<'scope>) {
        regs[self.base - k] = v;
    }

    #[inline]
    fn env<'scope>(&self, regs: &[Value<'scope>]) -> Option<JsEnv<'scope>> {
        self.slot(regs, Self::CONTEXT).as_context()
    }

    #[inline]
    fn set_env<'scope>(&self, regs: &mut [Value<'scope>], e: Option<JsEnv<'scope>>) {
        self.set_slot(regs, Self::CONTEXT, e.map_or(Value::Undefined, Value::context));
    }

    fn new_this<'scope>(&self, regs: &[Value<'scope>]) -> Option<Value<'scope>> {
        Some(self.slot(regs, Self::NEW_THIS)).filter(|v| !matches!(v.kind(), ValueKind::Uninit))
    }

    fn constructing(&self, regs: &[Value<'_>]) -> bool {
        self.slot(regs, Self::NEW_TARGET).bits() != j8framework::ValueBits::UNDEFINED
    }

    fn args<'scope>(&self, regs: &[Value<'scope>]) -> Vec<Value<'scope>> {
        self.slot(regs, Self::ARGS).as_object().map_or_else(Vec::new, |o| o.elements().to_vec())
    }
}

#[derive(Collect, Default)]
#[collect(no_drop)]
struct State<'scope> {
    regs: Vec<Value<'scope>>,
    frames: Vec<Frame>,
    acc: Value<'scope>,
    message: Value<'scope>,
    outer: usize,
}

impl<'scope> State<'scope> {
    fn new(regs: Vec<Value<'scope>>, mut frames: Vec<Frame>, f: Frame) -> Self {
        frames.push(f);
        Self { regs, frames, ..Self::default() }
    }

    #[inline]
    fn level(&self) -> usize {
        self.outer + self.frames.len()
    }

    #[inline]
    fn push(&mut self, f: Frame) -> usize {
        self.frames.push(f);
        f.base
    }

    #[inline]
    fn room(&self) -> bool {
        self.level() + 1 < STACK_LIMIT
    }

    #[inline]
    fn f(&self) -> &Frame {
        self.frames.last().expect("a running frame")
    }

    #[inline]
    fn f_mut(&mut self) -> &mut Frame {
        self.frames.last_mut().expect("a running frame")
    }

    #[inline]
    fn pop_frame(&mut self) -> bool {
        let Some(done) = self.frames.pop() else { return false };
        if self.frames.is_empty() {
            return false;
        }
        self.regs.truncate(done.params);
        true
    }

    fn returned(&mut self, v: Value<'scope>) -> Flow<'scope> {
        if !self.pop_frame() {
            return Flow::Done(Outcome::Return(v));
        }
        self.acc = v;
        Flow::Frame
    }
}

enum Outcome<'scope> {
    Return(Value<'scope>),
    Paused(Box<State<'scope>>),
}

impl<'scope> Outcome<'scope> {
    fn returned(self) -> Value<'scope> {
        match self {
            Outcome::Return(v) => v,
            Outcome::Paused(_) => unreachable!("only the outermost execution pauses"),
        }
    }
}

#[derive(Clone, Copy)]
enum Opening<'scope> {
    Call,
    Construct(Value<'scope>),
    Resume(JsObject<'scope>),
}

#[derive(Clone, Copy)]
enum Args<'a, 'scope> {
    Values(&'a [Value<'scope>]),
    Regs { first: usize, count: usize },
}

impl<'scope> Args<'_, 'scope> {
    fn len(self) -> usize {
        match self {
            Args::Values(v) => v.len(),
            Args::Regs { count, .. } => count,
        }
    }

    fn get(self, regs: &[Value<'scope>], i: usize) -> Value<'scope> {
        match self {
            Args::Values(v) => v.get(i).copied().unwrap_or_default(),
            Args::Regs { first, count } if i < count => regs[first + i],
            Args::Regs { .. } => Value::Undefined,
        }
    }

    fn to_vec(self, regs: &[Value<'scope>]) -> Vec<Value<'scope>> {
        match self {
            Args::Values(v) => v.to_vec(),
            Args::Regs { first, count } => regs[first..first + count].to_vec(),
        }
    }
}

enum CallStep<'scope> {
    Closure(Closure<'scope>),
    Resolver,
    Proxy,
    NativeClosure(j8framework::ClosureFn),
    Native(j8framework::Native),
    Forward(j8framework::Native),
    Bound(Value<'scope>, Value<'scope>, Vec<Value<'scope>>),
}

enum Argv<'a, 'scope> {
    Same(Args<'a, 'scope>),
    Owned(Vec<Value<'scope>>),
}

impl<'scope> Argv<'_, 'scope> {
    fn slice<'r>(&'r self, regs: &'r [Value<'scope>]) -> &'r [Value<'scope>] {
        match self {
            Argv::Same(Args::Values(v)) => v,
            Argv::Same(Args::Regs { first, count }) => &regs[*first..*first + *count],
            Argv::Owned(v) => v,
        }
    }

    fn args(&self) -> Args<'_, 'scope> {
        match self {
            Argv::Same(a) => *a,
            Argv::Owned(v) => Args::Values(v),
        }
    }
}

enum Callee<'a, 'scope> {
    Closure { closure: Closure<'scope>, callee: Value<'scope>, this: Value<'scope>, args: Argv<'a, 'scope> },
    Done(Value<'scope>),
}

enum Resumed<'scope> {
    Step(Step<'scope>),
    Frame(Vec<Value<'scope>>, Frame, usize),
}

enum Prepared<'scope> {
    Frame(Frame),
    Done(Value<'scope>),
}

struct Budget<'s> {
    tick: usize,
    left: Option<usize>,
    stop: &'s j8framework::Termination,
    terminated: bool,
    interrupted: bool,
}

impl<'s> Budget<'s> {
    fn new(stop: &'s j8framework::Termination, left: Option<usize>) -> Self {
        Self { tick: 0, left, stop, terminated: false, interrupted: false }
    }

    fn remaining(&self) -> Option<usize> {
        self.left.map(|l| l + self.tick)
    }

    fn refill(&mut self) -> bool {
        if self.stop.is_set() {
            self.terminated = self.stop.observe();
            self.interrupted = !self.terminated;
            return true;
        }
        let take = match &mut self.left {
            Some(0) => return true,
            Some(l) => {
                let t = (*l).min(CHECK_STEPS);
                *l -= t;
                t
            }
            None => CHECK_STEPS,
        };
        self.tick = take - 1;
        false
    }

    #[inline]
    fn spent(&mut self) -> bool {
        if self.tick == 0 {
            return self.refill();
        }
        self.tick -= 1;
        false
    }
}

struct Running {
    state: NonNull<()>,
    outer: Option<NonNull<Running>>,
}

#[derive(Default)]
struct Active(Cell<Option<NonNull<Running>>>);

unsafe impl Send for Active {}

impl Executor<'_> {
    pub(crate) fn is_running(&self) -> bool {
        self.active.0.get().is_some()
    }
}

#[derive(Default)]
pub struct Executor<'p> {
    tier: Option<Box<dyn Tier + Send>>,
    code: SegVec<(j8framework::Weak<'p, LoadedBrand>, Code)>,
    depth: Cell<usize>,
    module_of: RefCell<alloc::collections::BTreeMap<u32, u32>>,
    script_files: RefCell<alloc::collections::BTreeMap<u32, alloc::string::String>>,
    cache: Option<Arc<CodeCache>>,
    context_shapes: SegVec<ContextShape>,
    with_shape: Cell<Option<ShapeId>>,
    spare_frames: RefCell<Vec<Vec<Frame>>>,
    active: Active,
    interrupts: interrupt::Interrupts<'p>,
    debug: Option<Box<debug::Debug<'p>>>,
    raw: Cell<usize>,
    deferred: Cell<bool>,
    safe_next: Cell<bool>,
    roots: RefCell<Option<j8framework::ScopeGlobal<'p, RootsBrand>>>,
}

#[derive(Collect)]
#[collect(no_drop)]
struct RootCells<'scope>(Box<[j8framework::ValueCell<'scope>]>);

j8framework::paragc::brand!(RootsBrand = for<'heap, 'scope> RootCells<'scope>);

#[derive(Collect)]
#[collect(no_drop)]
struct FrameCodes(Box<[Code]>);

j8framework::paragc::brand!(FrameCodesBrand = for<'heap, 'scope> FrameCodes);

const MIN_ROOT_CELLS: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Halt {
    Go,
    Pause,
    Collected,
}

struct RawFrame<'e> {
    raw: &'e Cell<usize>,
    deferred: &'e Cell<bool>,
    stop: &'e j8framework::Termination,
}

impl Drop for RawFrame<'_> {
    fn drop(&mut self) {
        self.raw.set(self.raw.get() - 1);
        if self.deferred.take() {
            self.stop.request_collect();
        }
    }
}

const _: () = {
    const fn send<T: Send>() {}
    send::<Executor<'_>>();
};

enum Flow<'scope> {
    Next,
    Frame,
    Done(Outcome<'scope>),
}

impl Executor<'_> {
    #[must_use]
    pub fn with_tier(mut self, tier: impl Tier + Send + 'static) -> Self {
        self.tier = Some(Box::new(tier));
        self
    }

    #[must_use]
    pub fn with_cache(mut self, cache: Arc<CodeCache>) -> Self {
        self.cache = Some(cache);
        self
    }

    fn note_user(&self) {
        if let Some(t) = &self.tier {
            t.entered();
        }
    }

    fn within(a: &Scope<'_, '_>, level: usize) -> Result<usize, Error> {
        if level >= STACK_LIMIT {
            return Err(a.throw(ErrorKind::Range, "Maximum call stack size exceeded"));
        }
        Ok(level)
    }

    fn deeper(&self, a: &Scope<'_, '_>) -> Result<usize, Error> {
        Self::within(a, self.depth.get() + 1)
    }

    pub fn at_depth(&self, level: usize) -> Depth<'_> {
        Depth { cell: &self.depth, saved: self.depth.replace(level) }
    }

    fn deep<R>(&self, level: usize, f: impl FnOnce() -> R) -> R {
        let _depth = self.at_depth(level);
        f()
    }

    fn native_at<'scope, 'x, R>(&self, a: &mut Scope<'scope, 'x>, level: usize, new_target: Value<'scope>, f: impl FnOnce(&mut Scope<'scope, 'x>) -> R) -> R {
        let _depth = self.at_depth(level);
        let outer = core::mem::replace(&mut a.new_target, new_target);
        a.handles.push(outer);
        let r = f(a);
        a.new_target = a.handles.pop().unwrap_or_default();
        r
    }

    pub fn load(&self, mc: &j8framework::Mutation<'_>, module: Module) -> FuncId {
        let m = mc.gc_scope();
        let base = u32::try_from(self.code.len()).expect("fewer than 2^32 functions");
        let mut codes = Vec::with_capacity(module.functions.len());
        for func in module.functions {
            let id = FuncId(u32::try_from(self.code.len()).expect("fewer than 2^32 functions"));
            let h = m.alloc_handle(Loaded::new(base, id, func, &self.context_shapes));
            h.this.set(h.id());
            codes.push(Code::new(h));
            self.code.push((mc.weak::<LoadedBrand>(h.get()), Code::new(h)));
        }
        let unit = Code::new(m.alloc_handle(Unit(codes.clone().into_boxed_slice())));
        for c in &codes {
            unsafe { c.get::<Loaded>() }.unit.set(Some(unit));
        }
        FuncId(base + Module::MAIN)
    }

    fn context_shape(&self, id: ShapeId) -> &ContextShape {
        self.context_shapes.get(id.0 as usize).expect("shape ids come from load")
    }

    #[inline]
    fn code(&self, c: &Closure<'_>) -> &Loaded {
        match c.code {
            Some(code) => unsafe { code.get() },
            None => self.function(c.func),
        }
    }

    pub fn live_functions(&self) -> usize {
        self.code.iter().filter(|(w, _)| !w.is_cleared()).count()
    }

    fn function(&self, f: FuncId) -> &Loaded {
        self.live_function(f).expect("the code of a live function")
    }

    fn live_function(&self, f: FuncId) -> Option<&Loaded> {
        let (weak, code) = self.code.get(f.0 as usize)?;
        (!weak.is_cleared()).then(|| unsafe { code.get() })
    }

    #[expect(clippy::too_many_arguments, reason = "the parts of a call")]
    #[inline]
    fn open<'scope>(a: &mut Scope<'scope, '_>, regs: &mut Vec<Value<'scope>>, loaded: &Loaded, closure: Closure<'scope>, callee: Value<'scope>, this: Value<'scope>, args: Args<'_, 'scope>, how: Opening<'scope>) -> Frame {
        let f = &loaded.func;
        a.profiler.enter(closure.func);
        let receiver = match this.kind() {
            _ if !loaded.wraps_this => this,
            ValueKind::Undefined | ValueKind::Null => Value::Object(a.context.global),
            ValueKind::Object(_) | ValueKind::Uninit => this,
            prim => a.to_object(Value::from(prim)).map_or(Value::from(prim), Value::Object),
        };
        let kept = if loaded.keeps_args { Value::Object(a.array(args.to_vec(regs))) } else { Value::Undefined };
        let params = regs.len();
        let formals = usize::from(f.bytecode.parameter_count).saturating_sub(1);
        regs.push(receiver);
        for i in 0..formals {
            let v = args.get(regs, i);
            regs.push(v);
        }
        let (new_this, new_target, register) = match how {
            Opening::Call => (Value::Uninit, Value::Undefined, Value::Undefined),
            Opening::Construct(nt) => (this, nt, nt),
            Opening::Resume(co) => (Value::Uninit, Value::Undefined, co.into()),
        };
        regs.extend_from_slice(&[kept, new_this, new_target, callee, closure.env.map_or(Value::Undefined, Value::context)]);
        let base = regs.len();
        regs.resize(base + f.bytecode.register_count as usize, Value::Undefined);
        if let Some(r) = f.bytecode.new_target_or_generator {
            regs[base + usize::try_from(r).unwrap_or(0)] = register;
        }
        Frame { func: closure.func, loaded: LoadedRef::from(loaded), pc: 0, insn: 0, params, base, argc: args.len() }
    }

    #[inline]
    fn index(key: Value<'_>) -> Option<usize> {
        let x = key.as_number()?;
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "checked by converting back")]
        let i = x as u32;
        (f64::from(i) == x && i != u32::MAX).then_some(i as usize)
    }

    #[inline]
    fn slot_of(f: &Frame, k: OperandValue) -> Option<usize> {
        (k > Loaded::SPECIAL_LIMIT).then(|| f.base.wrapping_add_signed(k as isize))
    }

    #[inline]
    fn reg<'scope>(regs: &[Value<'scope>], f: &Frame, k: OperandValue) -> Value<'scope> {
        if k > Loaded::SPECIAL_LIMIT {
            return regs[f.base.wrapping_add_signed(k as isize)];
        }
        match i32::try_from(k - Loaded::SPECIAL).unwrap_or(0) {
            Register::ARGC_FROM_FP => Value::Number(f64::from(u32::try_from(f.argc).unwrap_or(u32::MAX))),
            _ => Value::Undefined,
        }
    }

    #[inline]
    fn set_reg<'scope>(regs: &mut [Value<'scope>], f: &mut Frame, k: OperandValue, v: Value<'scope>) {
        if k > Loaded::SPECIAL_LIMIT {
            regs[f.base.wrapping_add_signed(k as isize)] = v;
        }
    }

    fn reg_list<'scope>(regs: &[Value<'scope>], f: &Frame, first: OperandValue, count: OperandValue) -> Vec<Value<'scope>> {
        (0..count).map(|i| Self::reg(regs, f, first + i)).collect()
    }

    fn context_at<'scope>(a: &Scope<'scope, '_>, context: Value<'scope>, depth: OperandValue) -> Result<JsEnv<'scope>, Error> {
        let mut e = context.as_context().ok_or_else(|| a.throw(ErrorKind::Reference, "no context"))?;
        for _ in 0..depth {
            e = e.parent.ok_or_else(|| a.throw(ErrorKind::Reference, "no outer context"))?;
        }
        Ok(e)
    }

    fn env_index(slot: OperandValue) -> usize {
        usize::try_from(slot).unwrap_or(0).saturating_sub(Context::MIN_SLOTS as usize)
    }

    fn resolve<'a, 'scope>(&self, a: &mut Scope<'scope, '_>, regs: &[Value<'scope>], mut callee: Value<'scope>, mut this: Value<'scope>, args: Args<'a, 'scope>, level: usize) -> Result<Callee<'a, 'scope>, Error> {
        let mut args = Argv::Same(args);
        while let ValueKind::Object(o) = callee.kind() {
            let step = if let Some(c) = o.closure() {
                CallStep::Closure(c)
            } else if let Some(n) = o.native() {
                if n.forward == j8framework::Forward::No { CallStep::Native(n) } else { CallStep::Forward(n) }
            } else {
                match &o.borrow().kind {
                    ObjectKind::Function(c) => CallStep::Closure(*c),
                    ObjectKind::Resolver { .. } => CallStep::Resolver,
                    ObjectKind::Proxy(p) if p.callable => CallStep::Proxy,
                    ObjectKind::NativeClosure(c) => CallStep::NativeClosure(c.f),
                    ObjectKind::Native(n) if n.forward != j8framework::Forward::No => CallStep::Forward(*n),
                    ObjectKind::Native(n) => CallStep::Native(*n),
                    ObjectKind::Bound(b) => CallStep::Bound(b.target, b.this, b.args.clone()),
                    _ => break,
                }
            };
            if let CallStep::Forward(n) = step
                && a.is_callable(this)
            {
                let target = this;
                let mut rest = args.slice(regs).iter().copied();
                this = rest.next().unwrap_or_default();
                let list = match n.forward {
                    j8framework::Forward::Apply => match rest.next().unwrap_or_default().kind() {
                        ValueKind::Undefined | ValueKind::Null => Vec::new(),
                        list => a.list_from_array_like(Value::from(list))?,
                    },
                    _ => rest.collect(),
                };
                (callee, args) = (target, Argv::Owned(list));
                continue;
            }
            let r = match step {
                CallStep::Closure(c) => {
                    let loaded = self.code(&c);
                    if loaded.class_constructor {
                        return Err(a.throw(ErrorKind::Type, &alloc::format!("Class constructor {} cannot be invoked without 'new'", loaded.func.name)));
                    }
                    return Ok(Callee::Closure { closure: c, callee, this, args });
                }
                CallStep::Resolver => self.deep(level, || a.run_resolver(o, args.slice(regs).first().copied().unwrap_or_default())),
                CallStep::Proxy => self.deep(level, || a.proxy_call(o, this, args.slice(regs))),
                CallStep::NativeClosure(f) => self.native_at(a, level, Value::Undefined, |a| f(a, o, this, args.slice(regs))),
                CallStep::Native(n) | CallStep::Forward(n) => self.native_at(a, level, Value::Undefined, |a| (n.f)(a, this, args.slice(regs))),
                CallStep::Bound(target, bound_this, mut bound_args) => {
                    bound_args.extend_from_slice(args.slice(regs));
                    (callee, this, args) = (target, bound_this, Argv::Owned(bound_args));
                    continue;
                }
            };
            return r.map(Callee::Done);
        }
        let what = a.describe(callee);
        Err(a.throw(ErrorKind::Type, &alloc::format!("{what} is not a function")))
    }

    fn construct_closure<'scope>(a: &mut Scope<'scope, '_>, o: JsObject<'scope>, f: j8framework::ClosureFn, args: &[Value<'scope>], new_target: Value<'scope>) -> Result<Value<'scope>, Error> {
        let proto = a.proto_from_ctor(new_target, a.context.object_proto)?;
        let this: Value<'scope> = ObjectData { proto: Some(proto), ..ObjectData::default() }.alloc(a.mc).into();
        let outer = core::mem::replace(&mut a.new_target, new_target);
        let r = f(a, o, this, args);
        a.new_target = outer;
        let v = r?;
        Ok(if v.as_object().is_some() { v } else { this })
    }

    fn prepare<'scope>(&self, a: &mut Scope<'scope, '_>, regs: &mut Vec<Value<'scope>>, mut callee: Value<'scope>, mut args: Vec<Value<'scope>>, mut new_target: Value<'scope>, level: usize) -> Result<Prepared<'scope>, Error> {
        while let ValueKind::Object(o) = callee.kind() {
            let step = match &o.borrow().kind {
                ObjectKind::Function(c) => CallStep::Closure(*c),
                ObjectKind::Native(n) if n.constructor => CallStep::Native(*n),
                ObjectKind::NativeClosure(c) if c.constructor => CallStep::NativeClosure(c.f),
                ObjectKind::Bound(b) => CallStep::Bound(b.target, b.this, b.args.clone()),
                ObjectKind::Proxy(p) if p.constructor => CallStep::Proxy,
                _ => break,
            };
            return match step {
                CallStep::Closure(closure) => {
                    let loaded = self.code(&closure);
                    if !(loaded.constructs || loaded.class_constructor) {
                        break;
                    }
                    let this = if loaded.derived { Value::Uninit } else { Value::Object(Self::create_this(a, loaded, new_target)?) };
                    Ok(Prepared::Frame(Self::open(a, regs, loaded, closure, callee, this, Args::Values(&args), Opening::Construct(new_target))))
                }
                CallStep::Native(n) => {
                    let r = self.native_at(a, level, new_target, |a| (n.f)(a, Value::Undefined, &args));
                    let Some(obj) = (r?).as_object() else { unreachable!("built-in constructors return objects") };
                    Ok(Prepared::Done(obj.into()))
                }
                CallStep::NativeClosure(f) => self.deep(level, || Self::construct_closure(a, o, f, &args, new_target)).map(Prepared::Done),
                CallStep::Proxy => self.deep(level, || a.proxy_construct(o, &args, new_target)).map(Prepared::Done),
                CallStep::Bound(target, _, mut bound_args) => {
                    if matches!(new_target.kind(), ValueKind::Object(t) if ScopeLocal::ptr_eq(t, o)) {
                        new_target = target;
                    }
                    bound_args.extend(args);
                    (callee, args) = (target, bound_args);
                    continue;
                }
                CallStep::Resolver | CallStep::Forward(_) => unreachable!("not built for construction"),
            };
        }
        let what = a.describe(callee);
        Err(a.throw(ErrorKind::Type, &alloc::format!("{what} is not a constructor")))
    }

    fn create_this<'scope>(a: &mut Scope<'scope, '_>, ctor: &Loaded, new_target: Value<'scope>) -> Result<JsObject<'scope>, Error> {
        let proto = match new_target.kind() {
            ValueKind::Object(o) => match ctor.prototype.get(o) {
                Some(v) => v,
                None => {
                    let v = a.get_named(new_target, "prototype")?;
                    ctor.prototype.fill_get(a.mc, o, "prototype");
                    v
                }
            },
            _ => a.get_named(new_target, "prototype")?,
        };
        let proto = proto.as_object().unwrap_or(a.context.object_proto);
        Ok(ObjectData::ordinary(a.mc, Some(proto), usize::from(ctor.instance_size.get())))
    }

    fn return_value<'scope>(&self, a: &Scope<'scope, '_>, regs: &[Value<'scope>], f: &Frame, v: Value<'scope>) -> Result<Value<'scope>, Error> {
        if f.slot(regs, Frame::NEW_TARGET) == Value::Undefined || v.as_object().is_some() {
            return Ok(v);
        }
        let loaded = self.function(f.func);
        let r = match (loaded.derived, f.new_this(regs)) {
            (true, _) if v != Value::Undefined => Err(a.throw(ErrorKind::Type, "Derived constructors may only return object or undefined")),
            (true, None) => Err(a.throw(ErrorKind::Reference, "Must call super constructor in derived class before accessing 'this' or returning from derived constructor")),
            (_, Some(t)) => Ok(t),
            (_, None) => Ok(v),
        };
        if let Ok(ValueKind::Object(o)) = r.as_ref().map(|v| v.kind()) {
            loaded.note_instance(o);
        }
        r
    }

    #[inline]
    fn create_closure<'scope>(&self, a: &Scope<'scope, '_>, loaded: &Loaded, k: OperandValue, env: Option<JsEnv<'scope>>) -> JsObject<'scope> {
        let Some(Constant::Function(n)) = loaded.func.bytecode.constants.get(usize::try_from(k).unwrap_or(0)) else { unreachable!("a function constant") };
        self.function_object(a, Closure::new(FuncId(loaded.base + n), env))
    }

    fn function_object<'scope>(&self, a: &Scope<'scope, '_>, mut closure: Closure<'scope>) -> JsObject<'scope> {
        let loaded = self.function(closure.func);
        closure.code = Some(loaded.code());
        let func = &loaded.func;
        let proto = match (loaded.generator, loaded.asynchronous) {
            (true, true) => a.context.async_generator_function_proto,
            (true, false) => a.context.generator_function_proto,
            (false, true) => a.context.async_function_proto,
            (false, false) => a.context.function_proto,
        };
        let fo = ObjectData { kind: ObjectKind::Function(closure), proto: Some(proto), ..ObjectData::default() }.alloc(a.mc);
        let length = Value::Number(f64::from(func.length));
        let name = Value::lazy(Lazy::FunctionName);
        let prototype = if loaded.generator {
            let base = if loaded.asynchronous { a.context.async_generator_proto } else { a.context.generator_proto };
            Some((ObjectData { proto: Some(base), ..ObjectData::default() }.alloc(a.mc), true))
        } else if !loaded.no_prototype && !loaded.asynchronous {
            let p = a.object();
            match loaded.prototype_shape.get() {
                Some(shape) => p.fill(a.mc, unsafe { shape.handle() }, [Slot::Data(fo.into())]),
                None => p.borrow_mut(a.mc).define("constructor", Property::hidden(fo.into())),
            }
            Self::remember_shape(&loaded.prototype_shape, p, 1);
            Some((p, !loaded.class_constructor))
        } else {
            None
        };
        match loaded.function_shape.get() {
            Some(shape) => fo.fill(a.mc, unsafe { shape.handle() }, [Slot::Data(length), Slot::Data(name)].into_iter().chain(prototype.map(|(p, _)| Slot::Data(p.into())))),
            None => {
                let prop = |v: Value<'scope>| Property { slot: Slot::Data(v), enumerable: false, writable: false, configurable: true };
                let d = fo.borrow_mut(a.mc);
                d.define("length", prop(length));
                d.define("name", prop(name));
                if let Some((p, writable)) = prototype {
                    d.define("prototype", Property { slot: Slot::Data(p.into()), enumerable: false, writable, configurable: false });
                }
            }
        }
        Self::remember_shape(&loaded.function_shape, fo, 2 + usize::from(prototype.is_some()));
        fo
    }

    fn object_literal<'scope>(a: &Scope<'scope, '_>, loaded: &Loaded, k: OperandValue) -> JsObject<'scope> {
        let k = usize::try_from(k).unwrap_or(0);
        let Some(Constant::Object(b)) = loaded.func.bytecode.constants.get(k) else { unreachable!("an object boilerplate") };
        let ob = a.object_with(b.properties.len());
        let cache = &loaded.literals[k];
        let value = |v: &Option<LiteralValue>| v.as_ref().map_or(Value::Undefined, |v| Self::literal(a, v));
        match cache.get() {
            Some(shape) => {
                ob.fill(a.mc, unsafe { shape.handle() }, b.properties.iter().map(|(_, v)| Slot::Data(value(v))));
                return ob;
            }
            None => {
                for (key, v) in &b.properties {
                    ob.borrow_mut(a.mc).define(key, Property::data(value(v)));
                }
            }
        }
        Self::remember_shape(cache, ob, b.properties.len());
        ob
    }

    pub(crate) fn regexp_literal<'scope>(a: &mut Scope<'scope, '_>, loaded: &Loaded, pattern: OperandValue, bits: OperandValue) -> Result<(Value<'scope>, bool), Error> {
        let bits = u16::try_from(bits).unwrap_or(0);
        let k = usize::try_from(pattern).unwrap_or(usize::MAX);
        let lone = match loaded.func.bytecode.constants.get(k) {
            Some(Constant::Utf16(u)) => Some(u.as_ref()),
            _ => None,
        };
        match (a.context.regexp_literal.get().0, lone) {
            (Some(literal), None) => Ok((literal(a, Self::name_at(loaded, pattern), bits)?, true)),
            (_, lone) => {
                let mut flags = [0u8; REGEXP_FLAGS.len()];
                let mut n = 0;
                for (c, b) in REGEXP_FLAGS {
                    if bits & b != 0 && let Some(f) = flags.get_mut(n) {
                        *f = c;
                        n += 1;
                    }
                }
                let flags = core::str::from_utf8(flags.get(..n).unwrap_or_default()).unwrap_or_default();
                let p = match lone {
                    Some(u) => a.string_units(u),
                    None => a.string(Self::name_at(loaded, pattern)),
                };
                let fl = a.string(flags);
                Ok((a.call(a.context.regexp_ctor.into(), Value::Undefined, &[p.into(), fl.into()])?, false))
            }
        }
    }

    fn remember_shape(cache: &Cell<Option<ShapeKey>>, o: JsObject<'_>, n: usize) {
        if cache.get().is_some() {
            return;
        }
        cache.set(o.get().props().shape().filter(|s| s.len() == n).map(ShapeKey::from));
    }

    fn script_slot<'scope>(&self, a: &Scope<'scope, '_>, name: &str) -> Option<(JsEnv<'scope>, usize, Mutability)> {
        a.context.script_contexts.get().iter().find_map(|e| {
            let (i, n) = self.context_shape(e.shape?).slot(name)?;
            Some((*e, i, Mutability::from(n)))
        })
    }

    #[cold]
    fn uninitialized(a: &Scope<'_, '_>, name: &str) -> Error {
        a.throw(ErrorKind::Reference, &alloc::format!("Cannot access '{name}' before initialization"))
    }

    fn global_load<'scope>(&self, a: &mut Scope<'scope, '_>, cache: &ic::GlobalIc, name: &str, inside_typeof: bool) -> Result<Value<'scope>, Error> {
        if name == SCRIPT_THIS {
            return Ok(Value::Object(a.context.global));
        }
        let (scope, global) = (a.context.script_scope, a.context.global);
        if let Some(v) = cache.get(scope, global) {
            return Ok(v);
        }
        if let Some((e, i, _)) = self.script_slot(a, name) {
            let v = e.load(i);
            return if matches!(v.kind(), ValueKind::Uninit) { Err(Self::uninitialized(a, name)) } else { Ok(v) };
        }
        let g = Value::Object(global);
        if !a.has_property(g, name)? {
            if inside_typeof {
                return Ok(Value::Undefined);
            }
            return Err(a.throw(ErrorKind::Reference, &alloc::format!("{name} is not defined")));
        }
        let v = a.get_named(g, name)?;
        cache.fill(a.mc, scope, global, name);
        Ok(v)
    }

    fn global_store<'scope>(&self, a: &mut Scope<'scope, '_>, cache: &ic::GlobalIc, name: &str, v: Value<'scope>, strict: bool) -> Result<(), Error> {
        let (scope, global) = (a.context.script_scope, a.context.global);
        if let Some((o, at)) = cache.writable(scope, global)
            && let Slot::Data(_) = o.get().props().slot(at)
        {
            o.get().set_at(a.mc, at, v);
            return Ok(());
        }
        if let Some((e, i, mutability)) = self.script_slot(a, name) {
            if matches!(e.load(i).kind(), ValueKind::Uninit) {
                return Err(Self::uninitialized(a, name));
            }
            if mutability != Mutability::Mutable {
                return Err(a.throw(ErrorKind::Type, "Assignment to constant variable."));
            }
            Env::set(e, a.mc, i, v);
            return Ok(());
        }
        let g = Value::Object(global);
        if strict && !a.has_property(g, name)? {
            return Err(a.throw(ErrorKind::Reference, &alloc::format!("{name} is not defined")));
        }
        a.put_named(g, name, v, strict)?;
        cache.fill(a.mc, scope, global, name);
        Ok(())
    }

    fn lookup<'scope>(&self, a: &mut Scope<'scope, '_>, env: Option<JsEnv<'scope>>, name: &str) -> Result<Found<'scope>, Error> {
        let mut e = env;
        while let Some(c) = e {
            if let Some(ext) = c.extension {
                let with = c.shape.is_some_and(|s| self.context_shape(s).kind == ContextKind::With);
                let has = if with { a.has_property(Value::Object(ext), name)? } else { ext.borrow().get_own(a.mc, name).is_some() };
                if has && !(with && Self::unscopable(a, ext, name)?) {
                    return Ok(if with { Found::With(ext) } else { Found::Object(ext) });
                }
            }
            if let Some((i, n)) = c.shape.and_then(|s| self.context_shape(s).slot(name)) {
                return Ok(Found::Slot(c, i, Mutability::from(n)));
            }
            e = c.parent;
        }
        if let Some((e, i, constant)) = self.script_slot(a, name) {
            return Ok(Found::Slot(e, i, constant));
        }
        let g = Value::Object(a.context.global);
        if a.has_property(g, name)? {
            return Ok(Found::Object(a.context.global));
        }
        Ok(Found::Nothing)
    }

    fn unscopable<'scope>(a: &mut Scope<'scope, '_>, o: JsObject<'scope>, name: &str) -> Result<bool, Error> {
        let u = a.get_symbol(Value::Object(o), a.context.symbols.unscopables, Value::Object(o))?;
        if u.as_object().is_none() {
            return Ok(false);
        }
        let blocked = a.get_named(u, name)?;
        Ok(a.to_boolean(blocked))
    }

    fn lookup_load<'scope>(&self, a: &mut Scope<'scope, '_>, env: Option<JsEnv<'scope>>, name: &str, inside_typeof: bool) -> Result<Value<'scope>, Error> {
        let found = self.lookup(a, env, name)?;
        Self::load_found(a, found, name, inside_typeof)
    }

    fn load_found<'scope>(a: &mut Scope<'scope, '_>, found: Found<'scope>, name: &str, inside_typeof: bool) -> Result<Value<'scope>, Error> {
        match found {
            Found::Slot(c, i, _) => {
                let v = c.load(i);
                if matches!(v.kind(), ValueKind::Uninit) { Err(Self::uninitialized(a, name)) } else { Ok(v) }
            }
            Found::Object(o) | Found::With(o) => a.get_named(Value::Object(o), name),
            Found::Nothing if name == "this" => Ok(Value::Object(a.context.global)),
            Found::Nothing if inside_typeof => Ok(Value::Undefined),
            Found::Nothing => Err(a.throw(ErrorKind::Reference, &alloc::format!("{name} is not defined"))),
        }
    }

    fn lookup_for_call<'scope>(&self, a: &mut Scope<'scope, '_>, env: Option<JsEnv<'scope>>, name: &str) -> Result<(Value<'scope>, Value<'scope>), Error> {
        match self.lookup(a, env, name)? {
            Found::With(o) => Ok((a.get_named(Value::Object(o), name)?, Value::Object(o))),
            found => Ok((Self::load_found(a, found, name, false)?, Value::Undefined)),
        }
    }

    fn lookup_store<'scope>(&self, a: &mut Scope<'scope, '_>, env: Option<JsEnv<'scope>>, name: &str, v: Value<'scope>, strict: bool) -> Result<(), Error> {
        match self.lookup(a, env, name)? {
            Found::Slot(c, i, mutability) => {
                if matches!(c.load(i).kind(), ValueKind::Uninit) {
                    return Err(Self::uninitialized(a, name));
                }
                match mutability {
                    Mutability::Const => Err(a.throw(ErrorKind::Type, "Assignment to constant variable.")),
                    Mutability::FunctionName if strict => Err(a.throw(ErrorKind::Type, "Assignment to constant variable.")),
                    Mutability::FunctionName => Ok(()),
                    Mutability::Mutable => {
                        Env::set(c, a.mc, i, v);
                        Ok(())
                    }
                }
            }
            Found::Object(o) | Found::With(o) => a.put_named(Value::Object(o), name, v, strict),
            Found::Nothing if strict => Err(a.throw(ErrorKind::Reference, &alloc::format!("{name} is not defined"))),
            Found::Nothing => a.put_named(Value::Object(a.context.global), name, v, false),
        }
    }

    fn declaration_context<'scope>(&self, env: Option<JsEnv<'scope>>) -> Option<JsEnv<'scope>> {
        let mut e = env;
        while let Some(c) = e {
            match c.shape.map(|s| self.context_shape(s).kind) {
                Some(ContextKind::Catch | ContextKind::With | ContextKind::Block) => e = c.parent,
                _ => break,
            }
        }
        e
    }

    fn var_scope<'scope>(&self, a: &Scope<'scope, '_>, env: Option<JsEnv<'scope>>) -> JsObject<'scope> {
        let mut e = env;
        while let Some(c) = e {
            if let (Some(ext), Some(ContextKind::Function)) = (c.extension, c.shape.map(|s| self.context_shape(s).kind)) {
                return ext;
            }
            e = c.parent;
        }
        a.context.global
    }

    fn check_eval<'scope>(&self, a: &mut Scope<'scope, '_>, env: Option<JsEnv<'scope>>, name: &str, kind: j8bytecode::EvalDeclaration) -> Result<(), Error> {
        let mut e = env;
        while let Some(c) = e {
            if let Some(s) = c.shape.map(|s| self.context_shape(s)) {
                let body_started = |i: usize| s.kind != ContextKind::Function || !matches!(c.load(i).kind(), ValueKind::Uninit);
                if s.slot(name).is_some_and(|(i, n)| n.lexical && body_started(i)) {
                    return Err(a.throw(ErrorKind::Syntax, &alloc::format!("Identifier '{name}' has already been declared")));
                }
                if s.kind == ContextKind::Function && c.extension.is_some() {
                    return Ok(());
                }
            }
            e = c.parent;
        }
        let target = self.var_scope(a, env);
        if !ScopeLocal::ptr_eq(target, a.context.global) {
            return Ok(());
        }
        if self.script_slot(a, name).is_some() {
            return Err(a.throw(ErrorKind::Syntax, &alloc::format!("Identifier '{name}' has already been declared")));
        }
        let (existing, extensible) = {
            let g = target.borrow();
            (g.get_own(a.mc, name), !g.non_extensible())
        };
        match (existing, kind) {
            (None, _) if !extensible => Err(a.throw(ErrorKind::Type, &alloc::format!("Cannot declare global {name}: the global object is not extensible"))),
            (Some(p), j8bytecode::EvalDeclaration::Function) if !p.configurable && !(p.writable && p.enumerable && matches!(p.slot, Slot::Data(_))) => Err(a.throw(ErrorKind::Type, &alloc::format!("Cannot redefine property: {name}"))),
            _ => Ok(()),
        }
    }

    fn declare_eval<'scope>(&self, a: &mut Scope<'scope, '_>, env: Option<JsEnv<'scope>>, name: &str, value: Option<Value<'scope>>) -> Result<(), Error> {
        let mut e = env;
        while let Some(c) = e {
            if let Some(s) = c.shape.map(|s| self.context_shape(s)) {
                let body_started = |i: usize| s.kind != ContextKind::Function || !matches!(c.load(i).kind(), ValueKind::Uninit);
                if s.slot(name).is_some_and(|(i, n)| n.lexical && body_started(i)) {
                    return Err(a.throw(ErrorKind::Syntax, &alloc::format!("Identifier '{name}' has already been declared")));
                }
                if s.kind == ContextKind::Function && c.extension.is_some() {
                    if let Some((i, n)) = s.slot(name)
                        && !n.lexical
                    {
                        if let Some(v) = value {
                            Env::set(c, a.mc, i, v);
                        }
                        return Ok(());
                    }
                    break;
                }
            }
            e = c.parent;
        }
        let target = self.var_scope(a, env);
        if ScopeLocal::ptr_eq(target, a.context.global) && self.script_slot(a, name).is_some() {
            return Err(a.throw(ErrorKind::Syntax, &alloc::format!("Identifier '{name}' has already been declared")));
        }
        let existing = target.borrow().get_own(a.mc, name);
        match (existing, value) {
            (Some(_), None) => {}
            (Some(p), Some(v)) if !p.configurable => a.put_named(Value::Object(target), name, v, false)?,
            (_, v) => target.borrow_mut(a.mc).define(name, Property { slot: Slot::Data(v.unwrap_or_default()), enumerable: true, writable: true, configurable: true }),
        }
        Ok(())
    }

    fn declare_globals<'scope>(&self, a: &mut Scope<'scope, '_>, f: &Frame, env: Option<JsEnv<'scope>>, decls: &[j8bytecode::GlobalDeclaration]) -> Result<(), Error> {
        for d in decls {
            let name = &*d.name;
            if self.script_slot(a, name).is_some() {
                return Err(a.throw(ErrorKind::Syntax, &alloc::format!("Identifier '{name}' has already been declared")));
            }
            let (existing, extensible) = {
                let g = a.context.global.borrow();
                (g.get_own(a.mc, name), !g.non_extensible())
            };
            match existing {
                Some(p) if d.function.is_some() && !p.configurable && !(p.writable && p.enumerable && matches!(p.slot, Slot::Data(_))) => return Err(a.throw(ErrorKind::Type, &alloc::format!("Cannot redefine property: {name}"))),
                None if !extensible => return Err(a.throw(ErrorKind::Type, &alloc::format!("Cannot declare global {name}: the global object is not extensible"))),
                _ => {}
            }
        }
        let base = self.function(f.func).base;
        for d in decls {
            let name = &*d.name;
            let global = a.context.global;
            let existing = global.borrow().get_own(a.mc, name);
            let value = d.function.map(|n| {
                let closure = Closure::new(FuncId(base + n), env);
                Value::Object(self.function_object(a, closure))
            });
            let fresh = Property { slot: Slot::Data(value.unwrap_or_default()), enumerable: true, writable: true, configurable: false };
            let prop = match (existing, value) {
                (None, _) => fresh,
                (Some(_), None) => continue,
                (Some(p), Some(_)) if p.configurable => fresh,
                (Some(p), Some(v)) => Property { slot: Slot::Data(v), ..p },
            };
            global.borrow_mut(a.mc).define(name, prop);
        }
        Ok(())
    }

    fn script_context<'scope>(&self, a: &mut Scope<'scope, '_>, main: FuncId) -> Result<Option<JsEnv<'scope>>, Error> {
        let Some(id) = self.function(main).scope else { return Ok(None) };
        let shape = self.context_shape(id);
        if shape.kind != ContextKind::Script {
            return Ok(None);
        }
        for n in shape.names.iter() {
            let name = &*n.name;
            let restricted = a.context.global.borrow().get_own(a.mc, name).is_some_and(|p| !p.configurable);
            if self.script_slot(a, name).is_some() || restricted {
                return Err(a.throw(ErrorKind::Syntax, &alloc::format!("Identifier '{name}' has already been declared")));
            }
        }
        let env = Env::context(shape.names.len(), Value::Uninit, None, Some(id), None).alloc(a.mc);
        a.context.script_contexts.update(a.mc, |v| {
            let mut v = v.clone();
            v.push(env);
            v
        });
        for n in shape.names.iter() {
            a.context.script_scope.borrow_mut(a.mc).define(&n.name, Property::data(Value::Undefined));
        }
        Ok(Some(env))
    }

    fn script_state<'scope>(&self, a: &mut Scope<'scope, '_>, main: FuncId) -> Result<State<'scope>, Error> {
        let env = self.script_context(a, main)?;
        let this = Value::Object(a.context.global);
        let mut regs = Vec::new();
        let f = Self::open(a, &mut regs, self.function(main), Closure::new(main, env), Value::Undefined, this, Args::Values(&[]), Opening::Call);
        Ok(self.state(regs, f))
    }

    pub fn run_script<'scope>(&self, a: &mut Scope<'scope, '_>, main: FuncId) -> Result<Value<'scope>, Error> {
        let st = self.script_state(a, main)?;
        self.execute(a, st, &mut None).map(Outcome::returned)
    }

    fn unwind<'scope>(st: &mut State<'scope>, v: Value<'scope>) -> bool {
        loop {
            if st.frames.is_empty() {
                return false;
            }
            let loaded = unsafe { st.f().loaded.get() };
            let at = loaded.insns.get(st.f().insn).map_or(u32::MAX, |d| d.start);
            let h = loaded.func.bytecode.handlers.iter().filter(|h| h.start <= at && at < h.end).min_by_key(|h| h.end - h.start).copied();
            if let Some(h) = h {
                let ctx = Self::reg(&st.regs, st.f(), OperandValue::from(h.context));
                let f = *st.f();
                f.set_env(&mut st.regs, ctx.as_context());
                st.f_mut().pc = loaded.index_of(h.handler as usize);
                st.acc = v;
                return true;
            }
            if !st.pop_frame() {
                return false;
            }
        }
    }

    fn execute<'scope>(&self, a: &mut Scope<'scope, '_>, mut st: State<'scope>, budget: &mut Option<usize>) -> Result<Outcome<'scope>, Error> {
        if self.debug.is_some() {
            self.debug_new_code(a);
        }
        st.outer = self.depth.get();
        let _depth = self.at_depth(st.outer);
        let _raw = (budget.is_none() && !self.safe_next.take()).then(|| self.raw_frame(a.stop));
        let running = Running { state: NonNull::from(&st).cast(), outer: self.active.0.get() };
        self.active.0.set(Some(NonNull::from(&running)));
        let mut b = Budget::new(a.stop, *budget);
        let r = loop {
            match self.run(a, &mut st, &mut b) {
                Ok(None | Some(Outcome::Paused(_))) if b.terminated => break Err(Error::Terminated),
                Ok(Some(v)) => break Ok(Some(v)),
                Ok(None) => break Ok(None),
                Err(Error::Exception) => {
                    let reported = self.debug.is_none() || a.mc.rethrown();
                    let v = a.take_exception();
                    if !reported {
                        self.debug_throw(a, v, st.level());
                    }
                    if !Self::unwind(&mut st, v) {
                        break Err(a.rethrow(v));
                    }
                }
                Err(e) => break Err(e),
            }
        };
        self.active.0.set(running.outer);
        let r = match r {
            Ok(None) => Ok(Outcome::Paused(Box::new(st))),
            r => {
                self.recycle(a, st);
                r.map(|v| v.expect("an outcome"))
            }
        };
        *budget = b.remaining();
        r
    }

    fn run<'scope>(&self, a: &mut Scope<'scope, '_>, st: &mut State<'scope>, budget: &mut Budget<'_>) -> Result<Option<Outcome<'scope>>, Error> {
        loop {
            if self.halts(a, budget) == Halt::Pause {
                return Ok(None);
            }
            let loaded: &Loaded = unsafe { st.f().loaded.get() };
            let generation = a.mc.generation();
            let mut cx = threaded::Cx { exe: self, a, st, loaded, budget, generation };
            match Self::threaded(&mut cx) {
                threaded::Exit::Frame => {}
                threaded::Exit::Done(o) => return Ok(Some(o)),
                threaded::Exit::Throw(e) => return Err(e),
                #[cfg(not(tail_calls))]
                threaded::Exit::Next(..) => unreachable!("the loop in `threaded` takes these"),
            }
        }
    }

    #[inline]
    fn halts(&self, a: &mut Scope<'_, '_>, budget: &mut Budget<'_>) -> Halt {
        let mut collected = false;
        while budget.interrupted || budget.spent() {
            if !core::mem::take(&mut budget.interrupted) {
                return Halt::Pause;
            }
            self.serve_interrupts(a);
            collected |= self.serve_collect(a);
        }
        if collected { Halt::Collected } else { Halt::Go }
    }

    fn raw_frame<'e>(&'e self, stop: &'e j8framework::Termination) -> RawFrame<'e> {
        self.raw.set(self.raw.get() + 1);
        RawFrame { raw: &self.raw, deferred: &self.deferred, stop }
    }

    fn serve_collect(&self, a: &mut Scope<'_, '_>) -> bool {
        if !a.stop.take_collect() {
            return false;
        }
        if self.raw.get() > 0 {
            self.deferred.set(true);
            return false;
        }
        self.collect_at_safepoint(a)
    }

    fn collect_at_safepoint<'scope>(&self, a: &mut Scope<'scope, '_>) -> bool {
        let mut states: Vec<NonNull<State<'scope>>> = Vec::new();
        let mut at = self.active.0.get();
        while let Some(running) = at {
            let running = unsafe { running.as_ref() };
            states.push(running.state.cast());
            at = running.outer;
        }
        let pending = a.mc.take_exception();
        let mut values: Vec<Value<'scope>> = Vec::new();
        for st in &states {
            let st = unsafe { st.as_ref() };
            values.extend_from_slice(&st.regs);
            values.push(st.acc);
            values.push(st.message);
        }
        values.extend_from_slice(&a.handles);
        values.push(a.new_target);
        values.push(pending.unwrap_or(Value::Undefined));
        let context = a.mc.stash::<j8framework::ContextBrand>(a.context);
        let codes: Box<[Code]> = states.iter().flat_map(|st| unsafe { st.as_ref() }.frames.iter().map(|f| f.loaded.0)).collect();
        let codes = a.mc.stash::<FrameCodesBrand>(a.mc.alloc(FrameCodes(codes)));
        let used = self.hold_roots(a, &values);
        let full = a.heap_limit_pending();
        let collected = if full {
            unsafe { a.mc.collect_full_here() };
            true
        } else {
            unsafe { a.mc.collect_here() }
        };
        let cells = self.root_cells(a);
        let mut back = cells.0.iter().map(|c| c.get());
        for st in &mut states {
            let st = unsafe { st.as_mut() };
            for (r, v) in st.regs.iter_mut().zip(&mut back) {
                *r = v;
            }
            st.acc = back.next().unwrap_or_default();
            st.message = back.next().unwrap_or_default();
        }
        for h in &mut a.handles {
            *h = back.next().unwrap_or_default();
        }
        a.new_target = back.next().unwrap_or_default();
        let thrown = back.next().unwrap_or_default();
        for c in &cells.0[..used] {
            c.set(a.mc, Value::Undefined);
        }
        if pending.is_some() {
            let _ = a.mc.raise(thrown);
        }
        a.context = a.mc.fetch(&context);
        drop(codes);
        a.recheck_heap_limit();
        collected
    }

    fn root_cells<'scope>(&self, a: &Scope<'scope, '_>) -> j8framework::ScopeLocal<'scope, RootCells<'scope>> {
        let roots = self.roots.borrow();
        let Some(g) = roots.as_ref() else { unreachable!("cells held before collecting") };
        a.mc.fetch(g)
    }

    fn hold_roots<'scope>(&self, a: &Scope<'scope, '_>, values: &[Value<'scope>]) -> usize {
        let fits = self.roots.borrow().as_ref().is_some_and(|g| a.mc.fetch(g).0.len() >= values.len());
        if !fits {
            let n = values.len().next_power_of_two().max(MIN_ROOT_CELLS);
            let cells = RootCells((0..n).map(|_| j8framework::ValueCell::new(Value::Undefined)).collect());
            *self.roots.borrow_mut() = Some(a.mc.stash::<RootsBrand>(a.mc.alloc(cells)));
        }
        let cells = self.root_cells(a);
        for (c, v) in cells.0.iter().zip(values) {
            c.set(a.mc, *v);
        }
        values.len()
    }

    #[inline]
    fn record_call(loaded: &Loaded, fb: OperandValue, f: FuncId) {
        if let Some(ic) = loaded.call_ics.get(usize::try_from(fb).unwrap_or(usize::MAX)) {
            let id = f.0.wrapping_add(1);
            match ic.get() {
                0 => ic.set(id),
                x if x != id => ic.set(u32::MAX),
                _ => {}
            }
        }
    }

    #[inline]
    fn record_native(loaded: &Loaded, fb: OperandValue, f: j8framework::NativeFn) {
        if let Some(n) = loaded.native_ics.get(usize::try_from(fb).unwrap_or(usize::MAX)) {
            let p = f as usize as u64;
            let seen = n.get();
            if seen != p {
                n.set(if seen == 0 { p } else { u64::MAX });
            }
        }
    }

    fn call_from<'scope>(&self, a: &mut Scope<'scope, '_>, st: &mut State<'scope>, callee: Value<'scope>, this: Value<'scope>, args: Args<'_, 'scope>) -> Result<Flow<'scope>, Error> {
        let level = Self::within(a, st.level() + 1)?;
        match self.resolve(a, &st.regs, callee, this, args, level)? {
            Callee::Closure { closure, callee, this, args } => {
                let next = Self::open(a, &mut st.regs, self.code(&closure), closure, callee, this, args.args(), Opening::Call);
                st.push(next);
                Ok(Flow::Frame)
            }
            Callee::Done(v) => {
                st.acc = v;
                Ok(Flow::Next)
            }
        }
    }

    fn construct_from<'scope>(&self, a: &mut Scope<'scope, '_>, st: &mut State<'scope>, callee: Value<'scope>, args: Vec<Value<'scope>>, new_target: Value<'scope>) -> Result<Flow<'scope>, Error> {
        let level = Self::within(a, st.level() + 1)?;
        match self.prepare(a, &mut st.regs, callee, args, new_target, level)? {
            Prepared::Frame(next) => {
                st.push(next);
                Ok(Flow::Frame)
            }
            Prepared::Done(v) => {
                st.acc = v;
                Ok(Flow::Next)
            }
        }
    }

    fn constant<'scope>(a: &Scope<'scope, '_>, c: &Constant) -> Value<'scope> {
        match c {
            Constant::Number(x) => Value::Number(*x),
            Constant::String(s) => a.string(s).into(),
            Constant::Utf16(u) => a.string_units(u).into(),
            Constant::BigInt(digits) => a.bigint(Scope::parse_bigint(digits).unwrap_or_default()),
            Constant::AsyncIteratorSymbol => a.context.symbols.async_iterator.into(),
            _ => Value::Undefined,
        }
    }

    fn literal<'scope>(a: &Scope<'scope, '_>, v: &LiteralValue) -> Value<'scope> {
        match v {
            LiteralValue::Number(x) => Value::Number(*x),
            LiteralValue::String(s) => a.string(s).into(),
            LiteralValue::Bool(b) => Value::Bool(*b),
            LiteralValue::Null => Value::Null,
            LiteralValue::Undefined | LiteralValue::Hole => Value::Undefined,
        }
    }

    fn name_at(loaded: &Loaded, k: OperandValue) -> &str {
        match loaded.func.bytecode.constants.get(usize::try_from(k).unwrap_or(usize::MAX)) {
            Some(Constant::String(s)) => s,
            _ => "",
        }
    }

    #[expect(clippy::too_many_lines, reason = "one arm per bytecode")]
    fn step<'scope>(&self, a: &mut Scope<'scope, '_>, st: &mut State<'scope>, loaded: &Loaded, d: &Insn, budget: &mut Budget<'_>) -> Result<Flow<'scope>, Error> {
        let o = &d.operands;
        let State { regs, frames, acc, message, .. } = st;
        let f = frames.last_mut().expect("a running frame");
        macro_rules! r {
            ($i:expr) => {
                Self::reg(regs, f, o[$i])
            };
        }
        let strict = loaded.func.strict;
        macro_rules! arith {
            ($l:expr, $r:expr, $op:ident, $sym:tt) => {{
                let (l, r) = ($l, $r);
                *acc = match (l.as_number(), r.as_number()) {
                    (Some(x), Some(y)) => Value::Number(x $sym y),
                    _ => a.binary(BinaryOp::$op, l, r)?,
                };
            }};
        }
        macro_rules! relational {
            ($op:ident, $sym:tt) => {{
                let l = r!(0);
                *acc = match (l.as_number(), acc.as_number()) {
                    (Some(x), Some(y)) => Value::Bool(x $sym y),
                    _ => Value::Bool(a.compare(CompareOp::$op, l, *acc)?),
                };
            }};
        }
        let binary = |op: BinaryOp, a: &mut Scope<'scope, '_>, l: Value<'scope>, r: Value<'scope>| -> Result<Value<'scope>, Error> {
            match (l.as_number(), r.as_number()) {
                (Some(x), Some(y)) => Ok(op.numbers(x, y).into()),
                _ => a.binary(op, l, r),
            }
        };
        let compare = |op: CompareOp, a: &mut Scope<'scope, '_>, l: Value<'scope>, r: Value<'scope>| -> Result<Value<'scope>, Error> {
            match (l.as_number(), r.as_number()) {
                (Some(x), Some(y)) => Ok(op.numbers(x, y).into()),
                _ => a.compare(op, l, r).map(Value::Bool),
            }
        };
        use Bytecode as B;
        match d.bytecode {
            B::LdaZero => *acc = Value::Number(0.0),
            B::LdaSmi => *acc = Value::Number(o[0] as f64),
            B::LdaUndefined => *acc = Value::Undefined,
            B::LdaNull => *acc = Value::Null,
            B::LdaTheHole => *acc = Value::Uninit,
            B::LdaTrue => *acc = Value::Bool(true),
            B::LdaFalse => *acc = Value::Bool(false),
            B::LdaConstant => {
                let k = usize::try_from(o[0]).unwrap_or(0);
                let slot = u32::try_from(k).unwrap_or(0);
                *acc = match &loaded.func.bytecode.constants[k] {
                    Constant::Class(_) => Value::Number(k as f64),
                    Constant::String(s) => a.constant_string(loaded.id.0, slot, s).into(),
                    Constant::Utf16(u) => a.constant_units(loaded.id.0, slot, u).into(),
                    c => Self::constant(a, c),
                };
            }
            B::Ldar => *acc = r!(0),
            B::Star => Self::set_reg(regs, f, o[0], *acc),
            B::Mov => {
                let v = r!(0);
                Self::set_reg(regs, f, o[1], v);
            }
            B::LdaGlobal | B::LdaGlobalInsideTypeof => {
                let name = Self::name_at(loaded, o[0]);
                *acc = self.global_load(a, &loaded.globals[usize::try_from(o[1]).unwrap_or(0)], name, d.bytecode == B::LdaGlobalInsideTypeof)?;
            }
            B::StaGlobal => {
                let name = Self::name_at(loaded, o[0]);
                let slot = usize::try_from(o[1]).unwrap_or(0);
                let strict_slot = loaded.func.bytecode.feedback.get(slot) == Some(&j8bytecode::FeedbackSlotKind::StoreGlobalStrict);
                self.global_store(a, &loaded.globals[slot], name, *acc, strict_slot || strict)?;
            }
            B::LdaModuleVariable => {
                let cell = o[0];
                if cell >= 0 {
                    return Err(a.throw(ErrorKind::Reference, "module export cells are context slots"));
                }
                *acc = self.import_value(a, loaded.base, u16::try_from(-1 - cell).unwrap_or(u16::MAX))?;
            }
            B::LdaCurrentContextSlot | B::LdaCurrentContextSlotNoCell | B::LdaImmutableCurrentContextSlot => {
                let e = f.env(regs).ok_or_else(|| a.throw(ErrorKind::Reference, "no context"))?;
                *acc = e.load(Self::env_index(o[0]));
            }
            B::LdaContextSlot | B::LdaContextSlotNoCell | B::LdaImmutableContextSlot => {
                let e = Self::context_at(a, r!(0), o[2])?;
                *acc = e.load(Self::env_index(o[1]));
            }
            B::StaCurrentContextSlot | B::StaCurrentContextSlotNoCell => {
                let e = f.env(regs).ok_or_else(|| a.throw(ErrorKind::Reference, "no context"))?;
                Env::set(e, a.mc, Self::env_index(o[0]), *acc);
            }
            B::StaContextSlot | B::StaContextSlotNoCell => {
                let e = Self::context_at(a, r!(0), o[2])?;
                Env::set(e, a.mc, Self::env_index(o[1]), *acc);
            }
            B::PushContext => {
                let cur = f.env(regs).map_or(Value::Undefined, Value::context);
                Self::set_reg(regs, f, o[0], cur);
                f.set_env(regs, acc.as_context());
            }
            B::PopContext => {
                let e = r!(0).as_context();
                f.set_env(regs, e);
            }
            B::CreateFunctionContext | B::CreateEvalContext => {
                let shape = loaded.scope_shape(o[0]).map(|(s, _)| s);
                let n = usize::try_from(o[1]).unwrap_or(0);
                let extension = loaded.extension.then(|| ObjectData::default().alloc(a.mc));
                *acc = Value::context(Env::context(n, Value::Undefined, f.env(regs), shape, extension).alloc(a.mc));
            }
            B::CreateBlockContext => *acc = loaded.block_context(a, o[0], f.env(regs)),
            B::CreateCatchContext => {
                let shape = loaded.scope_shape(o[1]).map(|(s, _)| s);
                let e = Env::context(1, r!(0), f.env(regs), shape, None).alloc(a.mc);
                *acc = Value::context(e);
            }
            B::CreateWithContext => {
                let obj = r!(0).as_object();
                let shape = Some(self.with_shape.get().unwrap_or_else(|| {
                    let id = ShapeId(u32::try_from(self.context_shapes.push(ContextShape { kind: ContextKind::With, names: Box::new([]) })).expect("fewer than 2^32 context shapes"));
                    self.with_shape.set(Some(id));
                    id
                }));
                *acc = Value::context(Env::context(0, Value::Undefined, f.env(regs), shape, obj).alloc(a.mc));
            }
            B::LdaLookupSlot | B::LdaLookupSlotInsideTypeof | B::LdaLookupGlobalSlot | B::LdaLookupGlobalSlotInsideTypeof | B::LdaLookupContextSlot | B::LdaLookupContextSlotInsideTypeof | B::LdaLookupContextSlotNoCell | B::LdaLookupContextSlotNoCellInsideTypeof => {
                let name = Self::name_at(loaded, o[0]);
                let inside = matches!(d.bytecode, B::LdaLookupSlotInsideTypeof | B::LdaLookupGlobalSlotInsideTypeof | B::LdaLookupContextSlotInsideTypeof | B::LdaLookupContextSlotNoCellInsideTypeof);
                *acc = self.lookup_load(a, f.env(regs), name, inside)?;
            }
            B::StaLookupSlot => {
                let name = Self::name_at(loaded, o[0]);
                let flags = u8::try_from(o[1]).unwrap_or(0);
                let s = flags & j8bytecode::StoreLookupSlotFlags::STRICT != 0;
                let mut env = f.env(regs);
                if flags & j8bytecode::StoreLookupSlotFlags::LOOKUP_HOISTING != 0 {
                    env = self.declaration_context(env);
                }
                self.lookup_store(a, env, name, *acc, s)?;
            }
            B::GetNamedProperty => {
                let obj = r!(0);
                let name = Self::name_at(loaded, o[1]);
                let ic = &loaded.ics[usize::try_from(o[2]).unwrap_or(0)];
                *acc = match obj.kind() {
                    ValueKind::Object(ob) => match ic.get(ob) {
                        Some(v) => v,
                        None => {
                            let v = a.get_named(obj, name)?;
                            ic.fill_get(a.mc, ob, name);
                            v
                        }
                    },
                    ValueKind::String(s) if name == "length" => {
                        ic.note_length();
                        f64::from(u32::try_from(s.utf16_len()).unwrap_or(u32::MAX)).into()
                    }
                    ValueKind::String(_) => match ic.string_value(a.context.string_proto) {
                        Some(v) => v,
                        None => {
                            let v = a.get_named(obj, name)?;
                            ic.fill_primitive(a.mc, a.context.string_proto, name);
                            v
                        }
                    },
                    _ => a.get_named(obj, name)?,
                };
            }
            B::SetNamedProperty => {
                let obj = r!(0);
                let name = Self::name_at(loaded, o[1]);
                let slot = usize::try_from(o[2]).unwrap_or(0);
                let s = loaded.func.bytecode.feedback.get(slot) == Some(&j8bytecode::FeedbackSlotKind::SetNamedStrict);
                let ic = &loaded.ics[slot];
                match obj.kind() {
                    ValueKind::Object(ob) => {
                        if !ic.set(a.mc, ob, *acc, true) {
                            let before = ic::Before::from(ob);
                            a.put_named(obj, name, *acc, s)?;
                            ic.fill_set(a.mc, ob, name, before, true);
                        }
                    }
                    _ => a.put_named(obj, name, *acc, s)?,
                }
            }
            B::DefineNamedOwnProperty => {
                let name = Self::name_at(loaded, o[1]);
                if let Some(ob) = r!(0).as_object() {
                    let ic = &loaded.ics[usize::try_from(o[2]).unwrap_or(0)];
                    if !ic.set(a.mc, ob, *acc, false) {
                        let before = ic::Before::from(ob);
                        a.define(ob, name, Property::data(*acc))?;
                        ic.fill_set(a.mc, ob, name, before, false);
                    }
                }
            }
            B::GetKeyedProperty => *acc = a.get_keyed(r!(0), *acc)?,
            B::SetKeyedProperty => {
                let slot = usize::try_from(o[2]).unwrap_or(0);
                let s = loaded.func.bytecode.feedback.get(slot) == Some(&j8bytecode::FeedbackSlotKind::SetKeyedStrict);
                a.put_keyed(r!(0), r!(1), *acc, s)?;
            }
            B::DefineKeyedOwnProperty | B::DefineKeyedOwnPropertyInLiteral | B::StaInArrayLiteral => {
                if d.bytecode != B::StaInArrayLiteral
                    && o[2] & SET_FUNCTION_NAME != 0
                    && let Some(fo) = acc.as_object()
                {
                    a.set_function_name(fo, r!(1), None)?;
                }
                if let Some(ob) = r!(0).as_object() {
                    a.define_keyed(ob, r!(1), Property::data(*acc))?;
                }
            }
            B::DeletePropertyStrict | B::DeletePropertySloppy => {
                let ok = a.delete_keyed(r!(0), *acc)?;
                if d.bytecode == B::DeletePropertyStrict && !ok {
                    return Err(a.throw(ErrorKind::Type, "Cannot delete a non-configurable property"));
                }
                *acc = Value::Bool(ok);
            }
            B::Add => arith!(r!(0), *acc, Add, +),
            B::Sub => arith!(r!(0), *acc, Sub, -),
            B::Mul => arith!(r!(0), *acc, Mul, *),
            B::Div => arith!(r!(0), *acc, Div, /),
            B::AddSmi => arith!(*acc, Value::Number(o[0] as f64), Add, +),
            B::SubSmi => arith!(*acc, Value::Number(o[0] as f64), Sub, -),
            B::MulSmi => arith!(*acc, Value::Number(o[0] as f64), Mul, *),
            B::DivSmi => arith!(*acc, Value::Number(o[0] as f64), Div, /),
            B::Mod | B::Exp | B::BitwiseOr | B::BitwiseXor | B::BitwiseAnd | B::ShiftLeft | B::ShiftRight | B::ShiftRightLogical => {
                *acc = binary(Self::binary_op(d.bytecode), a, r!(0), *acc)?;
            }
            B::ModSmi | B::ExpSmi | B::BitwiseOrSmi | B::BitwiseXorSmi | B::BitwiseAndSmi | B::ShiftLeftSmi | B::ShiftRightSmi | B::ShiftRightLogicalSmi => {
                *acc = binary(Self::binary_op(d.bytecode), a, *acc, Value::Number(o[0] as f64))?;
            }
            B::Inc => *acc = if let Some(x) = acc.as_number() { (x + 1.0).into() } else { a.increment(*acc, 1)? },
            B::Dec => *acc = if let Some(x) = acc.as_number() { (x - 1.0).into() } else { a.increment(*acc, -1)? },
            B::Negate => *acc = a.unary(j8framework::UnaryOp::Neg, *acc)?,
            B::BitwiseNot => *acc = a.unary(j8framework::UnaryOp::BitNot, *acc)?,
            B::TypeOf => *acc = a.unary(j8framework::UnaryOp::Typeof, *acc)?,
            B::LogicalNot | B::ToBooleanLogicalNot => *acc = Value::Bool(!a.to_boolean(*acc)),
            B::ToBoolean => *acc = Value::Bool(a.to_boolean(*acc)),
            B::ToNumber => {
                if !acc.is_number() {
                    *acc = a.to_number(*acc)?.into();
                }
            }
            B::ToNumeric => {
                if !acc.is_number() {
                    *acc = match a.to_numeric(*acc)? {
                        j8framework::Numeric::Number(x) => x.into(),
                        j8framework::Numeric::BigInt(b) => Value::BigInt(b),
                    }
                }
            }
            B::ToName => {
                if !matches!(acc.kind(), ValueKind::Symbol(_)) {
                    *acc = a.property_key(*acc)?.into();
                }
            }
            B::ToString => *acc = a.to_string(*acc)?.into(),
            B::ToObject => {
                let ob = a.to_object(*acc)?;
                Self::set_reg(regs, f, o[0], ob.into());
            }
            B::TestLessThan => relational!(Lt, <),
            B::TestGreaterThan => relational!(Gt, >),
            B::TestLessThanOrEqual => relational!(Le, <=),
            B::TestGreaterThanOrEqual => relational!(Ge, >=),
            B::TestEqual | B::TestEqualStrict => {
                let op = if d.bytecode == B::TestEqual { CompareOp::Eq } else { CompareOp::StrictEq };
                *acc = compare(op, a, r!(0), *acc)?;
            }
            B::TestReferenceEqual => *acc = Value::Bool(r!(0).bits() == acc.bits()),
            B::TestInstanceOf => *acc = Value::Bool(a.instance_of(r!(0), *acc)?),
            B::TestIn => *acc = Value::Bool(a.has_keyed(*acc, r!(0))?),
            B::TestUndetectable => *acc = Value::Bool(acc.is_nullish()),
            B::TestNull => *acc = Value::Bool(matches!(acc.kind(), ValueKind::Null)),
            B::TestUndefined => *acc = Value::Bool(matches!(acc.kind(), ValueKind::Undefined)),
            B::TestTypeOf => {
                let want = ["number", "string", "symbol", "boolean", "bigint", "undefined", "function", "object"];
                let t = a.type_of(*acc);
                *acc = Value::Bool(want.get(usize::try_from(o[0]).unwrap_or(99)).is_some_and(|w| *w == t));
            }
            B::Jump | B::JumpConstant => f.pc = d.target as usize,
            B::JumpIfTrue | B::JumpIfTrueConstant | B::JumpIfFalse | B::JumpIfFalseConstant | B::JumpIfToBooleanTrue | B::JumpIfToBooleanTrueConstant | B::JumpIfToBooleanFalse | B::JumpIfToBooleanFalseConstant | B::JumpIfNull | B::JumpIfNullConstant | B::JumpIfNotNull | B::JumpIfNotNullConstant | B::JumpIfUndefined | B::JumpIfUndefinedConstant | B::JumpIfNotUndefined | B::JumpIfNotUndefinedConstant | B::JumpIfUndefinedOrNull | B::JumpIfUndefinedOrNullConstant | B::JumpIfJSReceiver | B::JumpIfJSReceiverConstant => {
                let take = match d.bytecode {
                    B::JumpIfTrue | B::JumpIfTrueConstant => matches!(acc.kind(), ValueKind::Bool(true)),
                    B::JumpIfFalse | B::JumpIfFalseConstant => matches!(acc.kind(), ValueKind::Bool(false)),
                    B::JumpIfToBooleanTrue | B::JumpIfToBooleanTrueConstant => a.to_boolean(*acc),
                    B::JumpIfToBooleanFalse | B::JumpIfToBooleanFalseConstant => !a.to_boolean(*acc),
                    B::JumpIfNull | B::JumpIfNullConstant => matches!(acc.kind(), ValueKind::Null),
                    B::JumpIfNotNull | B::JumpIfNotNullConstant => !matches!(acc.kind(), ValueKind::Null),
                    B::JumpIfUndefined | B::JumpIfUndefinedConstant => matches!(acc.kind(), ValueKind::Undefined),
                    B::JumpIfNotUndefined | B::JumpIfNotUndefinedConstant => !matches!(acc.kind(), ValueKind::Undefined),
                    B::JumpIfUndefinedOrNull | B::JumpIfUndefinedOrNullConstant => acc.is_nullish(),
                    _ => acc.as_object().is_some(),
                };
                if take {
                    f.pc = d.target as usize;
                }
            }
            B::JumpLoop => {
                f.pc = d.target as usize;
                if let Some(t) = &self.tier {
                    let at = f.insn;
                    let raw = self.raw_frame(a.stop);
                    let r = t.osr(&mut Entry { exe: self, a, st, budget }, at);
                    drop(raw);
                    match r {
                        Tiered::Interpret => {}
                        Tiered::Resumed => return Ok(Flow::Frame),
                        Tiered::Threw(e) => return Err(e),
                    }
                }
                match self.halts(a, budget) {
                    Halt::Pause => return Ok(Flow::Done(Outcome::Paused(Box::new(core::mem::take(st))))),
                    Halt::Collected => return Ok(Flow::Frame),
                    Halt::Go => {}
                }
            }
            B::SwitchOnSmiNoFeedback => {
                if let Some(x) = acc.as_number() {
                    let (table, size, base) = (o[0], o[1], o[2]);
                    #[expect(clippy::cast_possible_truncation, reason = "a case index")]
                    let i = x as i64 - base;
                    if x.fract() == 0.0 && (0..size).contains(&i)
                        && let Some(Constant::Offset(off)) = loaded.func.bytecode.constants.get(usize::try_from(table + i).unwrap_or(usize::MAX))
                    {
                        f.pc = loaded.index_of(d.start as usize + *off as usize);
                    }
                }
            }
            B::CallProperty0 | B::CallProperty1 | B::CallProperty2 | B::CallUndefinedReceiver0 | B::CallUndefinedReceiver1 | B::CallUndefinedReceiver2 => {
                let callee = r!(0);
                let (this, buf, n): (Value<'scope>, [Value<'scope>; 2], usize) = match d.bytecode {
                    B::CallProperty0 => (r!(1), [Value::Undefined; 2], 0),
                    B::CallProperty1 => (r!(1), [r!(2), Value::Undefined], 1),
                    B::CallProperty2 => (r!(1), [r!(2), r!(3)], 2),
                    B::CallUndefinedReceiver0 => (Value::Undefined, [Value::Undefined; 2], 0),
                    B::CallUndefinedReceiver1 => (Value::Undefined, [r!(1), Value::Undefined], 1),
                    _ => (Value::Undefined, [r!(1), r!(2)], 2),
                };
                return self.call_from(a, st, callee, this, Args::Values(&buf[..n]));
            }
            B::CallProperty | B::CallAnyReceiver | B::CallUndefinedReceiver => {
                let callee = r!(0);
                let first = Self::slot_of(f, o[1]).unwrap_or(regs.len());
                let count = usize::try_from(o[2]).unwrap_or(0);
                let (this, args) = match (d.bytecode, count) {
                    (B::CallUndefinedReceiver, _) => (Value::Undefined, Args::Regs { first, count }),
                    (_, 0) => (Value::Undefined, Args::Values(&[])),
                    _ => (regs[first], Args::Regs { first: first + 1, count: count - 1 }),
                };
                return self.call_from(a, st, callee, this, args);
            }
            B::CallWithSpread => {
                let callee = r!(0);
                let mut list = Self::reg_list(regs, f, o[1], o[2]);
                let spread = list.pop().unwrap_or_default();
                let (this, mut args) = list.split_first().map_or((Value::Undefined, Vec::new()), |(t, rest)| (*t, rest.to_vec()));
                args.extend(Self::spread(a, spread)?);
                return self.call_from(a, st, callee, this, Args::Values(&args));
            }
            B::Construct | B::ConstructWithSpread => {
                let callee = r!(0);
                let mut args = Self::reg_list(regs, f, o[1], o[2]);
                if d.bytecode == B::ConstructWithSpread {
                    let spread = args.pop().unwrap_or_default();
                    args.extend(Self::spread(a, spread)?);
                }
                let nt = *acc;
                return self.construct_from(a, st, callee, args, nt);
            }
            B::CallJSRuntime => {
                let list = Self::reg_list(regs, f, o[1], o[2]);
                let arg = |i: usize| list.get(i).copied().unwrap_or_default();
                return match NativeContextIndex::from_u8(u8::try_from(o[0]).unwrap_or(u8::MAX)) {
                    Some(NativeContextIndex::ReflectApply) => {
                        let args = a.list_from_array_like(arg(2))?;
                        self.call_from(a, st, arg(0), arg(1), Args::Values(&args))
                    }
                    Some(NativeContextIndex::ReflectConstruct) => {
                        let args = a.list_from_array_like(arg(1))?;
                        let new_target = if list.len() > 2 { arg(2) } else { arg(0) };
                        self.construct_from(a, st, arg(0), args, new_target)
                    }
                    _ => Err(a.throw(ErrorKind::Type, "unsupported native context function")),
                };
            }
            B::CallRuntime | B::InvokeIntrinsic => {
                let args = Self::reg_list(regs, f, o[1], o[2]);
                st.acc = if d.bytecode == B::InvokeIntrinsic {
                    let i = Intrinsic::from_u8(u8::try_from(o[0]).unwrap_or(u8::MAX)).ok_or_else(|| a.throw(ErrorKind::Type, "unknown intrinsic"))?;
                    if i == Intrinsic::GetImportMetaObject { self.import_meta(a, loaded.base)? } else { self.intrinsic(a, i, &args)? }
                } else {
                    let rt = Runtime::from_u16(u16::try_from(o[0]).unwrap_or(u16::MAX)).ok_or_else(|| a.throw(ErrorKind::Type, "unknown runtime function"))?;
                    self.runtime(a, st, loaded, rt, &args)?
                };
            }
            B::CallRuntimeForPair => {
                let rt = Runtime::from_u16(u16::try_from(o[0]).unwrap_or(u16::MAX)).ok_or_else(|| a.throw(ErrorKind::Type, "unknown runtime function"))?;
                let args = Self::reg_list(regs, f, o[1], o[2]);
                let (first, second) = match rt {
                    Runtime::LoadLookupSlotForCall => {
                        let name = alloc::string::String::from(&**a.to_string(args.first().copied().unwrap_or_default())?);
                        self.lookup_for_call(a, f.env(regs), &name)?
                    }
                    rt => return Err(a.throw(ErrorKind::Type, &alloc::format!("runtime function {rt:?} does not return a pair"))),
                };
                Self::set_reg(regs, f, o[3], first);
                Self::set_reg(regs, f, o[3] + 1, second);
            }
            B::CreateClosure => *acc = self.create_closure(a, loaded, o[0], f.env(regs)).into(),
            B::CreateEmptyObjectLiteral => *acc = a.object().into(),
            B::CreateObjectLiteral => *acc = Self::object_literal(a, loaded, o[0]).into(),
            B::CreateEmptyArrayLiteral => *acc = a.array(Vec::new()).into(),
            B::CreateArrayLiteral => {
                let Some(Constant::Array(b)) = loaded.func.bytecode.constants.get(usize::try_from(o[0]).unwrap_or(0)) else { unreachable!("an array boilerplate") };
                if b.elements.iter().any(|e| matches!(e, Some(LiteralValue::Hole))) {
                    let arr = a.array(Vec::new());
                    for (i, e) in b.elements.iter().enumerate() {
                        if let Some(v) = e.as_ref().filter(|e| **e != LiteralValue::Hole) {
                            let v = Self::literal(a, v);
                            a.define_keyed(arr, Value::Number(i as f64), Property::data(v))?;
                        }
                    }
                    a.put_named(arr.into(), "length", Value::Number(b.elements.len() as f64), true)?;
                    *acc = arr.into();
                } else {
                    let items = b.elements.iter().map(|e| e.as_ref().map_or(Value::Undefined, |v| Self::literal(a, v))).collect();
                    *acc = a.array(items).into();
                }
            }
            B::CreateRegExpLiteral => {
                let (v, _) = Self::regexp_literal(a, loaded, o[0], o[2])?;
                *acc = v;
            }
            B::GetTemplateObject => {
                let key = (f.func.0, u32::try_from(o[0]).unwrap_or(0));
                let cached = a.context.templates.get().get(&key).copied();
                *acc = match cached {
                    Some(t) => t.into(),
                    None => {
                        let Some(Constant::Template(t)) = loaded.func.bytecode.constants.get(usize::try_from(o[0]).unwrap_or(0)) else { unreachable!("a template description") };
                        let cooked: Vec<Value<'scope>> = t.cooked.iter().map(|c| c.as_ref().map_or(Value::Undefined, |s| a.string(s).into())).collect();
                        let raw: Vec<Value<'scope>> = t.raw.iter().map(|s| a.string(s).into()).collect();
                        let cooked = a.array(cooked);
                        let raw = a.array(raw);
                        let frozen = |v: Value<'scope>| Property { slot: Slot::Data(v), enumerable: false, writable: false, configurable: false };
                        cooked.borrow_mut(a.mc).define("raw", frozen(raw.into()));
                        a.context.templates.update(a.mc, |m| {
                            let mut m = m.clone();
                            m.insert(key, cooked);
                            m
                        });
                        cooked.into()
                    }
                };
            }
            B::CloneObject => {
                let ob = a.object();
                a.copy_data(ob, r!(0), &[])?;
                *acc = ob.into();
            }
            B::CreateRestParameter => {
                let formals = usize::from(loaded.func.formal_parameters);
                *acc = a.array(f.args(regs).get(formals..).unwrap_or(&[]).to_vec()).into();
            }
            B::CreateMappedArguments | B::CreateUnmappedArguments => {
                let strict = loaded.func.strict || d.bytecode == B::CreateUnmappedArguments;
                *acc = self.arguments(a, regs, f, strict)?;
            }
            B::GetIterator => *acc = a.iterator_object(r!(0))?,
            B::ForInEnumerate => {
                let keys = a.for_in_keys(r!(0))?;
                *acc = a.array(keys).into();
            }
            B::ForInPrepare => {
                let keys = *acc;
                let len = keys.as_object().map_or(0, |k| k.elements().len());
                let first = o[0];
                Self::set_reg(regs, f, first, keys);
                Self::set_reg(regs, f, first + 1, keys);
                Self::set_reg(regs, f, first + 2, Value::Number(len as f64));
            }
            B::JumpIfForInDone | B::JumpIfForInDoneConstant => {
                let (i, n) = (r!(1).as_number().unwrap_or(0.0), r!(2).as_number().unwrap_or(0.0));
                if i >= n {
                    f.pc = d.target as usize;
                }
            }
            B::ForInNext => {
                let receiver = r!(0);
                #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "an index")]
                let i = r!(1).as_number().unwrap_or(0.0) as usize;
                let keys = Self::reg(regs, f, o[2] + 1);
                let key = keys.as_object().and_then(|k| k.elements().get(i)).unwrap_or_default();
                let still = a.has_keyed(receiver, key)?;
                *acc = if still { key } else { Value::Undefined };
            }
            B::ForInStep => {
                let i = r!(0).as_number().unwrap_or(0.0);
                Self::set_reg(regs, f, o[0], Value::Number(i + 1.0));
            }
            B::SetPendingMessage => core::mem::swap(acc, message),
            B::Throw => return Err(a.throw_value(*acc)),
            B::ReThrow => return Err(a.rethrow(*acc)),
            B::ThrowReferenceErrorIfHole => {
                if matches!(acc.kind(), ValueKind::Uninit) {
                    return Err(Self::uninitialized(a, Self::name_at(loaded, o[0])));
                }
            }
            B::ThrowSuperAlreadyCalledIfNotHole => {
                if !matches!(acc.kind(), ValueKind::Uninit) {
                    return Err(a.throw(ErrorKind::Reference, "Super constructor may only be called once"));
                }
            }
            B::ThrowIfNotSuperConstructor => {
                let c = r!(0);
                if !a.is_constructor(c) {
                    let what = a.describe(c);
                    return Err(a.throw(ErrorKind::Type, &alloc::format!("Super constructor {what} of anonymous class is not a constructor")));
                }
            }
            B::GetSuperConstructor => {
                let parent = match acc.as_object() {
                    Some(fo) => a.get_prototype_of(fo)?.map_or(Value::Null, Value::Object),
                    None => Value::Null,
                };
                Self::set_reg(regs, f, o[0], parent);
            }
            B::FindNonDefaultConstructorOrConstruct => {
                let parent = match r!(0).as_object() {
                    Some(fo) => a.get_prototype_of(fo)?.map_or(Value::Null, Value::Object),
                    None => Value::Null,
                };
                Self::set_reg(regs, f, o[2], Value::Bool(false));
                Self::set_reg(regs, f, o[2] + 1, parent);
            }
            B::ConstructForwardAllArgs => {
                let callee = r!(0);
                let args = f.args(regs);
                let nt = *acc;
                return self.construct_from(a, st, callee, args, nt);
            }
            B::GetNamedPropertyFromSuper => {
                let receiver = r!(0);
                let name = a.string(Self::name_at(loaded, o[1])).into();
                *acc = a.super_get(*acc, name, receiver)?;
            }
            B::ThrowSuperNotCalledIfHole => {
                if matches!(acc.kind(), ValueKind::Uninit) {
                    return Err(a.throw(ErrorKind::Reference, "Must call super constructor in derived class before accessing 'this' or returning from derived constructor"));
                }
            }
            B::SwitchOnGeneratorState => {
                if let Some(co) = r!(0).as_object() {
                    let (continuation, context) = Self::coroutine(co, |c| (c.frame.continuation, c.frame.context));
                    f.set_env(regs, context);
                    if let Some(Constant::Offset(off)) = loaded.func.bytecode.constants.get(usize::try_from(o[1] + i64::from(continuation)).unwrap_or(usize::MAX)) {
                        f.pc = loaded.index_of(d.start as usize + *off as usize);
                    }
                }
            }
            B::SuspendGenerator => {
                let Some(co) = r!(0).as_object() else { unreachable!("a generator object") };
                let first = Self::slot_of(f, o[1]).unwrap_or(regs.len());
                let count = usize::try_from(o[2]).unwrap_or(0);
                let saved = regs[first..first + count].to_vec();
                let params = regs[f.params..f.base - Frame::HEADER].to_vec();
                let context = f.env(regs);
                let continuation = u32::try_from(o[3]).unwrap_or(0);
                Self::coroutine_mut(a, co, |c| {
                    c.frame.params = params;
                    c.frame.regs = saved;
                    c.frame.context = context;
                    c.frame.continuation = continuation;
                    c.state = j8framework::CoroutineState::Suspended;
                });
                let v = *acc;
                return Ok(st.returned(v));
            }
            B::ResumeGenerator => {
                let Some(co) = r!(0).as_object() else { unreachable!("a generator object") };
                let first = Self::slot_of(f, o[1]).unwrap_or(regs.len());
                let (input, saved) = Self::coroutine(co, |c| (c.input, c.frame.regs.clone()));
                regs[first..first + saved.len()].copy_from_slice(&saved);
                *acc = input;
            }
            B::Return => {
                let v = self.return_value(a, regs, f, *acc);
                let flow = st.returned(v.as_ref().copied().unwrap_or_default());
                return v.map(|_| flow);
            }
            B::Debugger => {}
            b => return Err(a.throw(ErrorKind::Syntax, &alloc::format!("bytecode {b:?} is not supported by the executor yet"))),
        }
        Ok(Flow::Next)
    }

    fn binary_op(b: Bytecode) -> BinaryOp {
        use Bytecode as B;
        match b {
            B::Add | B::AddSmi => BinaryOp::Add,
            B::Sub | B::SubSmi => BinaryOp::Sub,
            B::Mul | B::MulSmi => BinaryOp::Mul,
            B::Div | B::DivSmi => BinaryOp::Div,
            B::Mod | B::ModSmi => BinaryOp::Rem,
            B::Exp | B::ExpSmi => BinaryOp::Exp,
            B::BitwiseOr | B::BitwiseOrSmi => BinaryOp::BitOr,
            B::BitwiseXor | B::BitwiseXorSmi => BinaryOp::BitXor,
            B::BitwiseAnd | B::BitwiseAndSmi => BinaryOp::BitAnd,
            B::ShiftLeft | B::ShiftLeftSmi => BinaryOp::Shl,
            B::ShiftRight | B::ShiftRightSmi => BinaryOp::Shr,
            _ => BinaryOp::UShr,
        }
    }

    fn spread<'scope>(a: &mut Scope<'scope, '_>, v: Value<'scope>) -> Result<Vec<Value<'scope>>, Error> {
        a.to_list(v)
    }

    fn arguments<'scope>(&self, a: &mut Scope<'scope, '_>, regs: &[Value<'scope>], f: &Frame, strict: bool) -> Result<Value<'scope>, Error> {
        let args = f.args(regs);
        let outer = f.slot(regs, Frame::CLOSURE).as_object().and_then(|o| o.closure()).and_then(|c| c.env);
        let map = if strict { None } else { self.arguments_map(f.env(regs), outer, args.len()) };
        let o = ObjectData { kind: ObjectKind::Arguments(map), proto: Some(a.context.object_proto), ..ObjectData::default() }.alloc(a.mc);
        let hidden = |v: Value<'scope>| Property { slot: Slot::Data(v), enumerable: false, writable: true, configurable: true };
        for (i, &v) in args.iter().enumerate() {
            let k = alloc::string::String::from(ryu_js::Buffer::new().format(f64::from(u32::try_from(i).unwrap_or(u32::MAX))));
            o.borrow_mut(a.mc).define(&k, Property::data(v));
        }
        let len = f64::from(u32::try_from(args.len()).unwrap_or(u32::MAX));
        let values = a.get_symbol(Value::Object(a.context.array_proto), a.context.symbols.iterator, Value::Undefined)?;
        let mut dd = o.borrow_mut(a.mc);
        dd.define("length", hidden(len.into()));
        if strict {
            let thrower = a.context.named_native(a.mc, Self::throw_type_error, "", 0);
            dd.define("callee", Property { slot: Slot::Accessor { get: Some(thrower), set: Some(thrower) }, enumerable: false, writable: false, configurable: false });
        } else {
            dd.define("callee", hidden(f.slot(regs, Frame::CLOSURE)));
        }
        dd.define_symbol(a.context.symbols.iterator, hidden(values));
        drop(dd);
        Ok(o.into())
    }

    fn arguments_map<'scope>(&self, env: Option<JsEnv<'scope>>, outer: Option<JsEnv<'scope>>, count: usize) -> Option<j8framework::ArgumentsMap<'scope>> {
        let env = env.filter(|e| outer.is_none_or(|o| !ScopeLocal::ptr_eq(*e, o)))?;
        let shape = self.context_shape(env.shape?);
        if shape.kind != ContextKind::Function {
            return None;
        }
        let mut slots = alloc::vec![None; count];
        for (k, n) in shape.names.iter().enumerate() {
            if let Some(p) = n.parameter
                && let Some(s) = slots.get_mut(usize::from(p))
            {
                *s = u16::try_from(k).ok();
            }
        }
        slots.iter().any(Option::is_some).then_some(j8framework::ArgumentsMap { env, slots })
    }

    fn throw_type_error<'scope>(a: &mut Scope<'scope, '_>, _: Value<'scope>, _: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        Err(a.throw(ErrorKind::Type, "'caller', 'callee', and 'arguments' properties may not be accessed on strict mode functions or the arguments objects for calls to them"))
    }

    fn define_class<'scope>(a: &mut Scope<'scope, '_>, b: &j8bytecode::ClassBoilerplate, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        use j8bytecode::MemberKind as M;
        let arg = |n: usize| args.get(n).copied().unwrap_or_default();
        let Some(ctor) = arg(1).as_object() else { unreachable!("a class constructor") };
        let (proto_parent, ctor_parent) = a.class_heritage(arg(2))?;
        ctor.borrow_mut(a.mc).set_proto(Some(ctor_parent));
        let proto = ObjectData { proto: proto_parent, ..ObjectData::default() }.alloc(a.mc);
        ctor.borrow_mut(a.mc).define("prototype", Property { slot: Slot::Data(proto.into()), enumerable: false, writable: false, configurable: false });
        proto.borrow_mut(a.mc).define("constructor", Property::hidden(ctor.into()));
        let mut next = 3;
        for m in &b.members {
            let key = match &m.name {
                Some(n) => Value::from(a.string(n)),
                None => {
                    next += 1;
                    arg(next - 1)
                }
            };
            if m.kind == M::Field {
                continue;
            }
            let Some(fo) = arg(next).as_object() else { unreachable!("a member function") };
            next += 1;
            let target = if m.statik { ctor } else { proto };
            let prefix = match m.kind {
                M::Getter => Some("get"),
                M::Setter => Some("set"),
                M::Method | M::Field => None,
            };
            if m.name.is_none() {
                a.set_function_name(fo, key, prefix)?;
            }
            match m.kind {
                M::Getter | M::Setter => a.define_accessor_keyed(target, key, fo, m.kind == M::Setter, false)?,
                M::Method | M::Field => a.define_keyed(target, key, Property::hidden(fo.into()))?,
            }
        }
        Ok(proto.into())
    }

    fn coroutine<'scope, T>(co: JsObject<'scope>, read: impl FnOnce(&j8framework::Coroutine<'scope>) -> T) -> T {
        match &co.borrow().kind {
            ObjectKind::Coroutine(c) => read(c),
            _ => unreachable!("a generator object"),
        }
    }

    fn coroutine_mut<'scope, T>(a: &Scope<'scope, '_>, co: JsObject<'scope>, write: impl FnOnce(&mut j8framework::Coroutine<'scope>) -> T) -> T {
        match &mut co.borrow_mut(a.mc).kind {
            ObjectKind::Coroutine(c) => write(c),
            _ => unreachable!("a generator object"),
        }
    }

    fn intrinsic<'scope>(&self, a: &mut Scope<'scope, '_>, i: Intrinsic, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let arg = |n: usize| args.get(n).copied().unwrap_or_default();
        match i {
            Intrinsic::CopyDataProperties => {
                if let Some(t) = arg(0).as_object() {
                    a.copy_data(t, arg(1), &[])?;
                }
                Ok(arg(0))
            }
            Intrinsic::CopyDataPropertiesWithExcludedPropertiesOnStack => {
                let src = arg(0);
                if src.is_nullish() {
                    return Err(a.throw(ErrorKind::Type, "Cannot destructure 'undefined' or 'null'"));
                }
                let exclude = args.iter().skip(1).filter(|k| !matches!(k.kind(), ValueKind::Symbol(_))).map(|&k| a.property_key(k)).collect::<Result<Vec<_>, _>>()?;
                let ob = a.object();
                a.copy_data(ob, src, &exclude)?;
                Ok(ob.into())
            }
            Intrinsic::CreateJSGeneratorObject => {
                let closure = arg(0);
                let asynchronous = closure.as_object().is_some_and(|o| matches!(&o.borrow().kind, ObjectKind::Function(c) if self.function(c.func).asynchronous));
                let proto = match a.get_named(closure, "prototype")?.as_object() {
                    Some(p) => p,
                    None if asynchronous => a.context.async_generator_proto,
                    None => a.context.generator_proto,
                };
                let co = j8framework::Coroutine::new(closure, None, asynchronous);
                Ok(ObjectData { kind: ObjectKind::Coroutine(Box::new(co)), proto: Some(proto), ..ObjectData::default() }.alloc(a.mc).into())
            }
            Intrinsic::AsyncFunctionEnter => {
                let promise = a.new_promise();
                let co = j8framework::Coroutine::new(arg(0), Some(promise), false);
                Ok(ObjectData { kind: ObjectKind::Coroutine(Box::new(co)), proto: Some(a.context.object_proto), ..ObjectData::default() }.alloc(a.mc).into())
            }
            Intrinsic::AsyncFunctionAwait => {
                let Some(co) = arg(0).as_object() else { unreachable!("a generator object") };
                let p = a.promise_resolve(arg(1))?;
                a.perform_then(p, j8framework::Reaction::Await { coroutine: co });
                Ok(Self::coroutine(co, |c| c.promise).map_or(Value::Undefined, Value::Object))
            }
            Intrinsic::AsyncFunctionResolve | Intrinsic::AsyncFunctionReject => {
                let Some(co) = arg(0).as_object() else { unreachable!("a generator object") };
                let Some(promise) = Self::coroutine(co, |c| c.promise) else { unreachable!("an async function's promise") };
                if i == Intrinsic::AsyncFunctionResolve {
                    a.resolve_promise(promise, arg(1))?;
                } else {
                    a.reject_promise(promise, arg(1));
                }
                Ok(promise.into())
            }
            Intrinsic::CreateAsyncFromSyncIterator => {
                let iterator = arg(0);
                let next = a.get_named(iterator, "next")?;
                let rec = j8framework::IterRecord { iterator, next };
                Ok(ObjectData { kind: ObjectKind::AsyncFromSyncIterator(rec), proto: Some(a.context.async_from_sync_iterator_proto), ..ObjectData::default() }.alloc(a.mc).into())
            }
            Intrinsic::GeneratorGetResumeMode => {
                let Some(co) = arg(0).as_object() else { unreachable!("a generator object") };
                Ok(Value::Number(Self::coroutine(co, |c| if matches!(c.waiting, j8framework::Waiting::Rethrow) { RESUME_RETHROW } else { f64::from(c.mode) })))
            }
            Intrinsic::AsyncGeneratorAwait | Intrinsic::AsyncGeneratorYieldWithAwait => {
                let Some(co) = arg(0).as_object() else { unreachable!("a generator object") };
                let v = arg(1);
                let waiting = if i == Intrinsic::AsyncGeneratorAwait { j8framework::Waiting::Await(v) } else { j8framework::Waiting::Yield(v) };
                Self::coroutine_mut(a, co, |c| c.waiting = waiting);
                Ok(Value::Undefined)
            }
            Intrinsic::AsyncGeneratorResolve | Intrinsic::AsyncGeneratorReject => {
                let Some(co) = arg(0).as_object() else { unreachable!("a generator object") };
                let v = arg(1);
                let settlement = if i == Intrinsic::AsyncGeneratorResolve { j8framework::Settlement::Resolved(v) } else { j8framework::Settlement::Rejected(v) };
                Self::coroutine_mut(a, co, |c| {
                    if !matches!(c.settlement, j8framework::Settlement::Rejected(_)) {
                        c.settlement = settlement;
                    }
                });
                Ok(Value::Undefined)
            }
            Intrinsic::GeneratorYieldResult => Ok(a.iter_result(arg(0), false).into()),
            Intrinsic::GeneratorClose => {
                if let Some(co) = arg(0).as_object() {
                    Self::coroutine_mut(a, co, |c| c.state = j8framework::CoroutineState::Completed);
                }
                Ok(Value::Undefined)
            }
            other => Err(a.throw(ErrorKind::Syntax, &alloc::format!("intrinsic {other:?} is not supported yet"))),
        }
    }

    #[expect(clippy::too_many_lines, reason = "one arm per runtime function")]
    fn runtime<'scope>(&self, a: &mut Scope<'scope, '_>, st: &mut State<'scope>, loaded: &Loaded, rt: Runtime, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let arg = |n: usize| args.get(n).copied().unwrap_or_default();
        let text = |a: &mut Scope<'scope, '_>, v: Value<'scope>| -> Result<alloc::string::String, Error> { Ok(alloc::string::String::from(&**a.to_string(v)?)) };
        match rt {
            Runtime::DeclareGlobals => {
                let decls = loaded.func.bytecode.constants.iter().find_map(|c| match c {
                    Constant::Declarations(d) => Some(d.clone()),
                    _ => None,
                }).unwrap_or_default();
                self.declare_globals(a, st.f(), st.f().env(&st.regs), &decls)?;
                Ok(Value::Undefined)
            }
            Runtime::DeclareEvalVar => {
                let name = text(a, arg(0))?;
                #[expect(clippy::cast_possible_truncation, reason = "a small integer the compiler wrote")]
                let check = args.get(1).and_then(|v| v.as_number()).and_then(|n| j8bytecode::EvalDeclaration::try_from(n as i32).ok());
                match check {
                    Some(kind) => self.check_eval(a, st.f().env(&st.regs), &name, kind)?,
                    None => self.declare_eval(a, st.f().env(&st.regs), &name, None)?,
                }
                Ok(Value::Undefined)
            }
            Runtime::DeclareEvalFunction => {
                let name = text(a, arg(0))?;
                self.declare_eval(a, st.f().env(&st.regs), &name, Some(arg(1)))?;
                Ok(Value::Undefined)
            }
            Runtime::ThrowConstAssignError => Err(a.throw(ErrorKind::Type, "Assignment to constant variable.")),
            Runtime::DefineClass => {
                #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "a constant pool index")]
                let k = arg(0).as_number().map_or(usize::MAX, |k| k as usize);
                let Some(Constant::Class(b)) = loaded.func.bytecode.constants.get(k) else { unreachable!("a class boilerplate") };
                Self::define_class(a, b, args)
            }
            Runtime::CreatePrivateNameSymbol | Runtime::CreatePrivateBrandSymbol => {
                let desc = text(a, arg(0))?;
                Ok(Value::Symbol(a.private_name(&desc)))
            }
            Runtime::CreatePrivateAccessors => Ok(ObjectData { kind: ObjectKind::AccessorPair { get: arg(0).as_object(), set: arg(1).as_object() }, ..ObjectData::default() }.alloc(a.mc).into()),
            Runtime::LoadPrivateGetter | Runtime::LoadPrivateSetter => Ok(match arg(0).as_object().map(|o| match o.borrow().kind {
                ObjectKind::AccessorPair { get, set } => if rt == Runtime::LoadPrivateGetter { get } else { set },
                _ => None,
            }) {
                Some(Some(fo)) => fo.into(),
                _ => Value::Undefined,
            }),
            Runtime::LoadFromSuper | Runtime::LoadKeyedFromSuper => a.super_get(arg(1), arg(2), arg(0)),
            Runtime::StoreToSuper | Runtime::StoreKeyedToSuper => {
                a.super_set(arg(1), arg(2), arg(3), arg(0))?;
                Ok(arg(3))
            }
            Runtime::ThrowUnsupportedSuperError => Err(a.throw(ErrorKind::Reference, "Unsupported reference to 'super'")),
            Runtime::ThrowStaticPrototypeError => Err(a.throw(ErrorKind::Type, "Classes may not have a static property named 'prototype'")),
            Runtime::ThrowTypeError => {
                let m = text(a, arg(0))?;
                Err(a.throw(ErrorKind::Type, &m))
            }
            Runtime::ThrowSymbolAsyncIteratorInvalid => Err(a.throw(ErrorKind::Type, "Result of the Symbol.asyncIterator method is not an object")),
            Runtime::ThrowThrowMethodMissing => Err(a.throw(ErrorKind::Type, "The iterator does not provide a 'throw' method")),
            Runtime::ThrowIteratorResultNotAnObject => {
                let what = a.describe(arg(0));
                Err(a.throw(ErrorKind::Type, &alloc::format!("Iterator result {what} is not an object")))
            }
            Runtime::ThrowPatternAssignmentNonCoercible => {
                let what = a.describe(arg(0));
                Err(a.throw(ErrorKind::Type, &alloc::format!("Cannot destructure '{what}' as it is undefined or null.")))
            }
            Runtime::ResolvePossiblyDirectEval => {
                let (callee, source) = (arg(0), arg(1));
                let real = matches!(callee.kind(), ValueKind::Object(o) if ScopeLocal::ptr_eq(o, a.context.eval));
                let ValueKind::String(s) = source.kind() else { return Ok(callee) };
                if !real {
                    return Ok(callee);
                }
                let strict = arg(3).as_number().is_some_and(|x| x != 0.0);
                let src = s.source_text();
                let module = match Module::compile_direct(&j8framework::Source { text: &src, file: EVAL_FILE }, strict, s.lone_surrogates(), self.debug_info()) {
                    Ok(m) => m,
                    Err(Error::Syntax(m)) => return Err(a.throw(ErrorKind::Syntax, &m)),
                    Err(e) => return Err(e),
                };
                let main = self.load(a.mc, module);
                let closure = Closure::new(main, st.f().env(&st.regs));
                Ok(self.function_object(a, closure).into())
            }
            Runtime::DynamicImportCall => {
                let referrer = self.referrer(a, loaded.base);
                self.dynamic_import(a, &referrer, arg(1))
            }
            Runtime::DeleteLookupSlot => {
                let name = text(a, arg(0))?;
                Ok(Value::Bool(match self.lookup(a, st.f().env(&st.regs), &name)? {
                    Found::Slot(..) => false,
                    Found::Object(o) | Found::With(o) => a.delete(Value::Object(o), &name)?,
                    Found::Nothing => true,
                }))
            }
            Runtime::DefineAccessorPropertyUnchecked => {
                if let Some(o) = arg(0).as_object() {
                    if let Some(g) = arg(2).as_object() {
                        a.define_accessor_keyed(o, arg(1), g, false, true)?;
                    }
                    if let Some(s) = arg(3).as_object() {
                        a.define_accessor_keyed(o, arg(1), s, true, true)?;
                    }
                }
                Ok(Value::Undefined)
            }
            Runtime::InternalSetPrototype => {
                if let Some(o) = arg(0).as_object() {
                    match arg(1).kind() {
                        ValueKind::Object(p) => o.set_proto(a.mc, Some(p)),
                        ValueKind::Null => o.set_proto(a.mc, None),
                        _ => {}
                    }
                }
                Ok(arg(0))
            }
            other => Err(a.throw(ErrorKind::Syntax, &alloc::format!("runtime function {other:?} is not supported yet"))),
        }
    }

    fn state<'scope>(&self, regs: Vec<Value<'scope>>, f: Frame) -> State<'scope> {
        let frames = self.spare_frames.borrow_mut().pop().unwrap_or_else(|| Vec::with_capacity(FRAMES_RESERVED));
        State::new(regs, frames, f)
    }

    fn recycle<'scope>(&self, a: &mut Scope<'scope, '_>, st: State<'scope>) {
        let State { regs, mut frames, .. } = st;
        a.give_regs(regs);
        frames.clear();
        let mut spare = self.spare_frames.borrow_mut();
        if spare.len() < SPARE_FRAMES {
            spare.push(frames);
        }
    }

    fn resume_open<'scope>(&self, a: &mut Scope<'scope, '_>, co: JsObject<'scope>, mode: Resume, value: Value<'scope>) -> Result<Resumed<'scope>, Error> {
        use j8framework::{CoroutineState as S, Settlement, Waiting as W};
        let level = self.deeper(a)?;
        a.check_stack()?;
        let (state, waiting, async_generator) = Self::coroutine_mut(a, co, |c| (c.state, core::mem::replace(&mut c.waiting, W::Nothing), c.requests.is_some()));
        let (mut mode, mut rethrow) = (mode, false);
        match waiting {
            W::Yield(_) if mode == Resume::Next => return Ok(Resumed::Step(Step::Yield(value))),
            W::Yield(_) => rethrow = true,
            W::Return => mode = if mode == Resume::Next { Resume::Return } else { Resume::Throw },
            W::Nothing | W::Await(_) | W::Rethrow => {
                if async_generator && mode == Resume::Return && state != S::Running {
                    Self::coroutine_mut(a, co, |c| c.waiting = W::Return);
                    return Ok(Resumed::Step(Step::Await(value)));
                }
            }
        }
        if state == S::Suspended {
            Self::coroutine_mut(a, co, |c| {
                c.state = S::Running;
                c.mode = mode;
                c.input = value;
                c.settlement = Settlement::Nothing;
                if rethrow {
                    c.waiting = W::Rethrow;
                }
            });
        }
        match (state, mode) {
            (S::Suspended, _) => {}
            (S::Running, _) => return Err(a.throw(ErrorKind::Type, "Generator is already running")),
            (S::Completed, Resume::Next) => return Ok(Resumed::Step(Step::Done(Value::Undefined))),
            (S::Completed, Resume::Return) => return Ok(Resumed::Step(Step::Done(value))),
            (S::Completed, Resume::Throw) => return Err(a.throw_value(value)),
        }
        let (callee, params) = Self::coroutine(co, |c| (c.frame.closure, c.frame.params.clone()));
        let Some(closure) = callee.as_object().and_then(|o| match &o.borrow().kind {
            ObjectKind::Function(c) => Some(*c),
            _ => None,
        }) else {
            unreachable!("a generator's function")
        };
        let mut regs = a.take_regs();
        regs.clear();
        let (this, args) = params.split_first().map_or((Value::Undefined, &[][..]), |(t, rest)| (*t, rest));
        self.note_user();
        let f = Self::open(a, &mut regs, self.function(closure.func), closure, callee, this, Args::Values(args), Opening::Resume(co));
        Ok(Resumed::Frame(regs, f, level))
    }

    fn resume_close<'scope>(&self, a: &mut Scope<'scope, '_>, co: JsObject<'scope>, r: Result<Value<'scope>, Error>) -> Result<Step<'scope>, Error> {
        use j8framework::{CoroutineState as S, Settlement, Waiting as W};
        let v = match r {
            Ok(v) => v,
            Err(e) => {
                Self::coroutine_mut(a, co, |c| c.state = S::Completed);
                return Err(e);
            }
        };
        let (suspended, waiting, settlement) = Self::coroutine_mut(a, co, |c| {
            let suspended = c.state == S::Suspended;
            if !suspended {
                c.state = S::Completed;
            }
            (suspended, c.waiting, core::mem::replace(&mut c.settlement, Settlement::Nothing))
        });
        match (suspended, waiting, settlement) {
            (true, W::Await(w) | W::Yield(w), _) => Ok(Step::Await(w)),
            (true, _, Settlement::Resolved(x)) => Ok(Step::Yield(x)),
            (true, ..) => Ok(Step::Yield(v)),
            (false, _, Settlement::Rejected(e)) => Err(a.throw_value(e)),
            (false, _, Settlement::Resolved(x)) => Ok(Step::Done(x)),
            (false, ..) => Ok(Step::Done(v)),
        }
    }

    fn frames<'s, 'scope: 's>(&'s self) -> impl Iterator<Item = (&'s State<'scope>, &'s Frame)> + 's {
        let mut at = self.active.0.get();
        let states = core::iter::from_fn(move || {
            let running = unsafe { at?.as_ref() };
            at = running.outer;
            Some(unsafe { running.state.cast::<State<'scope>>().as_ref() })
        });
        states.flat_map(|st| st.frames.iter().rev().map(move |f| (st, f)))
    }

    fn run_frame<'scope>(&self, a: &mut Scope<'scope, '_>, regs: Vec<Value<'scope>>, f: Frame, level: usize) -> Result<Value<'scope>, Error> {
        self.deep(level, || self.execute(a, self.state(regs, f), &mut None).map(Outcome::returned))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mutability {
    Mutable,
    Const,
    FunctionName,
}

impl From<&ContextName> for Mutability {
    fn from(n: &ContextName) -> Self {
        match (n.function_name, n.constant) {
            (true, _) => Mutability::FunctionName,
            (false, true) => Mutability::Const,
            (false, false) => Mutability::Mutable,
        }
    }
}

enum Found<'scope> {
    Slot(JsEnv<'scope>, usize, Mutability),
    Object(JsObject<'scope>),
    With(JsObject<'scope>),
    Nothing,
}

impl<'scope> Invoke<'scope> for Executor<'_> {
    fn call(&self, a: &mut Scope<'scope, '_>, callee: Value<'scope>, this: Value<'scope>, args: &[Value<'scope>]) -> Result<Value<'scope>, Error> {
        let safe = a.gc_safe.replace(false);
        let level = self.deeper(a)?;
        a.check_stack()?;
        let (closure, callee, this, args) = match self.resolve(a, &[], callee, this, Args::Values(args), level)? {
            Callee::Done(v) => return Ok(v),
            Callee::Closure { closure, callee, this, args } => (closure, callee, this, args),
        };
        let l = self.code(&closure);
        if !safe
            && l.tier.get() != 0
            && let Some(t) = &self.tier
            && let Some(r) = self.deep(level, || {
                let _raw = self.raw_frame(a.stop);
                t.call(self, a, closure.func, callee, this, args.slice(&[]))
            })
        {
            return r;
        }
        self.note_user();
        let mut regs = a.take_regs();
        regs.clear();
        let f = Self::open(a, &mut regs, l, closure, callee, this, args.args(), Opening::Call);
        self.safe_next.set(safe);
        self.run_frame(a, regs, f, level)
    }

    fn callback(&self, a: &mut Scope<'scope, '_>, callee: Value<'scope>, this: Value<'scope>) -> j8framework::Callback<'scope> {
        if let Some(t) = &self.tier
            && let Some(c) = callee.as_object().and_then(|o| o.closure())
            && let f = self.jit_closure_function(&c)
            && !f.class_constructor()
            && let Some(cb) = t.callback(self, a, f, callee, this)
        {
            return cb;
        }
        j8framework::Callback::from((callee, this))
    }

    fn capture(&self, _: &Scope<'scope, '_>, limit: usize) -> Vec<j8framework::Captured> {
        let captured = |(st, f): (&State<'_>, &Frame)| {
            let loaded = unsafe { f.loaded.get() };
            j8framework::Captured { code: f.loaded.0, at: loaded.insns.get(f.insn).map_or(0, |d| d.start), constructor: f.constructing(&st.regs) }
        };
        self.frames().take(limit).map(captured).collect()
    }

    fn call_site(&self, a: &Scope<'scope, '_>, frame: &j8framework::Captured) -> j8framework::CallSite {
        let loaded: &Loaded = unsafe { frame.code.get() };
        let (line, column) = loaded.func.bytecode.position_at(frame.at).map_or((0, 0), |p| (p.line, p.column));
        j8framework::CallSite { function: loaded.func.name.clone(), script: self.referrer(a, loaded.base).into(), line, column, constructor: frame.constructor }
    }

    fn constructs(&self, closure: &Closure<'scope>) -> bool {
        let loaded: &Loaded = unsafe { closure.code.expect("closures carry their code").get() };
        loaded.constructs || loaded.class_constructor
    }

    fn eval_code(&self, a: &mut Scope<'scope, '_>, src: &str, kind: j8framework::CodeKind, hidden: bool) -> Result<Value<'scope>, Error> {
        let file = Executor::source_url(src).unwrap_or(EVAL_FILE);
        let module = match Module::compile_eval(&j8framework::Source { text: src, file }, kind, hidden, self.debug_info()) {
            Ok(m) => m,
            Err(Error::Syntax(m)) => return Err(a.throw(ErrorKind::Syntax, &m)),
            Err(e) => return Err(e),
        };
        let main = self.load_script(a.mc, module, file);
        let level = self.deeper(a)?;
        self.note_user();
        self.deep(level, || self.run_script(a, main))
    }

    fn resume(&self, a: &mut Scope<'scope, '_>, co: JsObject<'scope>, mode: Resume, value: Value<'scope>) -> Result<Step<'scope>, Error> {
        match self.resume_open(a, co, mode, value)? {
            Resumed::Step(step) => Ok(step),
            Resumed::Frame(regs, f, level) => {
                let r = self.run_frame(a, regs, f, level);
                self.resume_close(a, co, r)
            }
        }
    }

    fn construct(&self, a: &mut Scope<'scope, '_>, callee: Value<'scope>, args: &[Value<'scope>], new_target: Value<'scope>) -> Result<Value<'scope>, Error> {
        let level = self.deeper(a)?;
        a.check_stack()?;
        let mut regs = Vec::new();
        match self.prepare(a, &mut regs, callee, args.to_vec(), new_target, level)? {
            Prepared::Done(v) => Ok(v),
            Prepared::Frame(f) => {
                self.note_user();
                self.run_frame(a, regs, f, level)
            }
        }
    }
}

#[derive(Collect)]
#[collect(no_drop)]
enum After<'scope> {
    Script,
    Reaction(Option<j8framework::Capability<'scope>>),
    Await(JsObject<'scope>),
}

#[derive(Clone, Copy)]
enum Held {
    Script,
    Reaction(bool),
    Await,
}

impl<'scope> After<'scope> {
    fn hold(self, a: &mut Scope<'scope, '_>) -> Held {
        match self {
            After::Script => Held::Script,
            After::Reaction(None) => Held::Reaction(false),
            After::Reaction(Some(c)) => {
                a.handles.extend([c.promise, c.resolve, c.reject]);
                Held::Reaction(true)
            }
            After::Await(co) => {
                a.handles.push(co.into());
                Held::Await
            }
        }
    }
}

impl Held {
    fn restore<'scope>(self, a: &mut Scope<'scope, '_>, mark: usize) -> After<'scope> {
        let held = a.handles.split_off(mark);
        match (self, held.as_slice()) {
            (Held::Reaction(true), &[promise, resolve, reject]) => After::Reaction(Some(j8framework::Capability { promise, resolve, reject })),
            (Held::Await, &[co]) => After::Await(co.as_object().expect("a held coroutine")),
            (Held::Reaction(false), &[]) => After::Reaction(None),
            (Held::Script, &[]) => After::Script,
            _ => unreachable!("the held values match what was held"),
        }
    }
}

#[derive(Collect)]
#[collect(no_drop)]
struct Execution<'scope> {
    state: State<'scope>,
    after: After<'scope>,
}

j8framework::paragc::brand!(
    ParkedBrand = for<'heap, 'scope> j8framework::GcCell<'scope, Option<Execution<'scope>>>
);

type Parked<'p> = j8framework::ScopeGlobal<'p, ParkedBrand>;

enum Work {
    Script { module: Module, file: alloc::string::String },
    Module { text: alloc::string::String, file: alloc::string::String },
    Drain,
    Spent,
}

enum Next<'scope> {
    Run(Execution<'scope>),
    Paused,
    Done,
}

pub struct Job<'p> {
    work: Work,
    parked: Option<Parked<'p>>,
    value: Option<j8framework::ScopeGlobal<'p, j8framework::ValueBrand>>,
    entry: Option<j8framework::ScopeGlobal<'p, j8framework::ValueBrand>>,
}

impl From<Work> for Job<'_> {
    fn from(work: Work) -> Self {
        Self { work, parked: None, value: None, entry: None }
    }
}

impl<'p> Executor<'p> {
    pub const SLICE: usize = 10_000;

    pub fn start(&self, src: &j8framework::Source<'_>, kind: j8framework::ScriptKind) -> Result<Job<'p>, Error> {
        Ok(Job::from(match kind {
            j8framework::ScriptKind::Classic => Work::Script { module: Module::compile(src, j8framework::CodeKind::Script, self.debug_info())?, file: src.file.into() },
            j8framework::ScriptKind::Shared => Work::Script {
                module: match &self.cache {
                    Some(cache) => cache.script(src, self.debug_info())?,
                    None => Module::compile(src, j8framework::CodeKind::Script, self.debug_info())?,
                },
                file: src.file.into(),
            },
            j8framework::ScriptKind::Module => Work::Module { text: src.text.into(), file: src.file.into() },
        }))
    }

    pub fn checkpoint() -> Job<'p> {
        Job::from(Work::Drain)
    }

    pub fn resume(&self, isolate: &mut j8framework::Isolate<'p>, cx: &j8framework::ScopeGlobal<'p, j8framework::ContextBrand>, job: &mut Job<'p>, quota: &mut j8framework::Quota) -> Result<Option<j8framework::ScopeGlobal<'p, j8framework::ValueBrand>>, Error> {
        let mut budget = Some(quota.left());
        let done = isolate.enter(cx, self, |a| self.advance(a, job, &mut budget));
        quota.spend(quota.left() - budget.unwrap_or(0));
        if done? {
            return Ok(job.value.take());
        }
        quota.spend(quota.left());
        Ok(None)
    }

    fn advance<'scope>(&self, a: &mut Scope<'scope, '_>, job: &mut Job<'p>, budget: &mut Option<usize>) -> Result<bool, Error> {
        loop {
            let running = match job.parked.take() {
                Some(root) => j8framework::CellExt::borrow_mut(a.mc.fetch(&root), a.mc).take().expect("a parked execution"),
                None => match self.next_running(a, job, budget)? {
                    Next::Run(r) => r,
                    Next::Paused => return Ok(false),
                    Next::Done => {
                        if let Some(entry) = job.entry.take() {
                            let promise = a.local(&entry);
                            match Scope::promise_state(promise) {
                                Some(j8framework::Settled::Rejected(reason)) => return Err(a.throw_value(reason)),
                                Some(j8framework::Settled::Pending) => {
                                    if let Some(p) = promise.as_object() {
                                        a.set_promise_handled(p, false);
                                    }
                                }
                                _ => {}
                            }
                            job.value = Some(entry);
                        }
                        if job.value.is_none() {
                            job.value = Some(a.global(Value::Undefined));
                        }
                        return Ok(true);
                    }
                },
            };
            let Execution { state, after } = running;
            let mark = a.handles.len();
            let held = after.hold(a);
            let r = self.execute(a, state, budget);
            let after = held.restore(a, mark);
            match r {
                Ok(Outcome::Paused(state)) => {
                    job.parked = Some(a.mc.stash::<ParkedBrand>(a.mc.alloc(j8framework::GcCell::new(a.mc, Some(Execution { state: *state, after })))));
                    return Ok(false);
                }
                Ok(Outcome::Return(v)) => self.finished(a, job, after, Ok(v))?,
                Err(e) => self.finished(a, job, after, Err(e))?,
            }
        }
    }

    fn next_running<'scope>(&self, a: &mut Scope<'scope, '_>, job: &mut Job<'p>, budget: &mut Option<usize>) -> Result<Next<'scope>, Error> {
        loop {
            match &mut job.work {
                Work::Script { .. } => {
                    let Work::Script { module, file } = core::mem::replace(&mut job.work, Work::Drain) else { unreachable!("a script") };
                    let main = self.load_script(a.mc, module, &file);
                    return Ok(Next::Run(Execution { state: self.script_state(a, main)?, after: After::Script }));
                }
                Work::Module { .. } => {
                    let Work::Module { text, file } = core::mem::replace(&mut job.work, Work::Drain) else { unreachable!("a module") };
                    let promise = self.import(a, &file, &text)?;
                    a.set_promise_handled(promise, true);
                    job.entry = Some(a.global(promise.into()));
                }
                Work::Drain => {
                    if *budget == Some(0) {
                        return Ok(Next::Paused);
                    }
                    *budget = budget.map(|b| b - 1);
                    let Some(next) = a.next_job() else {
                        job.work = Work::Spent;
                        return Ok(Next::Done);
                    };
                    if let Some(r) = self.job_running(a, next)? {
                        return Ok(Next::Run(r));
                    }
                }
                Work::Spent => return Ok(Next::Done),
            }
        }
    }

    fn job_running<'scope>(&self, a: &mut Scope<'scope, '_>, job: j8framework::Job<'scope>) -> Result<Option<Execution<'scope>>, Error> {
        use j8framework::{Job as J, Reaction as R};
        match job {
            J::Reaction { reaction: R::Then { on_fulfilled, on_rejected, capability }, value, rejected } => {
                let handler = if rejected { on_rejected } else { on_fulfilled };
                if let ValueKind::Object(o) = handler.kind()
                    && let Some(c) = o.closure()
                    && let l = self.code(&c)
                    && !l.class_constructor
                {
                    self.note_user();
                    let mut regs = a.take_regs();
                    regs.clear();
                    let f = Self::open(a, &mut regs, l, c, handler, Value::Undefined, Args::Values(&[value]), Opening::Call);
                    return Ok(Some(Execution { state: self.state(regs, f), after: After::Reaction(capability) }));
                }
            }
            J::Reaction { reaction: R::Await { coroutine }, value, rejected } if !Self::coroutine(coroutine, |c| c.requests.is_some()) => {
                let promise = Self::coroutine(coroutine, |c| c.promise);
                let mode = if rejected { Resume::Throw } else { Resume::Next };
                return match self.resume_open(a, coroutine, mode, value) {
                    Ok(Resumed::Frame(regs, f, _)) => Ok(Some(Execution { state: self.state(regs, f), after: After::Await(coroutine) })),
                    Ok(Resumed::Step(step)) => a.driven(promise, Ok(step)).map(|()| None),
                    Err(e) => a.driven(promise, Err(e)).map(|()| None),
                };
            }
            _ => {}
        }
        a.run_job(job)?;
        Ok(None)
    }

    fn finished<'scope>(&self, a: &mut Scope<'scope, '_>, job: &mut Job<'p>, after: After<'scope>, r: Result<Value<'scope>, Error>) -> Result<(), Error> {
        match after {
            After::Script => job.value = Some(a.global(r?)),
            After::Reaction(capability) => a.reacted(capability, r)?,
            After::Await(co) => {
                let promise = Self::coroutine(co, |c| c.promise);
                let step = self.resume_close(a, co, r);
                a.driven(promise, step)?;
            }
        }
        Ok(())
    }

    pub fn eval(&self, isolate: &mut j8framework::Isolate<'p>, cx: &j8framework::ScopeGlobal<'p, j8framework::ContextBrand>, src: &j8framework::Source<'_>) -> Result<j8framework::ScopeGlobal<'p, j8framework::ValueBrand>, Error> {
        self.run_to_end(isolate, cx, src, j8framework::ScriptKind::Classic)
    }

    pub fn eval_module(&self, isolate: &mut j8framework::Isolate<'p>, cx: &j8framework::ScopeGlobal<'p, j8framework::ContextBrand>, src: &j8framework::Source<'_>) -> Result<j8framework::ScopeGlobal<'p, j8framework::ValueBrand>, Error> {
        self.run_to_end(isolate, cx, src, j8framework::ScriptKind::Module)
    }

    pub fn run_to_end(&self, isolate: &mut j8framework::Isolate<'p>, cx: &j8framework::ScopeGlobal<'p, j8framework::ContextBrand>, src: &j8framework::Source<'_>, kind: j8framework::ScriptKind) -> Result<j8framework::ScopeGlobal<'p, j8framework::ValueBrand>, Error> {
        let mut job = self.start(src, kind)?;
        loop {
            if let Some(g) = self.resume(isolate, cx, &mut job, &mut j8framework::Quota::from(Self::SLICE))? {
                return Ok(g);
            }
        }
    }
}
