use alloc::vec::Vec;
use core::cell::Cell;

use j8bytecode::{Bytecode, Constant, OperandValue, SharedFunction};
use j8framework::{Scope, Closure, Error, FuncId, JsEnv, JsObject, ShapeKey, Value};

pub use crate::ic::{GlobalFeedback, PropertyFeedback, StringFeedback};
use crate::{Args, Budget, Executor, Frame, Loaded, State};

pub trait Tier {
    fn enter(&self, e: &mut Entry<'_, '_, '_, '_, '_>) -> Tiered;
    fn osr(&self, e: &mut Entry<'_, '_, '_, '_, '_>, at: usize) -> Tiered;
    fn resume(&self, e: &mut Entry<'_, '_, '_, '_, '_>, pc: usize) -> Tiered;
    fn eager(&self) -> bool {
        false
    }

    fn call<'scope>(&self, exe: &Executor, a: &mut Scope<'scope, '_>, f: FuncId, callee: Value<'scope>, this: Value<'scope>, args: &[Value<'scope>]) -> Option<Result<Value<'scope>, Error>>;

    fn callback<'scope>(&self, _exe: &Executor, _a: &mut Scope<'scope, '_>, _f: Function<'_>, _callee: Value<'scope>, _this: Value<'scope>) -> Option<j8framework::Callback<'scope>> {
        None
    }

    fn entered(&self) {}
}

#[derive(Debug)]
pub enum Tiered {
    Interpret,
    Resumed,
    Threw(Error),
}

#[derive(Clone, Copy, Debug)]
pub struct Insn<'e> {
    pub bytecode: Bytecode,
    pub operands: &'e [OperandValue],
    pub target: usize,
    pub start: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reg {
    Param(usize),
    Local(usize),
    Context,
    Closure,
    Other,
}

pub struct FrameSlots;

impl FrameSlots {
    pub const CONTEXT: usize = Frame::CONTEXT;
    pub const CLOSURE: usize = Frame::CLOSURE;
    pub const NEW_TARGET: usize = Frame::NEW_TARGET;
    pub const NEW_THIS: usize = Frame::NEW_THIS;
}

#[derive(Clone, Copy, Debug)]
pub struct ScopeLayout {
    pub shape: j8framework::ShapeId,
    pub slots: usize,
}

#[derive(Clone, Copy)]
pub struct Function<'e>(&'e Loaded);

impl<'e> Function<'e> {
    pub fn id(self) -> FuncId {
        self.0.id
    }

    pub fn shared(self) -> &'e SharedFunction {
        &self.0.func
    }

    pub fn len(self) -> usize {
        self.0.insns.len()
    }

    pub fn is_empty(self) -> bool {
        self.0.insns.is_empty()
    }

    pub fn insn(self, i: usize) -> Insn<'e> {
        let d = &self.0.insns[i];
        Insn { bytecode: d.bytecode, operands: &d.operands.all()[..d.bytecode.operand_types().len()], target: d.target as usize, start: d.start }
    }

    pub fn index_of(self, byte: usize) -> usize {
        self.0.index_of(byte)
    }

    pub fn reg(self, k: OperandValue) -> Reg {
        let header = OperandValue::try_from(Frame::HEADER).unwrap_or(0);
        if k > Loaded::SPECIAL_LIMIT {
            if k >= 0 {
                return Reg::Local(usize::try_from(k).unwrap_or(0));
            }
            if k == -OperandValue::try_from(Frame::CONTEXT).unwrap_or(0) {
                return Reg::Context;
            }
            if k == -OperandValue::try_from(Frame::CLOSURE).unwrap_or(0) {
                return Reg::Closure;
            }
            let params = OperandValue::from(self.0.func.bytecode.parameter_count);
            let p = k + params + header;
            return match usize::try_from(p) {
                Ok(p) if k < -header => Reg::Param(p),
                _ => Reg::Other,
            };
        }
        Reg::Other
    }

    pub fn constant(self, k: OperandValue) -> Option<&'e Constant> {
        self.0.func.bytecode.constants.get(usize::try_from(k).ok()?)
    }

    pub fn name(self, k: OperandValue) -> &'e str {
        Executor::name_at(self.0, k)
    }

    pub fn property(self, slot: OperandValue) -> Option<PropertyFeedback> {
        self.0.ics.get(usize::try_from(slot).ok()?)?.feedback()
    }

    pub fn string_property(self, slot: OperandValue) -> Option<StringFeedback> {
        self.0.ics.get(usize::try_from(slot).ok()?)?.string_feedback()
    }

    pub fn global(self, slot: OperandValue) -> Option<GlobalFeedback> {
        self.0.globals.get(usize::try_from(slot).ok()?)?.feedback()
    }

    pub fn call_target(self, slot: OperandValue) -> Option<FuncId> {
        match self.0.call_ics.get(usize::try_from(slot).ok()?)?.get() {
            0 | u32::MAX => None,
            f => Some(FuncId(f - 1)),
        }
    }

    pub fn call_native(self, slot: OperandValue) -> Option<usize> {
        let i = usize::try_from(slot).ok()?;
        if self.0.call_ics.get(i)?.get() != 0 {
            return None;
        }
        match self.0.native_ics.get(i)?.get() {
            0 | u64::MAX => None,
            p => usize::try_from(p).ok(),
        }
    }

    pub fn literal_shape(self, k: OperandValue) -> Option<ShapeKey> {
        self.0.literals.get(usize::try_from(k).ok()?)?.get()
    }

    pub fn scope(self, k: OperandValue) -> Option<ScopeLayout> {
        let (shape, n) = self.0.scope_shape(k)?;
        Some(ScopeLayout { shape, slots: n as usize })
    }

    pub fn function_scope(self) -> Option<j8framework::ShapeId> {
        self.0.scope
    }

    pub fn prototype(self) -> Option<PropertyFeedback> {
        self.0.prototype.feedback()
    }

    pub fn instance_size(self) -> usize {
        usize::from(self.0.instance_size.get())
    }

    pub fn plain(self) -> bool {
        let l = self.0;
        !(l.class_constructor || l.generator || l.asynchronous || l.keeps_args || l.extension)
    }

    pub fn extension(self) -> bool {
        self.0.extension
    }

    pub fn constructs(self) -> bool {
        self.0.constructs
    }

    pub fn class_constructor(self) -> bool {
        self.0.class_constructor
    }

    pub fn derived(self) -> bool {
        self.0.derived
    }

    pub fn wraps_this(self) -> bool {
        self.0.wraps_this
    }

    pub fn strict(self) -> bool {
        self.0.func.strict
    }

    pub fn tier_word(self) -> &'e Cell<usize> {
        &self.0.tier
    }
}

