use j8bytecode::{Bytecode, Constant, OperandValue};
use j8framework::{Scope, Env, Error, ErrorKind, Value, ValueBits};

use crate::{Args, Budget, Executor, Flow, Frame, Insn, Loaded, Opening, Outcome, State};

#[expect(private_interfaces, reason = "crate-internal types; the module is private")]
pub struct Cx<'r, 'scope, 'x, 'b, 'p> {
    pub exe: &'r Executor<'p>,
    pub a: &'r mut Scope<'scope, 'x>,
    pub st: &'r mut State<'scope>,
    pub loaded: &'r Loaded,
    pub budget: &'r mut Budget<'b>,
    pub generation: u64,
}

#[expect(private_interfaces, reason = "crate-internal types; the module is private")]
pub enum Exit<'scope> {
    Frame,
    Done(Outcome<'scope>),
    Throw(Error),
    #[cfg(not(tail_calls))]
    Next(*const Insn, Value<'scope>),
}

pub type Handler = for<'r, 'scope, 'x, 'b, 'p> fn(&mut Cx<'r, 'scope, 'x, 'b, 'p>, *const Insn, *mut Value<'scope>, Value<'scope>) -> Exit<'scope>;

#[cfg(tail_calls)]
macro_rules! next {
    ($cx:ident, $pc:expr, $regs:ident, $acc:expr) => {{
        let pc: *const Insn = $pc;
        become (unsafe { (*pc).handler.get() })($cx, pc, $regs, $acc)
    }};
}

#[cfg(not(tail_calls))]
macro_rules! next {
    ($cx:ident, $pc:expr, $regs:ident, $acc:expr) => {{
        let pc: *const Insn = $pc;
        return Exit::Next(pc, $acc);
    }};
}

macro_rules! handlers {
    ($($name:ident($cx:ident, $pc:ident, $regs:ident, $acc:ident, $o:ident) $body:block)*) => {$(
        #[allow(unused_mut, unused_variables, clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "operands are in range")]
        fn $name<'scope>($cx: &mut Cx<'_, 'scope, '_, '_, '_>, $pc: *const Insn, $regs: *mut Value<'scope>, mut $acc: Value<'scope>) -> Exit<'scope> {
            let $o = unsafe { (*$pc).operands.all() };
            $body
        }
    )*};
}

macro_rules! step {
    ($pc:ident) => {
        unsafe { $pc.add(1) }
    };
}

macro_rules! target {
    ($cx:ident, $pc:ident) => {
        unsafe { $cx.loaded.insns.as_ptr().add((*$pc).target as usize) }
    };
}

macro_rules! reg {
    ($cx:ident, $regs:ident, $k:expr) => {{
        let k = $k;
        if k > Loaded::SPECIAL_LIMIT {
            unsafe { *$regs.offset(k as isize) }
        } else {
            Executor::reg(&$cx.st.regs, $cx.st.f(), k)
        }
    }};
}

#[cfg(tail_calls)]
macro_rules! run {
    ($h:ident, $cx:ident, $pc:ident, $regs:ident, $acc:ident) => {
        become $h($cx, $pc, $regs, $acc)
    };
}

#[cfg(not(tail_calls))]
macro_rules! run {
    ($h:ident, $cx:ident, $pc:ident, $regs:ident, $acc:ident) => {
        $h($cx, $pc, $regs, $acc)
    };
}

#[cfg(tail_calls)]
macro_rules! fall {
    ($cx:ident, $pc:ident, $regs:ident, $acc:ident) => {
        become slow($cx, $pc, $regs, $acc)
    };
}

#[cfg(not(tail_calls))]
macro_rules! fall {
    ($cx:ident, $pc:ident, $regs:ident, $acc:ident) => {
        return slow($cx, $pc, $regs, $acc)
    };
}

#[cfg(tail_calls)]
macro_rules! pass {
    ($f:ident, $cx:ident, $pc:ident, $regs:ident, $acc:ident) => {
        become $f($cx, $pc, $regs, $acc)
    };
}

#[cfg(not(tail_calls))]
macro_rules! pass {
    ($f:ident, $cx:ident, $pc:ident, $regs:ident, $acc:ident) => {
        return $f($cx, $pc, $regs, $acc)
    };
}

macro_rules! arith {
    ($cx:ident, $pc:ident, $regs:ident, $acc:ident, $l:expr, $r:expr, $sym:tt) => {{
        let (l, r) = ($l, $r);
        let (Some(x), Some(y)) = (l.as_number(), r.as_number()) else { fall!($cx, $pc, $regs, $acc) };
        next!($cx, step!($pc), $regs, Value::Number(x $sym y))
    }};
}

macro_rules! relational {
    ($cx:ident, $pc:ident, $regs:ident, $acc:ident, $o:ident, $sym:tt) => {{
        let l = reg!($cx, $regs, $o[0]);
        let (Some(x), Some(y)) = (l.as_number(), $acc.as_number()) else { fall!($cx, $pc, $regs, $acc) };
        next!($cx, step!($pc), $regs, Value::Bool(x $sym y))
    }};
}

