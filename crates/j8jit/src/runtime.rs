use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::RefCell;

use j8executor::{DeoptFrame, Entry, Executor};
use j8framework::{Scope, Error, ObjectData, Slot as PropSlot, Value, ValueBits};

use crate::build::{CONTEXT_TAG, OBJECT_TAG};
use crate::ir::{Helper, Rep, Slot};
use crate::codegen::{Reason, RESUMED, THREW};
use crate::Jit;

pub const BUF_WORDS: usize = 2048;

pub const ARGS_WORDS: usize = 16;

pub const BUDGET_CHUNK: i64 = 1024;

pub const DEPTH_LIMIT: u64 = 2000;

pub const NATIVE_STACK: u64 = 1 << 20;

#[cfg(windows)]
pub(crate) const STACK_MARGIN: u64 = NATIVE_STACK / 4;

#[repr(C)]
pub struct Ctx {
    pub budget: i64,
    pub pending: u64,
    pub global: u64,
    pub script_scope: u64,
    pub object_proto: u64,
    pub ret: u64,
    pub ret_pc: u64,
    pub acc: u64,
    pub depth: u64,
    pub limit: u64,
    pub epoch: u64,
    pub miss: u64,
    pub stack_floor: u64,
    pub array_proto: u64,
    pub indexed_cache: u64,
    pub string_proto: u64,
    pub chars: u64,
    pub empty: u64,
    pub args: [u64; ARGS_WORDS],
    pub buf: [u64; BUF_WORDS],
    pub entry: *mut core::ffi::c_void,
    pub exe: *const core::ffi::c_void,
    pub scope: *mut core::ffi::c_void,
    pub jit: *const Jit,
    pub error: Option<Error>,
}

impl Ctx {
    pub const BUDGET: i32 = 0;
    pub const PENDING: i32 = 8;
    pub const GLOBAL: i32 = 16;
    pub const SCRIPT_SCOPE: i32 = 24;
    pub const OBJECT_PROTO: i32 = 32;
    pub const RET: i32 = 40;
    pub const RET_PC: i32 = 48;
    pub const ACC: i32 = 56;
    pub const EPOCH: i32 = 80;
    pub const MISS: i32 = 88;
    pub const STACK_FLOOR: i32 = 96;
    pub const ARRAY_PROTO: i32 = 104;
    pub const INDEXED_CACHE: i32 = 112;
    pub const STRING_PROTO: i32 = 120;
    pub const CHARS: i32 = 128;
    pub const EMPTY: i32 = 136;
    pub const ARGS: i32 = 144;
    pub const BUF: i32 = 272;

    pub fn new(e: &mut Entry<'_, '_, '_, '_, '_>, jit: &Jit) -> Box<Ctx> {
        let acc = e.acc().bits();
        let entry: *mut core::ffi::c_void = core::ptr::from_mut(e).cast();
        let exe = e.executor();
        let mut c = Self::bare(exe, e.scope(), jit);
        c.acc = acc;
        c.entry = entry;
        c
    }

    pub fn bare(exe: &Executor, a: &mut Scope<'_, '_>, jit: &Jit) -> Box<Ctx> {
        let exe: *const core::ffi::c_void = core::ptr::from_ref(exe).cast();
        let agent: *mut core::ffi::c_void = core::ptr::from_mut(a).cast();
        if let Some(mut c) = jit.spare_ctx() {
            c.budget = BUDGET_CHUNK;
            c.pending = 0;
            c.refresh_ptrs(a);
            c.acc = ValueBits::UNDEFINED;
            c.epoch = jit.bump();
            c.miss = 0;
            c.stack_floor = jit.stack_floor();
            c.entry = core::ptr::null_mut();
            c.exe = exe;
            c.scope = agent;
            c.error = None;
            return c;
        }
        let mut c = Box::new(Ctx {
            budget: BUDGET_CHUNK,
            pending: 0,
            global: 0,
            script_scope: 0,
            object_proto: 0,
            ret: 0,
            ret_pc: 0,
            acc: ValueBits::UNDEFINED,
            depth: 0,
            limit: DEPTH_LIMIT,
            epoch: jit.bump(),
            miss: 0,
            stack_floor: jit.stack_floor(),
            array_proto: 0,
            indexed_cache: 0,
            string_proto: 0,
            chars: 0,
            empty: 0,
            args: [0; ARGS_WORDS],
            buf: [0; BUF_WORDS],
            entry: core::ptr::null_mut(),
            exe,
            scope: agent,
            jit,
            error: None,
        });
        c.refresh_ptrs(a);
        c
    }