pub struct DeoptFrame<'scope> {
    pub func: FuncId,
    pub callee: Value<'scope>,
    pub params: Vec<Value<'scope>>,
    pub argc: usize,
    pub regs: Vec<Value<'scope>>,
    pub context: Value<'scope>,
    pub acc: Value<'scope>,
    pub pc: usize,
    pub construct: Option<(Value<'scope>, Value<'scope>)>,
}

#[expect(private_interfaces, reason = "built by the executor only")]
pub struct Entry<'r, 'scope, 'x, 'b, 'p> {
    pub exe: &'r Executor<'p>,
    pub a: &'r mut Scope<'scope, 'x>,
    pub st: &'r mut State<'scope>,
    pub budget: &'r mut Budget<'b>,
}

impl<'r, 'scope, 'x, 'p> Entry<'r, 'scope, 'x, '_, 'p> {
    pub fn executor(&self) -> &'r Executor<'p> {
        self.exe
    }

    pub fn scope(&mut self) -> &mut Scope<'scope, 'x> {
        self.a
    }

    pub fn function(&self) -> Function<'r> {
        Function(unsafe { self.st.f().loaded.get() })
    }

    pub fn r0(&mut self) -> *mut Value<'scope> {
        let base = self.st.f().base;
        self.st.regs.as_mut_ptr().wrapping_add(base)
    }

    pub fn params(&mut self) -> *mut Value<'scope> {
        let at = self.st.f().params;
        self.st.regs.as_mut_ptr().wrapping_add(at)
    }

    pub fn constructing(&self) -> bool {
        let f = self.st.f();
        self.st.regs[f.base - Frame::NEW_TARGET].bits() != j8framework::ValueBits::UNDEFINED
    }

    pub fn callee(&self) -> Value<'scope> {
        let f = self.st.f();
        self.st.regs[f.base - Frame::CLOSURE]
    }

    pub fn argc(&self) -> usize {
        self.st.f().argc
    }

    pub fn acc(&self) -> Value<'scope> {
        self.st.acc
    }

    pub fn depth(&self) -> usize {
        self.st.level()
    }

    pub fn spend(&mut self, n: usize) -> bool {
        let b = &mut *self.budget;
        if b.stop.is_set() {
            b.tick = 0;
            return b.refill();
        }
        if n <= b.tick {
            b.tick -= n;
            return false;
        }
        let mut left = n - b.tick;
        b.tick = 0;
        while left > 0 {
            if b.refill() {
                return true;
            }
            let t = left.min(b.tick + 1);
            b.tick -= t - 1;
            left -= t;
        }
        false
    }

    pub fn resume_at(&mut self, pc: usize, acc: Value<'scope>) {
        let top = self.st.f_mut();
        top.pc = pc;
        top.insn = pc;
        self.st.acc = acc;
    }

    pub fn resume(&mut self, frames: Vec<DeoptFrame<'scope>>) {
        let mut it = frames.into_iter();
        if let Some(first) = it.next() {
            let f = *self.st.f();
            let regs = &mut self.st.regs;
            for (i, v) in first.params.iter().enumerate() {
                regs[f.params + i] = *v;
            }
            for (i, v) in first.regs.iter().enumerate() {
                regs[f.base + i] = *v;
            }
            if first.context.bits() != j8framework::ValueBits::UNINIT {
                regs[f.base - Frame::CONTEXT] = first.context;
            }
            if let Some((nt, this)) = first.construct {
                regs[f.base - Frame::NEW_TARGET] = nt;
                regs[f.base - Frame::NEW_THIS] = this;
            }
            let top = self.st.f_mut();
            top.pc = first.pc;
            top.insn = first.pc;
            self.st.acc = first.acc;
        }
        for d in it {
            self.exe.push_deopt(self.a, self.st, d);
        }
    }
}