handlers! {
    ldar(cx, pc, regs, acc, o) { next!(cx, step!(pc), regs, reg!(cx, regs, o[0])) }
    star(cx, pc, regs, acc, o) {
        if o[0] > Loaded::SPECIAL_LIMIT {
            unsafe { *regs.offset(o[0] as isize) = acc };
            next!(cx, step!(pc), regs, acc)
        }
        fall!(cx, pc, regs, acc)
    }
    mov(cx, pc, regs, acc, o) {
        if o[1] > Loaded::SPECIAL_LIMIT {
            let v = reg!(cx, regs, o[0]);
            unsafe { *regs.offset(o[1] as isize) = v };
            next!(cx, step!(pc), regs, acc)
        }
        fall!(cx, pc, regs, acc)
    }
    lda_zero(cx, pc, regs, acc, o) { next!(cx, step!(pc), regs, Value::Number(0.0)) }
    lda_smi(cx, pc, regs, acc, o) { next!(cx, step!(pc), regs, Value::Number(o[0] as f64)) }
    lda_undefined(cx, pc, regs, acc, o) { next!(cx, step!(pc), regs, Value::Undefined) }
    lda_null(cx, pc, regs, acc, o) { next!(cx, step!(pc), regs, Value::Null) }
    lda_the_hole(cx, pc, regs, acc, o) { next!(cx, step!(pc), regs, Value::Uninit) }
    lda_true(cx, pc, regs, acc, o) { next!(cx, step!(pc), regs, Value::Bool(true)) }
    lda_false(cx, pc, regs, acc, o) { next!(cx, step!(pc), regs, Value::Bool(false)) }
    add(cx, pc, regs, acc, o) {
        let l = reg!(cx, regs, o[0]);
        if let (Some(x), Some(y)) = (l.as_number(), acc.as_number()) {
            next!(cx, step!(pc), regs, Value::Number(x + y))
        }
        pass!(add_text, cx, pc, regs, acc)
    }
    sub(cx, pc, regs, acc, o) { arith!(cx, pc, regs, acc, reg!(cx, regs, o[0]), acc, -) }
    mul(cx, pc, regs, acc, o) { arith!(cx, pc, regs, acc, reg!(cx, regs, o[0]), acc, *) }
    div(cx, pc, regs, acc, o) { arith!(cx, pc, regs, acc, reg!(cx, regs, o[0]), acc, /) }
    add_smi(cx, pc, regs, acc, o) {
        if let Some(x) = acc.as_number() {
            next!(cx, step!(pc), regs, Value::Number(x + o[0] as f64))
        }
        pass!(add_smi_text, cx, pc, regs, acc)
    }
    sub_smi(cx, pc, regs, acc, o) { arith!(cx, pc, regs, acc, acc, Value::Number(o[0] as f64), -) }
    mul_smi(cx, pc, regs, acc, o) { arith!(cx, pc, regs, acc, acc, Value::Number(o[0] as f64), *) }
    div_smi(cx, pc, regs, acc, o) { arith!(cx, pc, regs, acc, acc, Value::Number(o[0] as f64), /) }
    inc(cx, pc, regs, acc, o) {
        if let Some(x) = acc.as_number() {
            next!(cx, step!(pc), regs, Value::Number(x + 1.0))
        }
        fall!(cx, pc, regs, acc)
    }
    dec(cx, pc, regs, acc, o) {
        if let Some(x) = acc.as_number() {
            next!(cx, step!(pc), regs, Value::Number(x - 1.0))
        }
        fall!(cx, pc, regs, acc)
    }
    test_less_than(cx, pc, regs, acc, o) { relational!(cx, pc, regs, acc, o, <) }
    test_greater_than(cx, pc, regs, acc, o) { relational!(cx, pc, regs, acc, o, >) }
    test_less_than_or_equal(cx, pc, regs, acc, o) { relational!(cx, pc, regs, acc, o, <=) }
    test_greater_than_or_equal(cx, pc, regs, acc, o) { relational!(cx, pc, regs, acc, o, >=) }
    test_equal_strict(cx, pc, regs, acc, o) {
        let l = reg!(cx, regs, o[0]);
        match (l.as_number(), acc.as_number()) {
            (Some(x), Some(y)) => next!(cx, step!(pc), regs, Value::Bool(x == y)),
            _ => fall!(cx, pc, regs, acc),
        }
    }
    jump(cx, pc, regs, acc, o) { next!(cx, target!(cx, pc), regs, acc) }
    jump_if_true(cx, pc, regs, acc, o) {
        if acc.bits() == ValueBits::TRUE {
            next!(cx, target!(cx, pc), regs, acc)
        }
        next!(cx, step!(pc), regs, acc)
    }
    jump_if_false(cx, pc, regs, acc, o) {
        if acc.bits() == ValueBits::FALSE {
            next!(cx, target!(cx, pc), regs, acc)
        }
        next!(cx, step!(pc), regs, acc)
    }
    jump_if_to_boolean_true(cx, pc, regs, acc, o) {
        if cx.a.to_boolean(acc) {
            next!(cx, target!(cx, pc), regs, acc)
        }
        next!(cx, step!(pc), regs, acc)
    }
    jump_if_to_boolean_false(cx, pc, regs, acc, o) {
        if cx.a.to_boolean(acc) {
            next!(cx, step!(pc), regs, acc)
        }
        next!(cx, target!(cx, pc), regs, acc)
    }
    jump_loop(cx, pc, regs, acc, o) {
        if cx.budget.tick > 0 {
            cx.budget.tick -= 1;
            next!(cx, target!(cx, pc), regs, acc)
        }
        fall!(cx, pc, regs, acc)
    }
    lda_current_context_slot(cx, pc, regs, acc, o) {
        match unsafe { *regs.sub(Frame::CONTEXT) }.as_context() {
            Some(e) => next!(cx, step!(pc), regs, e.load(Executor::env_index(o[0]))),
            None => fall!(cx, pc, regs, acc),
        }
    }
    sta_current_context_slot(cx, pc, regs, acc, o) {
        match unsafe { *regs.sub(Frame::CONTEXT) }.as_context() {
            Some(e) => {
                Env::set(e, cx.a.mc, Executor::env_index(o[0]), acc);
                next!(cx, step!(pc), regs, acc)
            }
            None => fall!(cx, pc, regs, acc),
        }
    }
    throw_reference_error_if_hole(cx, pc, regs, acc, o) {
        if acc.bits() == ValueBits::UNINIT {
            fall!(cx, pc, regs, acc)
        }
        next!(cx, step!(pc), regs, acc)
    }
    push_context(cx, pc, regs, acc, o) {
        if o[0] > Loaded::SPECIAL_LIMIT && acc.as_context().is_some() {
            unsafe {
                *regs.offset(o[0] as isize) = *regs.sub(Frame::CONTEXT);
                *regs.sub(Frame::CONTEXT) = acc;
            }
            next!(cx, step!(pc), regs, acc)
        }
        fall!(cx, pc, regs, acc)
    }
    pop_context(cx, pc, regs, acc, o) {
        let e = reg!(cx, regs, o[0]);
        unsafe { *regs.sub(Frame::CONTEXT) = if e.as_context().is_some() { e } else { Value::Undefined } };
        next!(cx, step!(pc), regs, acc)
    }
    create_block_context(cx, pc, regs, acc, o) {
        let parent = unsafe { *regs.sub(Frame::CONTEXT) }.as_context();
        next!(cx, step!(pc), regs, cx.loaded.block_context(cx.a, o[0], parent))
    }
    create_closure(cx, pc, regs, acc, o) {
        let env = unsafe { *regs.sub(Frame::CONTEXT) }.as_context();
        next!(cx, step!(pc), regs, cx.exe.create_closure(cx.a, cx.loaded, o[0], env).into())
    }
    get_named_property(cx, pc, regs, acc, o) {
        if let Some(ob) = reg!(cx, regs, o[0]).as_object() {
            if let Some(v) = cx.loaded.ics[o[2] as usize].get(ob) {
                next!(cx, step!(pc), regs, v)
            }
            fall!(cx, pc, regs, acc)
        }
        pass!(get_named_string, cx, pc, regs, acc)
    }
    lda_constant(cx, pc, regs, acc, o) {
        let k = o[0] as usize;
        let slot = o[0] as u32;
        match &cx.loaded.func.bytecode.constants[k] {
            Constant::String(s) => {
                let v = cx.a.constant_string(cx.loaded.id.0, slot, s);
                next!(cx, step!(pc), regs, Value::String(v))
            }
            Constant::Utf16(u) => {
                let v = cx.a.constant_units(cx.loaded.id.0, slot, u);
                next!(cx, step!(pc), regs, Value::String(v))
            }
            _ => fall!(cx, pc, regs, acc),
        }
    }
    lda_global(cx, pc, regs, acc, o) {
        let (scope, global) = (cx.a.context.script_scope, cx.a.context.global);
        match cx.loaded.globals[o[1] as usize].get(scope, global) {
            Some(v) => next!(cx, step!(pc), regs, v),
            None => fall!(cx, pc, regs, acc),
        }
    }
}