    pub fn refresh_ptrs(&mut self, a: &Scope<'_, '_>) {
        let addr = |o: j8framework::JsObject| Value::Object(o).bits() & ValueBits::PAYLOAD;
        self.global = addr(a.context.global);
        self.script_scope = addr(a.context.script_scope);
        self.object_proto = addr(a.context.object_proto);
        self.array_proto = addr(a.context.array_proto);
        self.indexed_cache = a.context.indexed_cache_addr();
        self.string_proto = addr(a.context.string_proto);
        self.chars = a.context.chars.as_ptr() as usize as u64;
        self.empty = Value::String(a.context.empty).bits() & ValueBits::PAYLOAD;
    }

    pub unsafe fn parts<'a>(&mut self) -> (&'a Executor<'a>, &'a mut Scope<'a, 'a>) {
        unsafe { (&*self.exe.cast(), &mut *self.scope.cast()) }
    }

    pub unsafe fn entry<'a>(&mut self) -> &'a mut Entry<'a, 'a, 'a, 'a, 'a> {
        unsafe { &mut *self.entry.cast() }
    }

    fn jit<'a>(&self) -> &'a Jit {
        unsafe { &*self.jit }
    }

    fn fail(&mut self, e: Error) -> u64 {
        self.pending = 1;
        self.error = Some(e);
        0
    }
}

const _: () = assert!(core::mem::offset_of!(Ctx, args) == 144);
const _: () = assert!(core::mem::offset_of!(Ctx, buf) == 272);
const _: () = assert!(core::mem::offset_of!(Ctx, array_proto) == 104);
const _: () = assert!(core::mem::offset_of!(Ctx, indexed_cache) == 112);
const _: () = assert!(core::mem::offset_of!(Ctx, string_proto) == 120);
const _: () = assert!(core::mem::offset_of!(Ctx, chars) == 128);
const _: () = assert!(core::mem::offset_of!(Ctx, empty) == 136);
const _: () = assert!(Ctx::ARGS == 144 && Ctx::BUF == 272 && Ctx::STRING_PROTO == 120 && Ctx::CHARS == 128 && Ctx::EMPTY == 136);

unsafe fn val<'scope>(w: u64) -> Value<'scope> {
    unsafe { Value::from_bits(w) }
}

#[expect(clippy::cast_possible_truncation, reason = "an i32 sign-extended to 64 bits")]
unsafe fn boxed<'scope>(rep: Rep, bits: u64) -> Value<'scope> {
    match rep {
        Rep::I32 => Value::Number(f64::from(bits as i64 as i32)),
        Rep::F64 => Value::Number(f64::from_bits(bits)),
        Rep::Bool => Value::Bool(bits & 1 != 0),
        Rep::Ptr => unsafe { val(bits | OBJECT_TAG) },
        Rep::Ctx => unsafe { val(bits | CONTEXT_TAG) },
        Rep::Word | Rep::None => unsafe { val(bits) },
    }
}

sysv! {
    #[unsafe(no_mangle)]
    pub fn j8_helper(ctx: *mut Ctx, id: u64, a: u64, b: u64, c: u64, d: u64) -> u64 {
        dispatch(ctx, id, a, b, c, d)
    }
}

sysv! {
    #[unsafe(no_mangle)]
    pub fn j8_concat(ctx: *mut Ctx, _id: u64, a: u64, b: u64, _c: u64, _d: u64) -> u64 {
        concat_entry(ctx, a, b)
    }
}

fn concat_entry(ctx: *mut Ctx, a: u64, b: u64) -> u64 {
    let ctx = unsafe { &mut *ctx };
    let lhs = unsafe { val(a) };
    let rhs = unsafe { val(b) };
    let (bits, keep, err) = {
        let (_, agent) = unsafe { ctx.parts() };
        match agent.concat_add(lhs, rhs) {
            Ok((v, keep)) => (v.bits(), keep, None),
            Err(err) => (0, false, Some(err)),
        }
    };
    if !keep || err.is_some() {
        ctx.epoch = ctx.jit().bump();
    }
    match err {
        Some(err) => ctx.fail(err),
        None => bits,
    }
}