impl Executor<'_> {
    pub fn jit_function(&self, f: FuncId) -> Function<'_> {
        Function(self.function(f))
    }

    pub fn jit_live_function(&self, f: FuncId) -> Option<Function<'_>> {
        self.live_function(f).map(Function)
    }

    pub fn jit_keep(&self, f: FuncId, shapes: &[ShapeKey], codes: &[FuncId]) {
        let Some(l) = self.live_function(f) else { return };
        l.jit_shapes.borrow_mut().extend_from_slice(shapes);
        l.jit_codes.borrow_mut().extend(codes.iter().filter_map(|&c| self.live_function(c)).map(Loaded::code));
    }

    pub fn jit_closure_function(&self, c: &Closure<'_>) -> Function<'_> {
        Function(self.code(c))
    }

    fn push_deopt<'scope>(&self, a: &mut Scope<'scope, '_>, st: &mut State<'scope>, d: DeoptFrame<'scope>) {
        let Some(c) = d.callee.as_object().and_then(|o| o.closure()) else { unreachable!("a deopt frame of a closure") };
        let l = self.code(&c);
        let args: Vec<Value<'scope>> = d.params.iter().skip(1).copied().chain(core::iter::repeat(Value::Undefined)).take(d.argc.max(d.params.len().saturating_sub(1))).collect();
        let this = d.params.first().copied().unwrap_or_default();
        let mut f = Self::open(a, &mut st.regs, l, c, d.callee, this, Args::Values(&args), crate::Opening::Call);
        (f.pc, f.insn, f.argc) = (d.pc, d.pc, d.argc);
        let base = f.base;
        st.regs[f.params] = this;
        for (i, v) in d.regs.iter().enumerate() {
            st.regs[base + i] = *v;
        }
        if d.context.bits() != j8framework::ValueBits::UNINIT {
            st.regs[base - Frame::CONTEXT] = d.context;
        }
        if let Some((nt, this)) = d.construct {
            st.regs[base - Frame::NEW_TARGET] = nt;
            st.regs[base - Frame::NEW_THIS] = this;
        }
        st.frames.push(f);
        st.acc = d.acc;
    }

    pub fn resume_frames<'scope>(&self, a: &mut Scope<'scope, '_>, frames: Vec<DeoptFrame<'scope>>, throw: Option<Value<'scope>>) -> Result<Value<'scope>, Error> {
        let level = self.deeper(a)?;
        let mut regs = a.take_regs();
        regs.clear();
        let mut st = State { regs, frames: self.spare_frames.borrow_mut().pop().unwrap_or_default(), ..State::default() };
        st.frames.clear();
        for d in frames {
            self.push_deopt(a, &mut st, d);
        }
        if let Some(v) = throw
            && !Self::unwind(&mut st, v)
        {
            self.recycle(a, st);
            return Err(a.throw_value(v));
        }
        self.note_user();
        self.deep(level, || self.execute(a, st, &mut None).map(crate::Outcome::returned))
    }

    pub fn jit_create_closure<'scope>(&self, a: &Scope<'scope, '_>, f: FuncId, k: OperandValue, env: Option<JsEnv<'scope>>) -> JsObject<'scope> {
        self.create_closure(a, self.function(f), k, env)
    }

    pub fn jit_object_literal<'scope>(&self, a: &Scope<'scope, '_>, f: FuncId, k: OperandValue) -> JsObject<'scope> {
        Self::object_literal(a, self.function(f), k)
    }

    pub fn jit_regexp_literal<'scope>(&self, a: &mut Scope<'scope, '_>, f: FuncId, k: OperandValue, bits: OperandValue) -> Result<(Value<'scope>, bool), Error> {
        Self::regexp_literal(a, self.function(f), k, bits)
    }

    pub fn jit_global_load<'scope>(&self, a: &mut Scope<'scope, '_>, f: FuncId, k: OperandValue, slot: OperandValue, inside_typeof: bool) -> Result<Value<'scope>, Error> {
        let l = self.function(f);
        let name = Self::name_at(l, k);
        let ic = l.globals.get(usize::try_from(slot).unwrap_or(usize::MAX)).ok_or_else(|| a.throw(j8framework::ErrorKind::Reference, "no global cache"))?;
        self.global_load(a, ic, name, inside_typeof)
    }
}