#[inline(never)]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "a register index fits in isize and a feedback slot is not negative")]
fn get_named_string<'scope>(cx: &mut Cx<'_, 'scope, '_, '_, '_>, pc: *const Insn, regs: *mut Value<'scope>, acc: Value<'scope>) -> Exit<'scope> {
    let o = unsafe { (*pc).operands.all() };
    if let Some(s) = reg!(cx, regs, o[0]).as_string() {
        let ic = &cx.loaded.ics[o[2] as usize];
        if ic.names_length() {
            let n = u32::try_from(s.utf16_len()).unwrap_or(u32::MAX);
            next!(cx, step!(pc), regs, Value::Number(f64::from(n)))
        }
        if let Some(v) = ic.string_value(cx.a.context.string_proto) {
            next!(cx, step!(pc), regs, v)
        }
    }
    fall!(cx, pc, regs, acc)
}

#[inline(never)]
#[allow(clippy::cast_possible_truncation, reason = "a register index fits in isize")]
fn add_text<'scope>(cx: &mut Cx<'_, 'scope, '_, '_, '_>, pc: *const Insn, regs: *mut Value<'scope>, acc: Value<'scope>) -> Exit<'scope> {
    let o = unsafe { (*pc).operands.all() };
    if let Some(v) = cx.a.try_concat(reg!(cx, regs, o[0]), acc) {
        next!(cx, step!(pc), regs, v)
    }
    fall!(cx, pc, regs, acc)
}