fn dispatch(ctx: *mut Ctx, id: u64, a: u64, b: u64, c: u64, d: u64) -> u64 {
    let ctx = unsafe { &mut *ctx };
    let jit = ctx.jit();
    let (h, n) = jit.call(id);
    let mut buf = [0u64; ARGS_WORDS];
    if n > 4 {
        buf[..n].copy_from_slice(&ctx.args[..n]);
    } else {
        buf[..4].copy_from_slice(&[a, b, c, d]);
    }
    let ops = &buf[..n.min(ARGS_WORDS)];
    let (exe, a) = unsafe { ctx.parts() };
    let mut keep = false;
    let r = helper(jit, exe, a, h, ops, ctx.epoch, &mut keep);
    if !keep && !matches!(h, Helper::ModF | Helper::ToBoolean | Helper::StoreCell | Helper::Alloc(_) | Helper::CreateClosure(..) | Helper::CreateContext(..) | Helper::ObjectLiteral(..) | Helper::Constant(..) | Helper::GlobalProbe(_)) {
        ctx.epoch = jit.bump();
    }
    if let (Helper::GlobalProbe(_), Ok(GLOBAL_MISS)) = (h, &r) {
        ctx.miss = 1;
        return 0;
    }
    match r {
        Ok(w) => w,
        Err(err) => ctx.fail(err),
    }
}

const GLOBAL_MISS: u64 = u64::MAX;

fn pack(out: j8framework::FastOut, v: Value<'_>) -> u64 {
    match out {
        j8framework::FastOut::Number => v.as_number().unwrap_or(f64::NAN).to_bits(),
        j8framework::FastOut::Bool => u64::from(v.bits() == ValueBits::TRUE),
        j8framework::FastOut::Value => v.bits(),
    }
}

fn direct<'scope>(a: &mut Scope<'scope, '_>, index: u32, out: j8framework::FastOut, ops: &[u64], keep: &mut bool) -> Result<u64, Error> {
    let v = |i: usize| unsafe { val::<'scope>(ops.get(i).copied().unwrap_or(ValueBits::UNDEFINED)) };
    let this = v(1);
    let callee = v(0).as_object().and_then(|o| o.native());
    let entry = a.context.fast_builtins.get().get(usize::try_from(index).unwrap_or(usize::MAX));
    let call = entry.filter(|e| callee.is_some_and(|n| core::ptr::fn_addr_eq(n.f, e.native))).map(|e| e.call);
    let got = match call {
        Some(j8framework::FastCall::A0(f)) if ops.len() == 2 => f(a, this),
        Some(j8framework::FastCall::A1(f)) if ops.len() == 3 => f(a, this, v(2)),
        Some(j8framework::FastCall::A2(f)) if ops.len() == 4 => f(a, this, v(2), v(3)),
        _ => None,
    };
    let value = match got {
        Some(value) => {
            *keep = true;
            value
        }
        None => {
            let mut args = [Value::Undefined; ARGS_WORDS];
            let n = ops.len().saturating_sub(2);
            for (i, x) in args.iter_mut().enumerate().take(n) {
                *x = v(2 + i);
            }
            a.call(v(0), this, &args[..n])?
        }
    };
    Ok(pack(out, value))
}

#[expect(clippy::cast_possible_truncation, reason = "the low 32 bits are the fast-table index")]
fn direct_parts(packed: u64) -> (u32, j8framework::FastOut) {
    let out = match packed >> 32 {
        1 => j8framework::FastOut::Number,
        2 => j8framework::FastOut::Bool,
        _ => j8framework::FastOut::Value,
    };
    (packed as u32, out)
}

fn fast_of<'scope>(a: &mut Scope<'scope, '_>, index: u32, callee: Value<'scope>) -> Option<j8framework::FastCall> {
    let native = callee.as_object().and_then(|o| o.native());
    let entry = a.context.fast_builtins.get().get(usize::try_from(index).unwrap_or(usize::MAX))?;
    native.is_some_and(|n| core::ptr::fn_addr_eq(n.f, entry.native)).then_some(entry.call)
}

