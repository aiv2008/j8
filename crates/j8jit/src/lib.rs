extern crate alloc;

#[cfg(all(windows, target_arch = "x86_64"))]
#[macro_export]
#[doc(hidden)]
macro_rules! sysv {
    ($(#[$m:meta])* $v:vis fn $name:ident($($p:tt)*) $(-> $ret:ty)? $body:block) => { $(#[$m])* $v extern "sysv64" fn $name($($p)*) $(-> $ret)? $body };
    (unsafe fn($($p:ty),* $(,)?) -> $ret:ty) => { unsafe extern "sysv64" fn($($p),*) -> $ret };
    (fn($($p:ty),* $(,)?) -> $ret:ty) => { extern "sysv64" fn($($p),*) -> $ret };
}

#[cfg(not(all(windows, target_arch = "x86_64")))]
#[macro_export]
#[doc(hidden)]
macro_rules! sysv {
    ($(#[$m:meta])* $v:vis fn $name:ident($($p:tt)*) $(-> $ret:ty)? $body:block) => { $(#[$m])* $v extern "C" fn $name($($p)*) $(-> $ret)? $body };
    (unsafe fn($($p:ty),* $(,)?) -> $ret:ty) => { unsafe extern "C" fn($($p),*) -> $ret };
    (fn($($p:ty),* $(,)?) -> $ret:ty) => { extern "C" fn($($p),*) -> $ret };
}

mod aarch64;
mod build;
mod asm;
mod cfg;
mod codegen;
mod compile;
mod helper;
mod ir;
mod live;
mod stitch;
mod opt;
mod runtime;
mod x86_64;

pub use build::Kind;
pub use compile::{Backend, CompileError, Compiler, MachineCode};
pub use helper::Helper as NumberHelper;
pub use aarch64::AArch64Emit;
pub use x86_64::{X86_64Code, X86_64Emit, X86_64Reloc};

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;

use j8executor::{Entry, Tier, Tiered};
use j8framework::{FuncId, ShapeKey, Value};

use build::{Builder, Hints, HINT_FLOAT, HINT_GENERIC, HINT_NO_INLINE};
use ir::{FrameState, Helper, Op, Rep, V};
use codegen::{Reason, RESUMED, RETURNED, THREW};
use runtime::{Arena, Ctx, Trampolines};

#[derive(Clone, Copy, Debug)]
pub enum Src {
    Buf(u32),
    Const(u64),
}

pub struct Meta {
    states: Vec<FrameState>,
    reps: Vec<Rep>,
    consts: Vec<Option<u64>>,
}

pub struct Deopt {
    meta: Rc<Meta>,
    fs: u32,
    slots: Vec<(V, u32)>,
    reason: Reason,
    op: u32,
    hint_at: Option<(FuncId, u32)>,
    native: bool,
    root: FuncId,
    kind: Kind,
}

impl Deopt {
    fn state(&self) -> &FrameState {
        &self.meta.states[self.fs as usize]
    }

    fn value(&self, v: V) -> (Rep, Src) {
        let rep = self.meta.reps.get(v as usize).copied().unwrap_or(Rep::Word);
        match self.slots.iter().find(|(x, _)| *x == v) {
            Some((_, i)) => (rep, Src::Buf(*i)),
            None => (rep, Src::Const(self.meta.consts.get(v as usize).copied().flatten().unwrap_or(j8framework::ValueBits::UNDEFINED))),
        }
    }
}

#[derive(Default)]
struct Func {
    enter: Option<usize>,
    osr: BTreeMap<usize, usize>,
    native: Option<usize>,
    stub: usize,
    native_calls: u32,
    deopts: u32,
    never: bool,
    native_never: bool,
}

struct Site {
    func: FuncId,
    name: u32,
    slot: u32,
    expect: Option<FuncId>,
    #[expect(dead_code, reason = "owned for its address only")]
    cache: Box<[u64; 2]>,
}

#[derive(Default)]
struct State {
    sites: Vec<Site>,
    funcs: Vec<Func>,
    deopts: Vec<Rc<Deopt>>,
    calls: Vec<(Helper, usize)>,
    shapes: Vec<Option<ShapeKey>>,
    code: Arena,
    tramps: Vec<Trampolines>,
    hints: Hints,
}

pub struct Jit {
    threshold: u32,
    debug: Debug,
    ctx: core::cell::Cell<Option<Box<Ctx>>>,
    ctxs: RefCell<Vec<Box<Ctx>>>,
    floor: core::cell::Cell<u64>,
    epoch: core::cell::Cell<u64>,
    entries: core::cell::Cell<u64>,
    st: RefCell<State>,
    stencils: core::cell::OnceCell<Option<stitch::Table>>,
}

impl Default for Jit {
    fn default() -> Self {
        Self::from(Jit::THRESHOLD)
    }
}

impl From<u32> for Jit {
    fn from(threshold: u32) -> Self {
        Self { threshold, debug: Debug::from_env(), ctx: core::cell::Cell::new(None), ctxs: RefCell::new(Vec::new()), floor: core::cell::Cell::new(0), epoch: core::cell::Cell::new(1), entries: core::cell::Cell::new(0), st: RefCell::new(State::default()), stencils: core::cell::OnceCell::new() }
    }
}

struct Debug {
    log: bool,
    ir: bool,
    mlir: bool,
    bin: Option<std::ffi::OsString>,
}

impl Debug {
    fn from_env() -> Self {
        Debug { log: std::env::var_os("J8_JIT_LOG").is_some(), ir: std::env::var_os("J8_JIT_IR").is_some(), mlir: std::env::var_os("J8_JIT_MLIR").is_some(), bin: std::env::var_os("J8_JIT_BIN") }
    }
}

const MAX_DEOPTS: u32 = 24;

struct Prepared {
    ctx: Box<Ctx>,
    seen_epoch: u64,
    seen_entries: u64,
    generation: u64,
}

type EntryFn = sysv!(unsafe fn(*mut Ctx, *mut Value<'static>, *mut Value<'static>) -> i64);

impl Jit {
    pub const THRESHOLD: u32 = 64;

    pub fn threshold(&self) -> u32 {
        self.threshold
    }

    fn call(&self, id: u64) -> (Helper, usize) {
        self.st.borrow().calls[usize::try_from(id).unwrap_or(0)]
    }

    fn spare_ctx(&self) -> Option<Box<Ctx>> {
        self.ctx.take().or_else(|| self.ctxs.borrow_mut().pop())
    }

    fn recycle(&self, ctx: Box<Ctx>) {
        if let Some(prev) = self.ctx.replace(Some(ctx)) {
            let mut v = self.ctxs.borrow_mut();
            if v.len() < 8 {
                v.push(prev);
            }
        }
    }

    fn watch_realm(ctx: &mut Ctx, a: &j8framework::Scope<'_, '_>) {
        ctx.refresh_ptrs(a);
    }

    unsafe fn release_callback(env: *mut ()) {
        let prep = unsafe { Box::from_raw(env.cast::<Prepared>()) };
        let jit = unsafe { &*prep.ctx.jit };
        jit.recycle(prep.ctx);
    }

    #[cold]
    #[inline(never)]
    fn refresh_callback(prep: &mut Prepared, a: &j8framework::Scope<'_, '_>, jit: &Jit) {
        let generation = a.mc.generation();
        if generation != prep.generation {
            Self::watch_realm(&mut prep.ctx, a);
            prep.generation = generation;
        }
        prep.ctx.epoch = jit.bump();
    }

    #[cold]
    #[inline(never)]
    fn callback_error(ctx: &mut Ctx) -> j8framework::Error {
        ctx.error.take().unwrap_or(j8framework::Error::Terminated)
    }

    #[expect(clippy::unnecessary_wraps, reason = "the callback hook declines a call by returning None")]
    unsafe fn run_callback<'scope>(a: &mut j8framework::Scope<'scope, '_>, cb: &j8framework::Callback<'scope>, args: &[Value<'scope>]) -> Option<Result<Value<'scope>, j8framework::Error>> {
        type StubFn = sysv!(unsafe fn(*mut Ctx, u64, u64, u64, u64, u64) -> runtime::Ret);
        let prep = unsafe { &mut *cb.env.cast::<Prepared>() };
        let jit = unsafe { &*prep.ctx.jit };
        if !cb.quiet.get() || jit.epoch.get() != prep.seen_epoch || jit.entries.get() != prep.seen_entries || a.mc.generation() != prep.generation {
            Self::refresh_callback(prep, a, jit);
        }
        let ctx = &mut prep.ctx;
        ctx.scope = core::ptr::from_mut(a).cast();
        ctx.budget = runtime::BUDGET_CHUNK;
        ctx.pending = 0;
        ctx.miss = 0;
        let undef = j8framework::ValueBits::UNDEFINED;
        let n = cb.arity.min(args.len());
        let w0 = if n > 0 { args[0].bits() } else { undef };
        let w1 = if n > 1 { args[1].bits() } else { undef };
        let w2 = if n > 2 { args[2].bits() } else { undef };
        let r = unsafe {
            let code: StubFn = core::mem::transmute::<usize, StubFn>(cb.code);
            code(core::ptr::from_mut(&mut **ctx), cb.callee.bits(), cb.this.bits(), w0, w1, w2).value
        };
        prep.seen_epoch = jit.epoch.get();
        prep.seen_entries = jit.entries.get();
        if prep.ctx.pending != 0 {
            return Some(Err(Self::callback_error(&mut prep.ctx)));
        }
        Some(Ok(unsafe { Value::from_bits(r) }))
    }

    fn stack_floor(&self) -> u64 {
        if self.floor.get() == 0 {
            let here = 0u8;
            let at = core::ptr::from_ref(&here) as u64;
            #[cfg(not(windows))]
            let floor = at.saturating_sub(runtime::NATIVE_STACK);
            #[cfg(windows)]
            let floor = at.saturating_sub(runtime::NATIVE_STACK).max(runtime::stack_low().saturating_add(runtime::STACK_MARGIN));
            self.floor.set(floor);
        }
        self.floor.get()
    }

    fn stack_room(&self) -> bool {
        let here = 0u8;
        let at = core::ptr::from_ref(&here) as u64;
        at > self.stack_floor() + runtime::NATIVE_STACK / 4
    }

    fn bump(&self) -> u64 {
        let e = self.epoch.get() + 1;
        self.epoch.set(e);
        e
    }

    fn site(&self, k: u32) -> (FuncId, u32, u32, Option<FuncId>) {
        let st = self.st.borrow();
        let s = &st.sites[k as usize];
        (s.func, s.name, s.slot, s.expect)
    }

    fn shape(&self, k: u32) -> Option<ShapeKey> {
        self.st.borrow().shapes.get(k as usize).copied().flatten()
    }

    fn deopt(&self, id: u64) -> Rc<Deopt> {
        self.st.borrow().deopts[usize::try_from(id).unwrap_or(0)].clone()
    }

    fn func<'s>(st: &'s mut State, f: FuncId) -> &'s mut Func {
        let i = f.0 as usize;
        if st.funcs.len() <= i {
            st.funcs.resize_with(i + 1, Func::default);
        }
        &mut st.funcs[i]
    }

    fn learn(&self, d: &Deopt) {
        let Some(fr) = d.state().frames.last() else { return };
        if self.debug.log {
            std::eprintln!("j8jit: deopt {:?} {:?} at {:?}@{} ({:?}, v{})", d.root, d.kind, fr.func, fr.pc, d.reason, d.op);
        }
        let mut key = d.hint_at.map_or((fr.func.0, fr.pc), |(f, pc)| (f.0, pc));
        let bits = match d.reason {
            Reason::Int => HINT_FLOAT,
            Reason::Number | Reason::Shape => HINT_GENERIC,
            Reason::Func => HINT_NO_INLINE | HINT_GENERIC,
            Reason::Poll | Reason::Throw | Reason::Hole | Reason::Depth => return,
            Reason::Always => {
                if let Kind::Osr(_) = d.kind {
                    return;
                }
                let frames = &d.state().frames;
                match frames.len().checked_sub(2).and_then(|i| frames.get(i)) {
                    Some(caller) => {
                        key = (caller.func.0, caller.pc.saturating_sub(1));
                        HINT_NO_INLINE
                    }
                    None => {
                        let mut st = self.st.borrow_mut();
                        let f = Self::func(&mut st, d.root);
                        f.never = true;
                        f.native_never = true;
                        0
                    }
                }
            }
        };
        let mut st = self.st.borrow_mut();
        *st.hints.entry(key).or_default() |= bits;
        let native_stub;
        {
            let f = Self::func(&mut st, d.root);
            f.deopts += 1;
            if f.deopts >= MAX_DEOPTS {
                f.never = true;
                f.native_never = true;
            }
            match d.kind {
                Kind::Enter => f.enter = None,
                Kind::Osr(at) => {
                    f.osr.remove(&at);
                }
                Kind::Native(_) => {
                    f.native = None;
                    f.native_calls = 0;
                }
            }
            native_stub = (matches!(d.kind, Kind::Native(_)), f.stub);
        }
        if native_stub.0 && native_stub.1 != 0 {
            Self::point(&st, native_stub.1, runtime::j8_interp as *const () as usize);
        }
    }

    fn point(st: &State, stub: usize, target: usize) {
        if let Some(t) = st.tramps.iter().find(|t| t.owns(stub)) {
            t.set_at(stub, target);
        }
    }

    fn stub(st: &mut State, f: FuncId) -> usize {
        let s = Self::func(st, f).stub;
        if s != 0 {
            return s;
        }
        let interp = runtime::j8_interp as *const () as usize;
        let mut s = st.tramps.last_mut().and_then(|t| t.take(interp));
        if s.is_none() {
            if let Some(mut t) = Trampolines::new() {
                s = t.take(interp);
                st.tramps.push(t);
            }
        }
        let s = s.unwrap_or(0);
        Self::func(st, f).stub = s;
        s
    }

    fn native_hot(&self, exe: &j8executor::Executor, a: &mut j8framework::Scope<'_, '_>, f: FuncId) {
        {
            let mut st = self.st.borrow_mut();
            let fs = Self::func(&mut st, f);
            if fs.native_never || fs.native.is_some() {
                return;
            }
            fs.native_calls += 1;
            if fs.native_calls < self.threshold {
                return;
            }
        }
        let formals = usize::from(exe.jit_function(f).shared().formal_parameters).min(3);
        match self.compile(exe, a, f, Kind::Native(formals)) {
            Some(addr) => {
                let mut st = self.st.borrow_mut();
                Self::func(&mut st, f).native = Some(addr);
                let stub = Self::stub(&mut st, f);
                Self::point(&st, stub, addr);
            }
            None => Self::func(&mut self.st.borrow_mut(), f).native_never = true,
        }
    }

    fn compile(&self, exe: &j8executor::Executor, a: &mut j8framework::Scope<'_, '_>, f: FuncId, kind: Kind) -> Option<usize> {
        if self.debug.log {
            std::eprintln!("j8jit: compile {f:?} {kind:?}");
        }
        let t0 = std::time::Instant::now();
        let st = self.st.borrow();
        let hints = &st.hints;
        let (global, scope) = (a.context.global, a.context.script_scope);
        let paths = move |g: bool, i: usize| if g { global.get().props().store_path(i) } else { scope.get().props().store_path(i) };
        let directs = build::directs_from(a);
        let built = Builder::build(exe, hints, f, kind, &paths, &directs, self.debug.log).ok();
        drop(st);
        let built = built?;
        let (deopt_base, helper_base) = {
            let st = self.st.borrow();
            (u32::try_from(st.deopts.len()).ok()?, u32::try_from(st.calls.len()).ok()?)
        };
        let t_build = t0.elapsed();
        if self.debug.ir {
            std::eprintln!("== {f:?} {kind:?}\n{}", built.graph.dump());
        }
        let mut graph = built.graph;
        let (site_base, addrs) = {
            let mut st = self.st.borrow_mut();
            let base = u32::try_from(st.sites.len()).ok()?;
            let mut expects = alloc::vec![None; graph.globals.len()];
            for inst in &mut graph.insts {
                if let Op::Global(k, e, _) = &mut inst.op {
                    expects[*k as usize] = *e;
                    *k += base;
                }
            }
            let mut addrs = Vec::new();
            for (i, (f, name, slot)) in graph.globals.iter().enumerate() {
                let cache = Box::new([0u64; 2]);
                addrs.push(cache.as_ptr() as usize as u64);
                st.sites.push(Site { func: *f, name: *name, slot: *slot, expect: expects[i], cache });
            }
            (base, addrs)
        };
        let mut all_addrs = alloc::vec![0u64; site_base as usize];
        all_addrs.extend(addrs);
        let mut built = build::Built { graph, ..built };
        let generated = self.through_mlir(&built.graph, kind, deopt_base, helper_base, &all_addrs).unwrap_or_else(|| codegen::Gen::generate(&built.graph, kind, deopt_base, helper_base, &all_addrs));
        if self.debug.log {
            std::eprintln!("j8jit: {f:?} {kind:?}: {} insts, {} bytes, build {t_build:?} total {:?}", built.graph.insts.len(), generated.bytes.len(), t0.elapsed());
        }
        if let Some(dir) = &self.debug.bin {
            let mut path = std::path::PathBuf::from(dir);
            path.push(alloc::format!("f{}-{kind:?}.bin", f.0));
            let _ = std::fs::write(path, &generated.bytes);
        }
        let mut st = self.st.borrow_mut();
        let mut targets = Vec::new();
        for t in &built.targets {
            targets.push(Self::stub(&mut st, *t));
        }
        let sym = |s: codegen::Sym| -> usize {
            match s {
                codegen::Sym::Deopt => runtime::j8_deopt as *const () as usize,
                codegen::Sym::Poll => runtime::j8_poll as *const () as usize,
                codegen::Sym::Helper => runtime::j8_helper as *const () as usize,
                codegen::Sym::Concat => runtime::j8_concat as *const () as usize,
                codegen::Sym::Direct(0) => runtime::j8_direct0 as *const () as usize,
                codegen::Sym::Direct(1) => runtime::j8_direct1 as *const () as usize,
                codegen::Sym::Direct(_) => runtime::j8_direct2 as *const () as usize,
                codegen::Sym::Target(k) => targets[k as usize],
            }
        };
        let addr = st.code.place(&generated.bytes, |buf, _| {
            for (off, s) in &generated.relocs {
                buf[*off..*off + 8].copy_from_slice(&(sym(*s) as u64).to_le_bytes());
            }
        })?;
        let lowered = generated;
        let shape_base = u32::try_from(st.shapes.len()).ok()?;
        st.shapes.extend(built.shapes.iter().copied());
        exe.jit_keep(f, &built.keep.iter().chain(built.shapes.iter().flatten()).copied().collect::<Vec<ShapeKey>>(), &[&built.codes[..], &built.targets[..]].concat());
        let arity = &lowered.arity;
        for (i, h) in lowered.calls.iter().enumerate() {
            let h = match h {
                Helper::Alloc(k) => Helper::Alloc(k + shape_base),
                h => *h,
            };
            st.calls.push((h, arity[i]));
        }
        let native = matches!(kind, Kind::Native(_));
        let consts = built.graph.insts.iter().map(|i| match &i.op {
            Op::I32(x) if i.rep == Rep::Bool => Some(u64::from(*x != 0)),
            Op::I32(x) => Some(f64::from(*x).to_bits()),
            Op::F64(x) => Some(x.to_bits()),
            Op::Word(w) => Some(*w),
            _ => None,
        }).collect();
        let reps = built.graph.insts.iter().map(|i| match i.op {
            Op::I32(_) | Op::F64(_) if i.rep != Rep::Bool => Rep::Word,
            _ => i.rep,
        }).collect();
        let hints: Vec<Option<(FuncId, u32)>> = lowered.exits.iter().map(|e| e.hint.and_then(|h| built.graph.states[h as usize].frames.last().map(|fr| (fr.func, fr.pc)))).collect();
        let meta = Rc::new(Meta { states: core::mem::take(&mut built.graph.states), reps, consts });
        let values = lowered.values;
        for ((exit, slots), hint_at) in lowered.exits.into_iter().zip(values).zip(hints) {
            st.deopts.push(Rc::new(Deopt { meta: meta.clone(), fs: exit.fs, slots, reason: exit.reason, op: exit.op, hint_at, native, root: f, kind }));
        }
        if self.debug.log {
            std::eprintln!("j8jit: installed {f:?} {kind:?} at {:?}", t0.elapsed());
        }
        Some(addr)
    }

    fn through_mlir(&self, g: &ir::Graph, kind: Kind, deopt_base: u32, helper_base: u32, globals: &[u64]) -> Option<codegen::Generated> {
        if !self.debug.mlir {
            return None;
        }
        let table = self.stencils.get_or_init(|| match stitch::Table::compile(&Compiler::default()) {
            Ok(t) => Some(t),
            Err(e) => {
                std::eprintln!("j8jit: the stencils did not compile: {e:?}");
                None
            }
        });
        match stitch::Stitch::stitch(g, kind, deopt_base, helper_base, globals, table.as_ref()?) {
            Ok(g) => Some(g),
            Err(e) => {
                if self.debug.log {
                    std::eprintln!("j8jit: stitching failed, direct code instead: {}", e.0);
                }
                None
            }
        }
    }

    fn run_native(&self, e: &mut Entry<'_, '_, '_, '_, '_>, addr: usize, ret: usize) -> Tiered {
        let params = e.params();
        let words: [u64; 4] = core::array::from_fn(|i| if i <= e.function().shared().formal_parameters as usize { unsafe { (*params.add(i)).bits() } } else { j8framework::ValueBits::UNDEFINED });
        let callee = e.callee().bits();
        let depth = e.executor().at_depth(e.depth());
        let mut ctx = Ctx::new(e, self);
        type NativeFn = sysv!(unsafe fn(*mut Ctx, u64, u64, u64, u64, u64) -> u64);
        let r = unsafe {
            let f: NativeFn = core::mem::transmute::<usize, NativeFn>(addr);
            f(&raw mut *ctx, callee, words[0], words[1], words[2], words[3])
        };
        drop(depth);
        let threw = (ctx.pending != 0).then(|| ctx.error.take().unwrap_or(j8framework::Error::Terminated));
        self.recycle(ctx);
        if let Some(err) = threw {
            return Tiered::Threw(err);
        }
        e.resume_at(ret, unsafe { Value::from_bits(r) });
        Tiered::Resumed
    }

    fn run(&self, e: &mut Entry<'_, '_, '_, '_, '_>, addr: usize) -> Tiered {
        let params = e.params().cast::<Value<'static>>();
        let r0 = e.r0().cast::<Value<'static>>();
        let depth = e.executor().at_depth(e.depth());
        let mut ctx = Ctx::new(e, self);
        let status = unsafe {
            let f: EntryFn = core::mem::transmute::<usize, EntryFn>(addr);
            f(&raw mut *ctx, params, r0)
        };
        drop(depth);
        let r = match status {
            RETURNED => {
                let v = unsafe { Value::from_bits(ctx.ret) };
                e.resume_at(usize::try_from(ctx.ret_pc).unwrap_or(0), v);
                Tiered::Resumed
            }
            RESUMED => Tiered::Resumed,
            THREW => Tiered::Threw(ctx.error.take().unwrap_or(j8framework::Error::Terminated)),
            _ => Tiered::Resumed,
        };
        self.recycle(ctx);
        r
    }
}

impl Tier for Jit {
    fn eager(&self) -> bool {
        self.threshold == 0
    }

    fn entered(&self) {
        self.entries.set(self.entries.get().wrapping_add(1));
    }

    fn call<'scope>(&self, exe: &j8executor::Executor, a: &mut j8framework::Scope<'scope, '_>, f: FuncId, callee: Value<'scope>, this: Value<'scope>, args: &[Value<'scope>]) -> Option<Result<Value<'scope>, j8framework::Error>> {
        let func = exe.jit_function(f);
        let formals = usize::from(func.shared().formal_parameters);
        if formals > 3 || !func.plain() || func.class_constructor() || !self.stack_room() {
            return None;
        }
        let addr = {
            let mut st = self.st.borrow_mut();
            Self::func(&mut st, f).native?
        };
        let this = if func.wraps_this() {
            match this.bits() {
                j8framework::ValueBits::UNDEFINED | j8framework::ValueBits::NULL => Value::Object(a.context.global),
                _ if this.as_object().is_some() => this,
                _ => return None,
            }
        } else {
            this
        };
        let arg = |i: usize| if i < formals { args.get(i).copied().unwrap_or_default().bits() } else { j8framework::ValueBits::UNDEFINED };
        let mut ctx = Ctx::bare(exe, a, self);
        type NativeFn = sysv!(unsafe fn(*mut Ctx, u64, u64, u64, u64, u64) -> u64);
        let r = unsafe {
            let code: NativeFn = core::mem::transmute::<usize, NativeFn>(addr);
            code(&raw mut *ctx, callee.bits(), this.bits(), arg(0), arg(1), arg(2))
        };
        let threw = (ctx.pending != 0).then(|| ctx.error.take().unwrap_or(j8framework::Error::Terminated));
        self.recycle(ctx);
        Some(match threw {
            Some(e) => Err(e),
            None => Ok(unsafe { Value::from_bits(r) }),
        })
    }

    fn callback<'scope>(&self, exe: &j8executor::Executor, a: &mut j8framework::Scope<'scope, '_>, func: j8executor::JitFunction<'_>, callee: Value<'scope>, this: Value<'scope>) -> Option<j8framework::Callback<'scope>> {
        let arity = usize::from(func.shared().formal_parameters);
        if arity > 3 || !func.plain() || func.class_constructor() || !self.stack_room() {
            return None;
        }
        let this = if func.wraps_this() {
            match this.bits() {
                j8framework::ValueBits::UNDEFINED | j8framework::ValueBits::NULL => Value::Object(a.context.global),
                _ if this.as_object().is_some() => this,
                _ => return None,
            }
        } else {
            this
        };
        let mut st = self.st.borrow_mut();
        if Self::func(&mut st, func.id()).native_never {
            return None;
        }
        let code = Self::stub(&mut st, func.id());
        drop(st);
        if code == 0 {
            return None;
        }
        let ctx = Ctx::bare(exe, a, self);
        let prep = Box::new(Prepared { ctx, seen_epoch: self.epoch.get(), seen_entries: self.entries.get(), generation: a.mc.generation() });
        let env = Box::into_raw(prep).cast::<()>();
        Some(j8framework::Callback { callee, this, run: Some(Self::run_callback), code, arity, env, release: Some(Self::release_callback), quiet: core::cell::Cell::new(false) })
    }

    fn enter(&self, e: &mut Entry<'_, '_, '_, '_, '_>) -> Tiered {
        let f = e.function();
        let id = f.id();
        f.tier_word().set(1);
        if !self.stack_room() || e.callee().as_object().and_then(|o| o.closure()).is_none() {
            return Tiered::Interpret;
        }
        let formals = usize::from(f.shared().formal_parameters);
        if f.plain() && !f.class_constructor() && !e.constructing() && formals <= 3 && e.callee().as_object().and_then(|o| o.closure()).is_some() {
            let native = {
                let mut st = self.st.borrow_mut();
                let fs = Self::func(&mut st, id);
                if fs.native_never { None } else { Some(fs.native) }
            };
            if let Some(native) = native {
                let addr = match native {
                    Some(a) => Some(a),
                    None => match self.compile(e.executor(), e.scope(), id, Kind::Native(formals)) {
                        Some(a) => {
                            let mut st = self.st.borrow_mut();
                            Self::func(&mut st, id).native = Some(a);
                            let stub = Self::stub(&mut st, id);
                            Self::point(&st, stub, a);
                            Some(a)
                        }
                        None => {
                            Self::func(&mut self.st.borrow_mut(), id).native_never = true;
                            None
                        }
                    },
                };
                if let Some(addr) = addr
                    && let Some(ret) = (0..f.len()).find(|&i| f.insn(i).bytecode == j8bytecode::Bytecode::Return)
                {
                    return self.run_native(e, addr, ret);
                }
            }
        }
        let code = {
            let mut st = self.st.borrow_mut();
            let fs = Self::func(&mut st, id);
            if fs.never {
                return Tiered::Interpret;
            }
            fs.enter
        };
        let addr = match code {
            Some(a) => a,
            None => match self.compile(e.executor(), e.scope(), id, Kind::Enter) {
                Some(a) => {
                    Self::func(&mut self.st.borrow_mut(), id).enter = Some(a);
                    a
                }
                None => {
                    Self::func(&mut self.st.borrow_mut(), id).never = true;
                    return Tiered::Interpret;
                }
            },
        };
        self.run(e, addr)
    }

    fn resume(&self, e: &mut Entry<'_, '_, '_, '_, '_>, pc: usize) -> Tiered {
        let f = e.function();
        let id = f.id();
        if !self.stack_room() {
            return Tiered::Interpret;
        }
        let code = {
            let mut st = self.st.borrow_mut();
            let fs = Self::func(&mut st, id);
            if fs.never {
                return Tiered::Interpret;
            }
            fs.osr.iter().find(|(at, _)| f.insn(**at).target == pc).map(|(_, a)| *a)
        };
        match code {
            Some(addr) => self.run(e, addr),
            None => Tiered::Interpret,
        }
    }

    fn osr(&self, e: &mut Entry<'_, '_, '_, '_, '_>, at: usize) -> Tiered {
        let f = e.function();
        let id = f.id();
        f.tier_word().set(1);
        if !self.stack_room() {
            return Tiered::Interpret;
        }
        let code = {
            let mut st = self.st.borrow_mut();
            let fs = Self::func(&mut st, id);
            if fs.never {
                return Tiered::Interpret;
            }
            fs.osr.get(&at).copied()
        };
        let addr = match code {
            Some(a) => a,
            None => match self.compile(e.executor(), e.scope(), id, Kind::Osr(at)) {
                Some(a) => {
                    Self::func(&mut self.st.borrow_mut(), id).osr.insert(at, a);
                    a
                }
                None => {
                    Self::func(&mut self.st.borrow_mut(), id).never = true;
                    return Tiered::Interpret;
                }
            },
        };
        self.run(e, addr)
    }
}

unsafe impl Send for Jit {}