#[inline(never)]
#[allow(clippy::cast_precision_loss, reason = "an smi operand fits in an f64")]
fn add_smi_text<'scope>(cx: &mut Cx<'_, 'scope, '_, '_, '_>, pc: *const Insn, regs: *mut Value<'scope>, acc: Value<'scope>) -> Exit<'scope> {
    let o = unsafe { (*pc).operands.all() };
    if let Some(v) = cx.a.try_concat(acc, Value::Number(o[0] as f64)) {
        next!(cx, step!(pc), regs, v)
    }
    fall!(cx, pc, regs, acc)
}

pub enum Entered<'scope> {
    At(*const Insn, *mut Value<'scope>, Value<'scope>),
    Throw(Error),
    No,
}

#[inline]
fn enter<'r, 'scope>(cx: &mut Cx<'r, 'scope, '_, '_, '_>, pc: *const Insn, callee: Value<'scope>, this: Value<'scope>, args: Args<'_, 'scope>, fb: OperandValue) -> Entered<'scope> {
    let Some(o) = callee.as_object() else { return Entered::No };
    let Some(c) = o.closure() else { return Entered::No };
    let l: &'r Loaded = cx.exe.code(&c);
    if l.class_constructor || !cx.st.room() {
        return Entered::No;
    }
    Executor::record_call(cx.loaded, fb, c.func);
    let at = usize::try_from(unsafe { pc.offset_from(cx.loaded.insns.as_ptr()) }).unwrap_or(0);
    let f = cx.st.f_mut();
    f.insn = at;
    f.pc = at + 1;
    if l.tier.get() != 0
        && let Some(t) = &cx.exe.tier
        && let Args::Values(vals) = args
    {
        let r = cx.exe.deep(cx.st.level() + 1, || {
            let _raw = cx.exe.raw_frame(cx.a.stop);
            t.call(cx.exe, cx.a, c.func, callee, this, vals)
        });
        if let Some(r) = r {
            return match r {
                Ok(v) => {
                    let base = cx.st.f().base;
                    Entered::At(unsafe { pc.add(1) }, cx.st.regs.as_mut_ptr().wrapping_add(base), v)
                }
                Err(e) => Entered::Throw(e),
            };
        }
    }
    let next = Executor::open(cx.a, &mut cx.st.regs, l, c, callee, this, args, Opening::Call);
    let base = cx.st.push(next);
    cx.loaded = l;
    if cx.exe.tier.is_some() {
        return tier_enter(cx);
    }
    Entered::At(l.insns.as_ptr(), cx.st.regs.as_mut_ptr().wrapping_add(base), Value::Undefined)
}

#[inline(never)]
fn tier_enter<'scope>(cx: &mut Cx<'_, 'scope, '_, '_, '_>) -> Entered<'scope> {
    let l = cx.loaded;
    let n = l.calls.get().wrapping_add(1);
    l.calls.set(n);
    if l.tier.get() == 0 && n % crate::TIER_CALLS != 0 && !cx.exe.tier.as_ref().is_some_and(|t| t.eager()) {
        let base = cx.st.f().base;
        return Entered::At(l.insns.as_ptr(), cx.st.regs.as_mut_ptr().wrapping_add(base), Value::Undefined);
    }
    let Some(t) = &cx.exe.tier else { unreachable!("checked by the caller") };
    let raw = cx.exe.raw_frame(cx.a.stop);
    let r = t.enter(&mut crate::Entry { exe: cx.exe, a: cx.a, st: cx.st, budget: cx.budget });
    drop(raw);
    match r {
        crate::Tiered::Threw(e) => Entered::Throw(e),
        _ => {
            let f = *cx.st.f();
            let l: &Loaded = unsafe { f.loaded.get() };
            cx.loaded = l;
            Entered::At(l.insns.as_ptr().wrapping_add(f.pc), cx.st.regs.as_mut_ptr().wrapping_add(f.base), cx.st.acc)
        }
    }
}