#[cold]
#[inline(never)]
fn direct_miss(ctx: &mut Ctx, callee: u64, this: u64, args: &[u64], out: j8framework::FastOut) -> u64 {
    let r = {
        let (_, agent) = unsafe { ctx.parts() };
        let mut buf = [Value::Undefined; 2];
        for (i, w) in args.iter().enumerate() {
            buf[i] = unsafe { val(*w) };
        }
        agent.call(unsafe { val(callee) }, unsafe { val(this) }, &buf[..args.len()])
    };
    let epoch = ctx.jit().bump();
    ctx.epoch = epoch;
    match r {
        Ok(v) => pack(out, v),
        Err(e) => ctx.fail(e),
    }
}

fn direct_finish(hit: Option<Value<'_>>, ctx: &mut Ctx, callee: u64, this: u64, args: &[u64], out: j8framework::FastOut) -> u64 {
    match hit {
        Some(v) => pack(out, v),
        None => direct_miss(ctx, callee, this, args, out),
    }
}

sysv! {
    #[unsafe(no_mangle)]
    pub fn j8_direct0(ctx: *mut Ctx, callee: u64, this: u64, packed: u64) -> u64 {
        let ctx = unsafe { &mut *ctx };
        let (index, out) = direct_parts(packed);
        let hit = {
            let (_, agent) = unsafe { ctx.parts() };
            match fast_of(agent, index, unsafe { val(callee) }) {
                Some(j8framework::FastCall::A0(f)) => f(agent, unsafe { val(this) }),
                _ => None,
            }
        };
        direct_finish(hit, ctx, callee, this, &[], out)
    }
}

sysv! {
    #[unsafe(no_mangle)]
    pub fn j8_direct1(ctx: *mut Ctx, callee: u64, this: u64, arg: u64, packed: u64) -> u64 {
        let ctx = unsafe { &mut *ctx };
        let (index, out) = direct_parts(packed);
        let hit = {
            let (_, agent) = unsafe { ctx.parts() };
            match fast_of(agent, index, unsafe { val(callee) }) {
                Some(j8framework::FastCall::A1(f)) => f(agent, unsafe { val(this) }, unsafe { val(arg) }),
                _ => None,
            }
        };
        direct_finish(hit, ctx, callee, this, &[arg], out)
    }
}

sysv! {
    #[unsafe(no_mangle)]
    pub fn j8_direct2(ctx: *mut Ctx, callee: u64, this: u64, x: u64, y: u64, packed: u64) -> u64 {
        let ctx = unsafe { &mut *ctx };
        let (index, out) = direct_parts(packed);
        let hit = {
            let (_, agent) = unsafe { ctx.parts() };
            match fast_of(agent, index, unsafe { val(callee) }) {
                Some(j8framework::FastCall::A2(f)) => f(agent, unsafe { val(this) }, unsafe { val(x) }, unsafe { val(y) }),
                _ => None,
            }
        };
        direct_finish(hit, ctx, callee, this, &[x, y], out)
    }
}

fn helper<'scope>(jit: &Jit, exe: &Executor, a: &mut Scope<'scope, '_>, h: Helper, ops: &[u64], epoch: u64, keep: &mut bool) -> Result<u64, Error> {
    let v = |i: usize| unsafe { val::<'scope>(ops.get(i).copied().unwrap_or(ValueBits::UNDEFINED)) };
    Ok(match h {
        Helper::Call => {
            let mut args = [Value::Undefined; ARGS_WORDS];
            let n = ops.len().saturating_sub(2);
            for (i, x) in args.iter_mut().enumerate().take(n) {
                *x = v(2 + i);
            }
            a.call(v(0), v(1), &args[..n])?.bits()
        }
        Helper::Construct => {
            let args: Vec<Value<'scope>> = (2..ops.len()).map(v).collect();
            a.construct(v(0), &args, v(1))?.bits()
        }
        Helper::GetNamed(f, k) => {
            let name = exe.jit_function(f).name(i64::from(k));
            a.get_named(v(0), name)?.bits()
        }
        Helper::SetNamed(f, k, assign) => {
            let name = exe.jit_function(f).name(i64::from(k));
            match (assign, v(0).as_object()) {
                (false, Some(o)) => a.define(o, name, j8framework::Property::data(v(1)))?,
                _ => a.put_named(v(0), name, v(1), exe.jit_function(f).strict())?,
            }
            0
        }
        Helper::CreateClosure(f, k) => Value::Object(exe.jit_create_closure(a, f, i64::from(k), v(0).as_context())).bits(),
        Helper::CreateContext(shape, n) => Value::context(j8framework::Env::context(n as usize, Value::Undefined, v(0).as_context(), Some(shape), None).alloc(a.mc)).bits(),
        Helper::ObjectLiteral(f, k) => Value::Object(exe.jit_object_literal(a, f, i64::from(k))).bits(),
        Helper::RegExpLiteral(f, pat, bits) => {
            let (v, keep_epoch) = exe.jit_regexp_literal(a, f, i64::from(pat), i64::from(bits))?;
            *keep = keep_epoch;
            v.bits()
        }
        Helper::Constant(f, k) => match exe.jit_function(f).constant(i64::from(k)) {
            Some(j8bytecode::Constant::String(s)) => Value::String(a.constant_string(f.0, k, s)).bits(),
            Some(j8bytecode::Constant::Utf16(u)) => Value::String(a.constant_units(f.0, k, u)).bits(),
            _ => ValueBits::UNDEFINED,
        },
        Helper::Concat => {
            let (value, keep_epoch) = a.concat_add(v(0), v(1))?;
            *keep = keep_epoch;
            value.bits()
        },
        Helper::ModF => (f64::from_bits(ops[0]) % f64::from_bits(ops[1])).to_bits(),
        Helper::ToBoolean => u64::from(a.to_boolean(v(0))),
        Helper::StoreCell => {
            let cell = unsafe { &*(ops[0] as usize as *const j8framework::ValueCell<'scope>) };
            cell.set(a.mc, v(1));
            0
        }
        Helper::LoadGlobal(f, k, slot, inside_typeof) => exe.jit_global_load(a, f, i64::from(k), i64::from(slot), inside_typeof)?.bits(),
        Helper::GlobalProbe(k) => {
            let (f, name, slot, expect) = jit.site(k);
            let v = exe.jit_global_load(a, f, i64::from(name), i64::from(slot), false)?;
            if let Some(want) = expect
                && v.as_object().and_then(|o| o.closure()).is_none_or(|c| c.func != want)
            {
                return Ok(GLOBAL_MISS);
            }
            unsafe {
                let cache = ops[0] as usize as *mut u64;
                cache.write(epoch);
                cache.add(1).write(v.bits());
            }
            v.bits()
        }
        Helper::Binary(op) => a.binary(op, v(0), v(1))?.bits(),
        Helper::GetKeyed => a.get_keyed(v(0), v(1))?.bits(),
        Helper::SetKeyed(strict) => {
            a.put_keyed(v(0), v(1), v(2), strict)?;
            0
        }
        Helper::Unary(op) => a.unary(op, v(0))?.bits(),
        Helper::LooseEquals => u64::from(a.loose_equals(v(0), v(1))?),
        Helper::InstanceOf => u64::from(a.instance_of(v(0), v(1))?),
        Helper::Compare(op) => u64::from(a.compare(op, v(0), v(1))?),
        Helper::StrictEquals => u64::from(a.compare(j8framework::CompareOp::StrictEq, v(0), v(1))?),
        Helper::Direct(index, out) => return direct(a, index, out, ops, keep),
        Helper::InlinePush(index) => return direct(a, index, j8framework::FastOut::Number, ops, keep),
        Helper::Alloc(k) => {
            let shape = jit.shape(k);
            let proto = unsafe { val::<'scope>(ops[0] | OBJECT_TAG) }.as_object();
            let o = ObjectData::ordinary(a.mc, proto, ops.len() - 1);
            if let Some(s) = &shape {
                o.fill(a.mc, unsafe { s.handle() }, (1..ops.len()).map(|i| PropSlot::Data(v(i))));
            }
            Value::Object(o).bits() & ValueBits::PAYLOAD
        }
    })
}

sysv! {
    #[unsafe(no_mangle)]
    pub fn j8_poll(ctx: *mut Ctx) -> u64 {
        poll(ctx)
    }
}

fn poll(ctx: *mut Ctx) -> u64 {
    let ctx = unsafe { &mut *ctx };
    ctx.budget = BUDGET_CHUNK;
    if ctx.entry.is_null() {
        let (_, a) = unsafe { ctx.parts() };
        return u64::from(a.stop.is_set());
    }
    let e = unsafe { ctx.entry() };
    u64::from(e.spend(BUDGET_CHUNK as usize))
}

sysv! {
    #[unsafe(no_mangle)]
    pub fn j8_deopt(ctx: *mut Ctx, id: u64) -> Ret {
        Ret { value: deopt(ctx, id), int: 0 }
    }
}

#[repr(C)]
pub struct Ret {
    pub value: u64,
    pub int: u64,
}

impl Ret {
    pub const INT: u64 = 1 << 32;
}

fn deopt(ctx: *mut Ctx, id: u64) -> u64 {
    let ctx = unsafe { &mut *ctx };
    let jit = ctx.jit();
    let d = jit.deopt(id);
    jit.learn(&d);
    let (exe, a) = unsafe { ctx.parts() };
    let frames = frames(a, ctx, &d);
    ctx.epoch = jit.bump();
    let throw = if d.reason == Reason::Throw { ctx.error.take() } else { None };
    if d.native {
        ctx.pending = 0;
        let thrown = match throw {
            Some(Error::Exception) => Some(a.take_exception()),
            Some(other) => return ctx.fail(other),
            None => None,
        };
        return match exe.resume_frames(a, frames, thrown) {
            Ok(v) => v.bits(),
            Err(err) => ctx.fail(err),
        };
    }
    let e = unsafe { ctx.entry() };
    e.resume(frames);
    match throw {
        Some(err) => {
            ctx.error = Some(err);
            THREW as u64
        }
        None => RESUMED as u64,
    }
}

fn frames<'scope>(a: &mut Scope<'scope, '_>, ctx: &Ctx, d: &crate::Deopt) -> Vec<DeoptFrame<'scope>> {
    let st = d.state();
    let mut objs: Vec<Option<Value<'scope>>> = alloc::vec![None; st.objects.len()];
    let get = |v: u32| -> Value<'scope> {
        let (rep, src) = d.value(v);
        let bits = match src {
            crate::Src::Buf(i) => ctx.buf[i as usize],
            crate::Src::Const(b) => b,
        };
        unsafe { boxed(rep, bits) }
    };
    let cell = RefCell::new(&mut objs);
    let slot = |s: &Slot| -> Value<'scope> {
        match s {
            Slot::Value(v) => get(*v),
            Slot::Virtual(i) => cell.borrow()[*i as usize].unwrap_or(Value::Undefined),
            Slot::Closure => Value::Uninit,
        }
    };
    for i in (0..st.objects.len()).rev() {
        let o = &st.objects[i];
        let proto = get(o.proto).as_object();
        let fields: Vec<Value<'scope>> = o.fields.iter().map(slot).collect();
        let ob = ObjectData::ordinary(a.mc, proto, fields.len());
        if let Some(s) = &o.shape {
            ob.fill(a.mc, unsafe { s.handle() }, fields.into_iter().map(PropSlot::Data));
        }
        cell.borrow_mut()[i] = Some(Value::Object(ob));
    }
    for (c, idx, s) in &st.writes {
        let env = get(*c);
        let env = unsafe { val::<'scope>((env.bits() & ValueBits::PAYLOAD) | CONTEXT_TAG) }.as_context();
        if let Some(env) = env {
            j8framework::Env::set(env, a.mc, *idx as usize, slot(s));
        }
    }
    st.frames.iter().map(|f| DeoptFrame {
        func: f.func,
        callee: slot(&f.callee),
        params: f.params.iter().map(slot).collect(),
        argc: if f.argc == u32::MAX { f.params.len().saturating_sub(1) } else { f.argc as usize },
        regs: f.regs.iter().map(slot).collect(),
        context: {
            let c = slot(&f.context);
            if c.as_context().is_some() || c.bits() == ValueBits::UNDEFINED || c.bits() == ValueBits::UNINIT { c } else {
                unsafe { val::<'scope>((c.bits() & ValueBits::PAYLOAD) | CONTEXT_TAG) }
            }
        },
        acc: slot(&f.acc),
        pc: f.pc as usize,
        construct: f.construct.as_ref().map(|(n, t)| (slot(n), slot(t))),
    }).collect()
}

sysv! {
    #[unsafe(no_mangle)]
    pub fn j8_interp(ctx: *mut Ctx, callee: u64, this: u64, a0: u64, a1: u64, a2: u64) -> Ret {
        Ret { value: interp(ctx, callee, this, a0, a1, a2), int: 0 }
    }
}

fn interp(ctx: *mut Ctx, callee: u64, this: u64, a0: u64, a1: u64, a2: u64) -> u64 {
    let ctx = unsafe { &mut *ctx };
    let jit = ctx.jit();
    let (exe, a) = unsafe { ctx.parts() };
    let (f, t, args) = unsafe { (val(callee), val(this), [val(a0), val(a1), val(a2)]) };
    if let Some(c) = f.as_object().and_then(|o| o.closure()) {
        jit.native_hot(exe, a, c.func);
    }
    let n = f.as_object().and_then(|o| o.closure()).map_or(3, |c| exe.jit_function(c.func).shared().formal_parameters as usize).min(3);
    let r = a.call(f, t, &args[..n]);
    ctx.epoch = jit.bump();
    match r {
        Ok(v) => v.bits(),
        Err(err) => ctx.fail(err),
    }
}

#[derive(Default)]
pub struct Arena {
    chunks: Vec<(Mapped, Mapped)>,
    used: usize,
}

impl Arena {
    const CHUNK: usize = 1 << 20;
    const ALIGN: usize = 16;

    #[cfg(target_os = "linux")]
    fn chunk(len: usize) -> Option<(Mapped, Mapped)> {
        unsafe {
            let fd = libc::memfd_create(c"j8jit".as_ptr(), libc::MFD_CLOEXEC);
            if fd < 0 {
                return None;
            }
            let ok = libc::ftruncate(fd, libc::off_t::try_from(len).ok()?) == 0;
            let map = |prot| libc::mmap(core::ptr::null_mut(), len, prot, libc::MAP_SHARED, fd, 0);
            let (w, x) = if ok { (map(libc::PROT_READ | libc::PROT_WRITE), map(libc::PROT_READ | libc::PROT_EXEC)) } else { (libc::MAP_FAILED, libc::MAP_FAILED) };
            libc::close(fd);
            if w == libc::MAP_FAILED || x == libc::MAP_FAILED {
                return None;
            }
            Some((Mapped { at: w.cast(), len }, Mapped { at: x.cast(), len }))
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn chunk(len: usize) -> Option<(Mapped, Mapped)> {
        let mut mem = region::alloc(len, region::Protection::READ_WRITE_EXECUTE).ok()?;
        let at = mem.as_mut_ptr::<u8>();
        let len = mem.len();
        core::mem::forget(mem);
        Some((Mapped { at, len }, Mapped { at, len }))
    }

    pub fn place(&mut self, bytes: &[u8], patch: impl FnOnce(&mut [u8], usize)) -> Option<usize> {
        let len = bytes.len().max(1);
        let at = self.used.next_multiple_of(Self::ALIGN);
        if self.chunks.last().is_none_or(|(w, _)| at + len > w.len) {
            self.chunks.push(Self::chunk(len.next_multiple_of(Self::CHUNK))?);
            self.used = 0;
            return self.place(bytes, patch);
        }
        let (w, x) = self.chunks.last()?;
        let buf = unsafe { core::slice::from_raw_parts_mut(w.at.add(at), len) };
        let addr = x.at as usize + at;
        Code::writable(|| {
            buf[..bytes.len()].copy_from_slice(bytes);
            patch(buf, addr);
        });
        Code::sync(addr, len);
        self.used = at + len;
        Some(addr)
    }
}

pub struct Mapped {
    pub at: *mut u8,
    pub len: usize,
}

pub struct Trampolines {
    mem: region::Allocation,
    used: usize,
}

impl Trampolines {
    const PAGE: usize = 4096;
    const STUB: usize = 8;
    pub const PER_PAGE: usize = Self::PAGE / Self::STUB;

    pub fn new() -> Option<Self> {
        let mut mem = region::alloc(2 * Self::PAGE, Code::STUBS).ok()?;
        let at = mem.as_mut_ptr::<u8>();
        Code::writable(|| unsafe {
            for i in 0..Self::PER_PAGE {
                let stub = at.add(i * Self::STUB);
                let cell = at.add(Self::PAGE + i * 8);
                let code = Code::jump_through(stub as usize, cell as usize);
                core::ptr::copy_nonoverlapping(code.as_ptr(), stub, Self::STUB);
            }
        });
        if Code::STUBS != region::Protection::READ_WRITE_EXECUTE && unsafe { region::protect(at, Self::PAGE, region::Protection::READ_EXECUTE) }.is_err() {
            return None;
        }
        Code::sync(at as usize, Self::PAGE);
        Some(Trampolines { mem, used: 0 })
    }

    fn base(&self) -> *mut u8 {
        self.mem.as_ptr::<u8>().cast_mut()
    }

    pub fn take(&mut self, target: usize) -> Option<usize> {
        if self.used == Self::PER_PAGE {
            return None;
        }
        let i = self.used;
        self.used += 1;
        self.set(i, target);
        Some(self.base() as usize + i * Self::STUB)
    }

    pub fn set_at(&self, stub: usize, target: usize) {
        let i = (stub - self.base() as usize) / Self::STUB;
        self.set(i, target);
    }

    fn set(&self, i: usize, target: usize) {
        Code::writable(|| unsafe { core::ptr::write_volatile(self.base().add(Self::PAGE + i * 8).cast::<usize>(), target) });
    }

    pub fn owns(&self, stub: usize) -> bool {
        stub >= self.base() as usize && stub < self.base() as usize + Self::PAGE
    }
}

struct Code;

impl Code {
    #[cfg(all(target_vendor = "apple", target_arch = "aarch64"))]
    const STUBS: region::Protection = region::Protection::READ_WRITE_EXECUTE;
    #[cfg(not(all(target_vendor = "apple", target_arch = "aarch64")))]
    const STUBS: region::Protection = region::Protection::READ_WRITE;

    #[cfg(all(target_vendor = "apple", target_arch = "aarch64"))]
    fn writable<T>(f: impl FnOnce() -> T) -> T {
        const WRITE: libc::c_int = 0;
        const EXECUTE: libc::c_int = 1;
        unsafe { libc::pthread_jit_write_protect_np(WRITE) };
        let r = f();
        unsafe { libc::pthread_jit_write_protect_np(EXECUTE) };
        r
    }

    #[cfg(not(all(target_vendor = "apple", target_arch = "aarch64")))]
    fn writable<T>(f: impl FnOnce() -> T) -> T {
        f()
    }

    fn sync(run: usize, len: usize) {
        let start = run as *const u8;
        unsafe { clear_cache::clear_cache(start, start.wrapping_add(len)) };
    }
}

#[cfg(not(target_arch = "aarch64"))]
impl Code {
    fn jump_through(stub: usize, cell: usize) -> [u8; Trampolines::STUB] {
        let disp = i32::try_from(cell.wrapping_sub(stub + 6).cast_signed()).unwrap_or(0);
        let mut code = [0xff, 0x25, 0, 0, 0, 0, 0xcc, 0xcc];
        code[2..6].copy_from_slice(&disp.to_le_bytes());
        code
    }
}

#[cfg(target_arch = "aarch64")]
impl Code {
    const LDR_X16: u32 = 0x5800_0010;
    const BR_X16: u32 = 0xD61F_0200;
    const WORD: usize = 4;

    fn jump_through(stub: usize, cell: usize) -> [u8; Trampolines::STUB] {
        let words = u32::try_from((cell - stub) / Self::WORD).unwrap_or(0);
        let mut code = [0; Trampolines::STUB];
        code[..4].copy_from_slice(&(Self::LDR_X16 | words << 5).to_le_bytes());
        code[4..].copy_from_slice(&Self::BR_X16.to_le_bytes());
        code
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThreadStackLimits(low: *mut usize, high: *mut usize);
}

#[cfg(windows)]
pub(crate) fn stack_low() -> u64 {
    let mut low = 0usize;
    let mut high = 0usize;
    unsafe { GetCurrentThreadStackLimits(&raw mut low, &raw mut high) };
    u64::try_from(low).unwrap_or(u64::MAX)
}