#[inline]
fn native<'scope>(cx: &mut Cx<'_, 'scope, '_, '_, '_>, pc: *const Insn, callee: Value<'scope>, this: Value<'scope>, args: &[Value<'scope>], fb: OperandValue) -> Option<Result<Value<'scope>, Error>> {
    let Some(o) = callee.as_object() else { return None };
    let n = o.native()?;
    Executor::record_native(cx.loaded, fb, n.f);
    if n.forward != j8framework::Forward::No || !cx.st.room() {
        return None;
    }
    sync(cx, pc);
    Some(cx.exe.native_at(cx.a, cx.st.level() + 1, Value::Undefined, |a| (n.f)(a, this, args)))
}

#[inline]
fn sync(cx: &mut Cx<'_, '_, '_, '_, '_>, pc: *const Insn) {
    let at = usize::try_from(unsafe { pc.offset_from(cx.loaded.insns.as_ptr()) }).unwrap_or(0);
    let f = cx.st.f_mut();
    f.insn = at;
    f.pc = at + 1;
}

#[inline]
fn int_rem(x: f64, y: f64) -> Option<f64> {
    const EXACT: f64 = 9_007_199_254_740_992.0;
    if !(x.abs() < EXACT && y.abs() < EXACT) {
        return None;
    }
    #[expect(clippy::cast_possible_truncation, reason = "below 2^53, checked by converting back")]
    let (a, b) = (x as i64, y as i64);
    #[expect(clippy::cast_precision_loss, reason = "below 2^53")]
    if a as f64 != x || b as f64 != y || b == 0 {
        return None;
    }
    let r = a % b;
    #[expect(clippy::cast_precision_loss, reason = "below 2^53")]
    Some(if r == 0 && x.is_sign_negative() { -0.0 } else { r as f64 })
}

#[inline]
fn int32(v: Value<'_>) -> Option<i32> {
    let x = v.as_number()?;
    #[expect(clippy::cast_possible_truncation, reason = "checked by converting back")]
    let i = x as i32;
    (f64::from(i) == x).then_some(i)
}

macro_rules! bitwise {
    ($cx:ident, $pc:ident, $regs:ident, $acc:ident, $l:expr, $r:expr, |$x:ident, $y:ident| $e:expr) => {{
        let (Some($x), Some($y)) = (int32($l), int32($r)) else { fall!($cx, $pc, $regs, $acc) };
        next!($cx, step!($pc), $regs, Value::Number(f64::from($e)))
    }};
}

macro_rules! call {
    ($cx:ident, $pc:ident, $regs:ident, $acc:ident, $o:ident, $fb:literal, $this:expr, [$($arg:expr),*]) => {{
        let callee = reg!($cx, $regs, $o[0]);
        let args = [$($arg),*];
        match enter($cx, $pc, callee, $this, Args::Values(&args), $o[$fb]) {
            Entered::At(pc, regs, acc) => next!($cx, pc, regs, acc),
            Entered::Throw(e) => return Exit::Throw(e),
            Entered::No => match native($cx, $pc, callee, $this, &args, $o[$fb]) {
                Some(Ok(v)) if $cx.a.mc.generation() != $cx.generation => {
                    $cx.st.acc = v;
                    return Exit::Frame;
                }
                Some(Ok(v)) => {
                    let $regs = $cx.st.regs.as_mut_ptr().wrapping_add($cx.st.f().base);
                    next!($cx, $pc.wrapping_add(1), $regs, v)
                }
                Some(Err(e)) => return Exit::Throw(e),
                None => fall!($cx, $pc, $regs, $acc),
            },
        }
    }};
}

handlers! {
    call_undefined_receiver0(cx, pc, regs, acc, o) { call!(cx, pc, regs, acc, o, 1, Value::Undefined, []) }
    call_undefined_receiver1(cx, pc, regs, acc, o) { call!(cx, pc, regs, acc, o, 2, Value::Undefined, [reg!(cx, regs, o[1])]) }
    call_undefined_receiver2(cx, pc, regs, acc, o) { call!(cx, pc, regs, acc, o, 3, Value::Undefined, [reg!(cx, regs, o[1]), reg!(cx, regs, o[2])]) }
    call_property0(cx, pc, regs, acc, o) { call!(cx, pc, regs, acc, o, 2, reg!(cx, regs, o[1]), []) }
    call_property1(cx, pc, regs, acc, o) { call!(cx, pc, regs, acc, o, 3, reg!(cx, regs, o[1]), [reg!(cx, regs, o[2])]) }
    call_property2(cx, pc, regs, acc, o) { call!(cx, pc, regs, acc, o, 4, reg!(cx, regs, o[1]), [reg!(cx, regs, o[2]), reg!(cx, regs, o[3])]) }
    construct(cx, pc, regs, acc, o) {
        let callee = reg!(cx, regs, o[0]);
        let Some(ob) = callee.as_object() else { fall!(cx, pc, regs, acc) };
        let Some(c) = ob.closure() else { fall!(cx, pc, regs, acc) };
        let l = cx.exe.code(&c);
        if !(l.constructs || l.class_constructor) || l.generator || l.asynchronous || !cx.st.room() {
            fall!(cx, pc, regs, acc)
        }
        Executor::record_call(cx.loaded, o[3], c.func);
        sync(cx, pc);
        let this = if l.derived {
            Value::Uninit
        } else {
            match Executor::create_this(cx.a, l, acc) {
                Ok(t) => Value::Object(t),
                Err(e) => return Exit::Throw(e),
            }
        };
        let first = Executor::slot_of(cx.st.f(), o[1]).unwrap_or(cx.st.regs.len());
        let args = Args::Regs { first, count: o[2] as usize };
        let next = Executor::open(cx.a, &mut cx.st.regs, l, c, callee, this, args, Opening::Construct(acc));
        let base = cx.st.push(next);
        cx.loaded = l;
        if cx.exe.tier.is_some() {
            match tier_enter(cx) {
                Entered::At(pc, regs, acc) => next!(cx, pc, regs, acc),
                Entered::Throw(e) => return Exit::Throw(e),
                Entered::No => unreachable!("the tier runs or interprets"),
            }
        }
        let regs = cx.st.regs.as_mut_ptr().wrapping_add(base);
        next!(cx, l.insns.as_ptr(), regs, Value::Undefined)
    }
    create_object_literal(cx, pc, regs, acc, o) {
        let ob = Executor::object_literal(cx.a, cx.loaded, o[0]);
        next!(cx, step!(pc), regs, ob.into())
    }
    set_named_property(cx, pc, regs, acc, o) {
        if let Some(ob) = reg!(cx, regs, o[0]).as_object()
            && cx.loaded.ics[o[2] as usize].set(cx.a.mc, ob, acc, true)
        {
            next!(cx, step!(pc), regs, acc)
        }
        fall!(cx, pc, regs, acc)
    }
    define_named_own_property(cx, pc, regs, acc, o) {
        if let Some(ob) = reg!(cx, regs, o[0]).as_object()
            && cx.loaded.ics[o[2] as usize].set(cx.a.mc, ob, acc, false)
        {
            next!(cx, step!(pc), regs, acc)
        }
        fall!(cx, pc, regs, acc)
    }
    get_keyed_property(cx, pc, regs, acc, o) {
        if let (Some(ob), Some(i)) = (reg!(cx, regs, o[0]).as_object(), Executor::index(acc))
            && ob.is_array()
            && let Some(v) = ob.elements().get(i)
        {
            next!(cx, step!(pc), regs, v)
        }
        fall!(cx, pc, regs, acc)
    }
    set_keyed_property(cx, pc, regs, acc, o) {
        if let (Some(ob), Some(i)) = (reg!(cx, regs, o[0]).as_object(), Executor::index(reg!(cx, regs, o[1])))
            && ob.write_index(cx.a.mc, u32::try_from(i).unwrap_or(u32::MAX), acc)
        {
            next!(cx, step!(pc), regs, acc)
        }
        fall!(cx, pc, regs, acc)
    }
    mod_(cx, pc, regs, acc, o) {
        let (Some(x), Some(y)) = (reg!(cx, regs, o[0]).as_number(), acc.as_number()) else { fall!(cx, pc, regs, acc) };
        match int_rem(x, y) {
            Some(r) => next!(cx, step!(pc), regs, Value::Number(r)),
            None => fall!(cx, pc, regs, acc),
        }
    }
    mod_smi(cx, pc, regs, acc, o) {
        let Some(x) = acc.as_number() else { fall!(cx, pc, regs, acc) };
        match int_rem(x, o[0] as f64) {
            Some(r) => next!(cx, step!(pc), regs, Value::Number(r)),
            None => fall!(cx, pc, regs, acc),
        }
    }
    bitwise_and(cx, pc, regs, acc, o) { bitwise!(cx, pc, regs, acc, reg!(cx, regs, o[0]), acc, |x, y| x & y) }
    bitwise_or(cx, pc, regs, acc, o) { bitwise!(cx, pc, regs, acc, reg!(cx, regs, o[0]), acc, |x, y| x | y) }
    bitwise_xor(cx, pc, regs, acc, o) { bitwise!(cx, pc, regs, acc, reg!(cx, regs, o[0]), acc, |x, y| x ^ y) }
    shift_left(cx, pc, regs, acc, o) { bitwise!(cx, pc, regs, acc, reg!(cx, regs, o[0]), acc, |x, y| x.wrapping_shl(y as u32 & 31)) }
    shift_right(cx, pc, regs, acc, o) { bitwise!(cx, pc, regs, acc, reg!(cx, regs, o[0]), acc, |x, y| x >> (y as u32 & 31)) }
    shift_right_logical(cx, pc, regs, acc, o) { bitwise!(cx, pc, regs, acc, reg!(cx, regs, o[0]), acc, |x, y| (x as u32) >> (y as u32 & 31)) }
    bitwise_and_smi(cx, pc, regs, acc, o) { bitwise!(cx, pc, regs, acc, acc, Value::Number(o[0] as f64), |x, y| x & y) }
    bitwise_or_smi(cx, pc, regs, acc, o) { bitwise!(cx, pc, regs, acc, acc, Value::Number(o[0] as f64), |x, y| x | y) }
    bitwise_xor_smi(cx, pc, regs, acc, o) { bitwise!(cx, pc, regs, acc, acc, Value::Number(o[0] as f64), |x, y| x ^ y) }
    shift_left_smi(cx, pc, regs, acc, o) { bitwise!(cx, pc, regs, acc, acc, Value::Number(o[0] as f64), |x, y| x.wrapping_shl(y as u32 & 31)) }
    shift_right_smi(cx, pc, regs, acc, o) { bitwise!(cx, pc, regs, acc, acc, Value::Number(o[0] as f64), |x, y| x >> (y as u32 & 31)) }
    shift_right_logical_smi(cx, pc, regs, acc, o) { bitwise!(cx, pc, regs, acc, acc, Value::Number(o[0] as f64), |x, y| (x as u32) >> (y as u32 & 31)) }
    return_(cx, pc, regs, acc, o) {
        let acc = if unsafe { *regs.sub(Frame::NEW_TARGET) } == Value::Undefined {
            acc
        } else {
            let Some(ob) = acc.as_object().or_else(|| unsafe { *regs.sub(Frame::NEW_THIS) }.as_object()) else { fall!(cx, pc, regs, acc) };
            cx.loaded.note_instance(ob);
            ob.into()
        };
        let st = &mut *cx.st;
        if !st.pop_frame() {
            return Exit::Done(Outcome::Return(acc));
        }
        let caller = st.f();
        let (at, base) = (caller.pc, caller.base);
        let l = unsafe { caller.loaded.get() };
        cx.loaded = l;
        let pc = unsafe { l.insns.as_ptr().add(at) };
        let regs = cx.st.regs.as_mut_ptr().wrapping_add(base);
        next!(cx, pc, regs, acc)
    }
}

#[inline(never)]
fn slow<'scope>(cx: &mut Cx<'_, 'scope, '_, '_, '_>, pc: *const Insn, _: *mut Value<'scope>, acc: Value<'scope>) -> Exit<'scope> {
    let base = cx.loaded.insns.as_ptr();
    let at = usize::try_from(unsafe { pc.offset_from(base) }).unwrap_or(0);
    let st = &mut *cx.st;
    let f = st.f_mut();
    f.insn = at;
    f.pc = at + 1;
    st.acc = acc;
    let d = unsafe { &*pc };
    match cx.exe.step(cx.a, st, cx.loaded, d, cx.budget) {
        Ok(Flow::Next) if cx.a.mc.generation() != cx.generation => Exit::Frame,
        Ok(Flow::Next) => {
            #[cfg_attr(not(tail_calls), expect(unused_variables, reason = "the loop reloads the registers"))]
            let regs = st.regs.as_mut_ptr().wrapping_add(st.f().base);
            let acc = st.acc;
            next!(cx, unsafe { base.add(cx.st.f().pc) }, regs, acc)
        }
        Ok(Flow::Frame) => Exit::Frame,
        Ok(Flow::Done(o)) => Exit::Done(o),
        Err(e) => Exit::Throw(e),
    }
}

fn hold<'scope>(cx: &mut Cx<'_, 'scope, '_, '_, '_>, pc: *const Insn, acc: Value<'scope>) -> usize {
    let at = usize::try_from(unsafe { pc.offset_from(cx.loaded.insns.as_ptr()) }).unwrap_or(0);
    let f = cx.st.f_mut();
    f.insn = at;
    f.pc = at;
    cx.st.acc = acc;
    cx.st.level()
}

pub fn brk<'scope>(cx: &mut Cx<'_, 'scope, '_, '_, '_>, pc: *const Insn, _: *mut Value<'scope>, acc: Value<'scope>) -> Exit<'scope> {
    let level = hold(cx, pc, acc);
    cx.exe.debug_break(cx.a, cx.st, level, false);
    let regs = cx.st.regs.as_mut_ptr().wrapping_add(cx.st.f().base);
    let acc = cx.st.acc;
    let h = Insn::handler(unsafe { (*pc).bytecode });
    run!(h, cx, pc, regs, acc)
}

fn statement<'scope>(cx: &mut Cx<'_, 'scope, '_, '_, '_>, pc: *const Insn, _: *mut Value<'scope>, acc: Value<'scope>) -> Exit<'scope> {
    let level = hold(cx, pc, acc);
    cx.exe.debug_break(cx.a, cx.st, level, true);
    #[cfg_attr(not(tail_calls), expect(unused_variables, reason = "the loop reloads the registers"))]
    let regs = cx.st.regs.as_mut_ptr().wrapping_add(cx.st.f().base);
    let acc = cx.st.acc;
    next!(cx, step!(pc), regs, acc)
}

fn illegal<'scope>(cx: &mut Cx<'_, 'scope, '_, '_, '_>, _: *const Insn, _: *mut Value<'scope>, _: Value<'scope>) -> Exit<'scope> {
    Exit::Throw(cx.a.throw(ErrorKind::Syntax, "ran past the end of the bytecode"))
}

impl Insn {
    pub fn handler(bytecode: Bytecode) -> Handler {
        use Bytecode as B;
        match bytecode {
            B::Ldar => ldar,
            B::Star => star,
            B::Mov => mov,
            B::LdaZero => lda_zero,
            B::LdaSmi => lda_smi,
            B::LdaUndefined => lda_undefined,
            B::LdaNull => lda_null,
            B::LdaTheHole => lda_the_hole,
            B::LdaTrue => lda_true,
            B::LdaFalse => lda_false,
            B::LdaConstant => lda_constant,
            B::Add => add,
            B::Sub => sub,
            B::Mul => mul,
            B::Div => div,
            B::AddSmi => add_smi,
            B::SubSmi => sub_smi,
            B::MulSmi => mul_smi,
            B::DivSmi => div_smi,
            B::Inc => inc,
            B::Dec => dec,
            B::TestLessThan => test_less_than,
            B::TestGreaterThan => test_greater_than,
            B::TestLessThanOrEqual => test_less_than_or_equal,
            B::TestGreaterThanOrEqual => test_greater_than_or_equal,
            B::TestEqualStrict => test_equal_strict,
            B::Jump | B::JumpConstant => jump,
            B::JumpIfTrue | B::JumpIfTrueConstant => jump_if_true,
            B::JumpIfFalse | B::JumpIfFalseConstant => jump_if_false,
            B::JumpIfToBooleanTrue | B::JumpIfToBooleanTrueConstant => jump_if_to_boolean_true,
            B::JumpIfToBooleanFalse | B::JumpIfToBooleanFalseConstant => jump_if_to_boolean_false,
            B::JumpLoop => jump_loop,
            B::LdaCurrentContextSlot | B::LdaCurrentContextSlotNoCell | B::LdaImmutableCurrentContextSlot => lda_current_context_slot,
            B::StaCurrentContextSlot | B::StaCurrentContextSlotNoCell => sta_current_context_slot,
            B::ThrowReferenceErrorIfHole => throw_reference_error_if_hole,
            B::PushContext => push_context,
            B::PopContext => pop_context,
            B::CreateBlockContext => create_block_context,
            B::CreateClosure => create_closure,
            B::GetNamedProperty => get_named_property,
            B::LdaGlobal => lda_global,
            B::CallUndefinedReceiver0 => call_undefined_receiver0,
            B::CallUndefinedReceiver1 => call_undefined_receiver1,
            B::CallUndefinedReceiver2 => call_undefined_receiver2,
            B::CallProperty0 => call_property0,
            B::CallProperty1 => call_property1,
            B::CallProperty2 => call_property2,
            B::Return => return_,
            B::Construct => construct,
            B::CreateObjectLiteral => create_object_literal,
            B::SetNamedProperty => set_named_property,
            B::DefineNamedOwnProperty => define_named_own_property,
            B::GetKeyedProperty => get_keyed_property,
            B::SetKeyedProperty => set_keyed_property,
            B::Mod => mod_,
            B::ModSmi => mod_smi,
            B::BitwiseAnd => bitwise_and,
            B::BitwiseOr => bitwise_or,
            B::BitwiseXor => bitwise_xor,
            B::ShiftLeft => shift_left,
            B::ShiftRight => shift_right,
            B::ShiftRightLogical => shift_right_logical,
            B::BitwiseAndSmi => bitwise_and_smi,
            B::BitwiseOrSmi => bitwise_or_smi,
            B::BitwiseXorSmi => bitwise_xor_smi,
            B::ShiftLeftSmi => shift_left_smi,
            B::ShiftRightSmi => shift_right_smi,
            B::ShiftRightLogicalSmi => shift_right_logical_smi,
            B::Illegal => illegal,
            B::Debugger => statement,
            _ => slow,
        }
    }
}

impl Executor<'_> {
    pub fn threaded<'scope>(cx: &mut Cx<'_, 'scope, '_, '_, '_>) -> Exit<'scope> {
        if cx.exe.tier.is_some() {
            if cx.st.f().pc == 0 {
                if let Entered::Throw(e) = tier_enter(cx) {
                    return Exit::Throw(e);
                }
            } else if cx.loaded.tier.get() != 0
                && let Some(t) = &cx.exe.tier
            {
                let pc = cx.st.f().pc;
                let raw = cx.exe.raw_frame(cx.a.stop);
                let r = t.resume(&mut crate::Entry { exe: cx.exe, a: cx.a, st: cx.st, budget: cx.budget }, pc);
                drop(raw);
                match r {
                    crate::Tiered::Threw(e) => return Exit::Throw(e),
                    crate::Tiered::Resumed => {
                        let f = *cx.st.f();
                        cx.loaded = unsafe { f.loaded.get() };
                    }
                    crate::Tiered::Interpret => {}
                }
            }
        }
        let st = &mut *cx.st;
        let pc = &raw const cx.loaded.insns[st.f().pc];
        let regs = st.regs.as_mut_ptr().wrapping_add(st.f().base);
        let acc = st.acc;
        let h = unsafe { (*pc).handler.get() };
        #[cfg(tail_calls)]
        {
            h(cx, pc, regs, acc)
        }
        #[cfg(not(tail_calls))]
        {
            let (mut h, mut pc, mut acc, mut regs) = (h, pc, acc, regs);
            loop {
                match h(cx, pc, regs, acc) {
                    Exit::Next(p, a) => {
                        pc = p;
                        acc = a;
                        regs = cx.st.regs.as_mut_ptr().wrapping_add(cx.st.f().base);
                        h = unsafe { (*pc).handler.get() };
                    }
                    e => return e,
                }
            }
        }
    }
}
