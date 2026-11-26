use alloc::boxed::Box;
use alloc::sync::Arc;
use core::ffi::c_void;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicUsize, Ordering};

use j8framework::{Scope, AsyncJob, Backing, ElementKind, External, ObjectData, ObjectKind, ScopeGlobal, Settle, Value, ValueBrand, ViewData};
use j8lockfree::{Fifo, Oneshot, Sender};

use crate::convert::Ffi;
use crate::{CleanupHook, Env, Finalize, Handle, NodeVersion, Status, napi};

#[derive(Clone, Copy, Debug)]
struct Raw<T>(T);

unsafe impl<T> Send for Raw<T> {}

unsafe impl<T> Sync for Raw<T> {}

impl<T> Raw<T> {
    fn get(self) -> T {
        self.0
    }
}

impl Env<'_> {
    fn uint8_array<'scope>(a: &Scope<'scope, '_>, bytes: Backing) -> j8framework::JsObject<'scope> {
        let len = bytes.len();
        let buffer = ObjectData { kind: ObjectKind::ArrayBuffer(Box::new(j8framework::BufferData { bytes: Some(bytes), max: None, shared: false })), proto: Some(a.context.array_buffer_proto), ..ObjectData::default() }.alloc(a.mc);
        let at = ElementKind::ALL.iter().position(|&k| k == ElementKind::Uint8).unwrap_or_default();
        let view = ViewData { buffer, offset: 0, length: Some(len) };
        ObjectData { kind: ObjectKind::TypedArray(ElementKind::Uint8, view), proto: Some(a.context.typed_protos[at]), ..ObjectData::default() }.alloc(a.mc)
    }

    fn spawn(a: &Scope<'_, '_>, ready: impl core::future::Future<Output = ()> + Send + 'static, settle: Settle, keep_alive: bool) -> Result<(), Status> {
        let p = a.new_promise();
        let (resolve, reject) = a.promise_resolvers(p);
        let future = Box::pin(async move {
            ready.await;
            settle
        });
        let job = AsyncJob::new(a.global(resolve.into()), a.global(reject.into()), future);
        a.spawn(if keep_alive { job } else { job.unref() }).map_err(|_| Status::GenericFailure)
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkState {
    Idle = 0,
    Queued = 1,
    Running = 2,
    Cancelled = 3,
}

pub struct Work {
    env: NonNull<c_void>,
    execute: unsafe extern "C" fn(env: *mut Env<'_>, data: *mut c_void),
    complete: Option<unsafe extern "C" fn(env: *mut Env<'_>, status: Status, data: *mut c_void)>,
    data: *mut c_void,
    state: Arc<AtomicU8>,
}

impl Work {
    fn queue(&self, a: &Scope<'_, '_>) -> Result<(), Status> {
        if self.state.compare_exchange(WorkState::Idle as u8, WorkState::Queued as u8, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return Err(Status::GenericFailure);
        }
        let (tx, rx) = Oneshot::<Status>::new();
        let job = Raw((self.env, self.execute, self.data, Arc::clone(&self.state)));
        std::thread::spawn(move || {
            let (env, execute, data, state) = job.get();
            let start = state.compare_exchange(WorkState::Queued as u8, WorkState::Running as u8, Ordering::AcqRel, Ordering::Acquire);
            if start.is_ok() {
                unsafe { execute(env.as_ptr().cast(), data) };
                tx.send(Status::Ok);
            } else {
                tx.send(Status::Cancelled);
            }
        });
        let back = Raw((self.env, self.complete, self.data, Arc::clone(&self.state)));
        let status = Arc::new(AtomicU8::new(0));
        let seen = Arc::clone(&status);
        let ready = async move {
            let s = rx.await;
            seen.store(s as u8, Ordering::Release);
        };
        let settle: Settle = Box::new(move |a| {
            let (env, complete, data, state) = back.get();
            let s = if status.load(Ordering::Acquire) == Status::Cancelled as u8 { Status::Cancelled } else { Status::Ok };
            state.store(WorkState::Idle as u8, Ordering::Release);
            let env = unsafe { env.cast::<Env<'_>>().as_ref() };
            env.enter(a, |env| {
                if let Some(complete) = complete {
                    unsafe { complete(env.as_ptr(), s, data) };
                }
                env.take_pending().map_or(Ok(Value::Undefined), Err)
            })
        });
        Env::spawn(a, ready, settle, true)
    }
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseMode {
    Release = 0,
    Abort = 1,
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallMode {
    NonBlocking = 0,
    Blocking = 1,
}

impl TryFrom<i32> for ReleaseMode {
    type Error = Status;

    fn try_from(n: i32) -> Result<Self, Status> {
        match n {
            0 => Ok(Self::Release),
            1 => Ok(Self::Abort),
            _ => Err(Status::InvalidArg),
        }
    }
}

impl TryFrom<i32> for CallMode {
    type Error = Status;

    fn try_from(n: i32) -> Result<Self, Status> {
        match n {
            0 => Ok(Self::NonBlocking),
            1 => Ok(Self::Blocking),
            _ => Err(Status::InvalidArg),
        }
    }
}

type CallJs = unsafe extern "C" fn(env: *mut Env<'_>, js_callback: Handle, context: *mut c_void, data: *mut c_void);

struct TsfnJs<'e> {
    env: NonNull<Env<'e>>,
    func: Option<ScopeGlobal<'e, ValueBrand>>,
    call_js: Option<CallJs>,
    finalize: Finalize,
    finalize_data: *mut c_void,
}

pub struct Tsfn {
    queue: Fifo<Raw<*mut c_void>>,
    queued: AtomicUsize,
    max: usize,
    threads: AtomicUsize,
    closing: AtomicBool,
    aborted: AtomicBool,
    refed: AtomicBool,
    armed: AtomicPtr<Sender<()>>,
    context: *mut c_void,
    js: *mut c_void,
}

unsafe impl Send for Tsfn {}

unsafe impl Sync for Tsfn {}

impl Tsfn {
    fn trigger(&self) {
        let p = self.armed.swap(core::ptr::null_mut(), Ordering::AcqRel);
        if !p.is_null() {
            unsafe { Box::from_raw(p) }.send(());
        }
    }

    fn arm(self: &Arc<Self>, a: &Scope<'_, '_>) -> Result<(), Status> {
        let (tx, rx) = Oneshot::<()>::new();
        let me = Arc::clone(self);
        let settle: Settle = Box::new(move |a| {
            me.run(a);
            Ok(Value::Undefined)
        });
        Env::spawn(a, rx, settle, self.refed.load(Ordering::Acquire))?;
        let old = self.armed.swap(Box::into_raw(Box::new(tx)), Ordering::AcqRel);
        if !old.is_null() {
            drop(unsafe { Box::from_raw(old) });
        }
        if !self.queue.is_empty() || self.closing.load(Ordering::Acquire) {
            self.trigger();
        }
        Ok(())
    }

    fn run(self: &Arc<Self>, a: &mut Scope<'_, '_>) {
        let js = unsafe { &*self.js.cast::<TsfnJs<'_>>() };
        let env = unsafe { js.env.as_ref() };
        let aborted = self.aborted.load(Ordering::Acquire);
        env.enter(a, |env| {
            while let Some(Raw(data)) = self.queue.pop() {
                self.queued.fetch_sub(1, Ordering::AcqRel);
                if aborted {
                    continue;
                }
                let Ok(a) = env.scope() else { return };
                let f = js.func.as_ref().map_or(Value::Undefined, |g| a.local(g));
                match js.call_js {
                    Some(cb) => unsafe { cb(env.as_ptr(), env.push(f), self.context, data) },
                    None if a.is_callable(f) => {
                        let r = a.call(f, Value::Undefined, &[]);
                        if let Err(e) = r {
                            env.throw(e);
                        }
                    }
                    None => {}
                }
                drop(env.take_pending());
            }
        });
        let done = self.closing.load(Ordering::Acquire) && (aborted || self.threads.load(Ordering::Acquire) == 0);
        if done || self.arm(a).is_err() {
            self.finish(env);
        }
    }

    fn finish(self: &Arc<Self>, env: &Env<'_>) {
        let js = unsafe { Box::from_raw(self.js.cast::<TsfnJs<'_>>()) };
        if let Some(cb) = js.finalize {
            unsafe { cb(env.as_ptr(), js.finalize_data, self.context) };
        }
        drop(unsafe { Arc::from_raw(Arc::as_ptr(self)) });
    }

    fn set_ref(&self, refed: bool) {
        if self.refed.swap(refed, Ordering::AcqRel) != refed {
            self.trigger();
        }
    }

    fn of<'t>(p: *const Self) -> Result<&'t Self, Status> {
        unsafe { p.as_ref() }.ok_or(Status::InvalidArg)
    }
}

napi! {
    fn napi_create_buffer(env, size: usize, data: *mut *mut c_void, result: *mut Handle) {
        let a = env.scope()?;
        let mut bytes = alloc::vec![0u8; size];
        Ffi::put_opt(data, bytes.as_mut_ptr().cast());
        env.out(result, Env::uint8_array(a, bytes.into()).into())
    }

    fn napi_create_buffer_copy(env, length: usize, data: *const c_void, result_data: *mut *mut c_void, result: *mut Handle) {
        let a = env.scope()?;
        let mut bytes = Ffi::slice(data.cast::<u8>(), length)?.to_vec();
        Ffi::put_opt(result_data, bytes.as_mut_ptr().cast());
        env.out(result, Env::uint8_array(a, bytes.into()).into())
    }

    fn napi_create_external_buffer(env, length: usize, data: *mut c_void, finalize_cb: Finalize, finalize_hint: *mut c_void, result: *mut Handle) {
        let a = env.scope()?;
        let owner = Box::new(env.finalizer(finalize_cb, data, finalize_hint));
        let e = unsafe { External::from_raw(data.cast(), length, owner) };
        env.out(result, Env::uint8_array(a, e.into()).into())
    }

    fn node_api_create_buffer_from_arraybuffer(env, arraybuffer: Handle, byte_offset: usize, byte_length: usize, result: *mut Handle) {
        let a = env.scope()?;
        let buffer = env.value(arraybuffer)?.as_object().ok_or(Status::ArraybufferExpected)?;
        let len = match &buffer.borrow().kind {
            ObjectKind::ArrayBuffer(b) if !b.shared => b.bytes.as_deref().map_or(0, <[u8]>::len),
            _ => return Err(Status::ArraybufferExpected),
        };
        if byte_offset.checked_add(byte_length).is_none_or(|end| end > len) {
            env.throw(a.throw(j8framework::ErrorKind::Range, "The byte offset + length is out of range"));
            return Err(Status::PendingException);
        }
        let at = ElementKind::ALL.iter().position(|&k| k == ElementKind::Uint8).unwrap_or_default();
        let view = ViewData { buffer, offset: byte_offset, length: Some(byte_length) };
        let o = ObjectData { kind: ObjectKind::TypedArray(ElementKind::Uint8, view), proto: Some(a.context.typed_protos[at]), ..ObjectData::default() }.alloc(a.mc);
        env.out(result, o.into())
    }

    fn napi_get_buffer_info(env, value: Handle, data: *mut *mut c_void, length: *mut usize) {
        let a = env.scope()?;
        let o = env.value(value)?.as_object().ok_or(Status::InvalidArg)?;
        let (size, view) = match &o.borrow().kind {
            ObjectKind::TypedArray(k, v) => (k.size(), *v),
            ObjectKind::DataView(v) => (1, *v),
            _ => return Err(Status::InvalidArg),
        };
        let n = Scope::view_length(&view, size).unwrap_or(0) * size;
        let p = match &mut view.buffer.borrow_mut(a.mc).kind {
            ObjectKind::ArrayBuffer(b) => b.bytes.as_deref_mut().map_or(core::ptr::null_mut(), |s| s.as_mut_ptr().wrapping_add(view.offset)),
            _ => core::ptr::null_mut(),
        };
        Ffi::put_opt(data, p.cast());
        Ffi::put_opt(length, n);
        Ok(())
    }

    fn napi_is_buffer(env, value: Handle, result: *mut bool) {
        let is = env.value(value)?.as_object().is_some_and(|o| matches!(o.borrow().kind, ObjectKind::TypedArray(..) | ObjectKind::DataView(_)));
        Ffi::put(result, is)
    }

    fn napi_get_node_version(env, result: *mut *const NodeVersion) {
        Ffi::put(result, env.node_version())
    }

    fn napi_add_env_cleanup_hook(env, fun: Option<CleanupHook>, arg: *mut c_void) {
        env.add_cleanup(fun.ok_or(Status::InvalidArg)?, arg)
    }

    fn napi_remove_env_cleanup_hook(env, fun: Option<CleanupHook>, arg: *mut c_void) {
        env.remove_cleanup(fun.ok_or(Status::InvalidArg)?, arg)
    }

    fn napi_fatal_exception(env, err: Handle) {
        let a = env.scope()?;
        let e = env.value(err)?;
        let report = env.js(a.get_named(a.context.global.into(), "reportError"))?;
        if a.is_callable(report) {
            env.js(a.call(report, Value::Undefined, &[e])).map(drop)
        } else {
            env.throw(a.throw_value(e));
            Ok(())
        }
    }

    fn napi_get_uv_event_loop(env, r#loop: *mut *mut c_void) {
        let _ = (env, r#loop);
        Err(Status::GenericFailure)
    }

    fn napi_create_async_work(
        env,
        async_resource: Handle,
        async_resource_name: Handle,
        execute: Option<unsafe extern "C" fn(*mut Env<'_>, *mut c_void)>,
        complete: Option<unsafe extern "C" fn(*mut Env<'_>, Status, *mut c_void)>,
        data: *mut c_void,
        result: *mut *mut Work,
    ) {
        let _ = (async_resource, async_resource_name);
        let execute = execute.ok_or(Status::InvalidArg)?;
        let w = Work { env: NonNull::from(env).cast(), execute, complete, data, state: Arc::new(AtomicU8::new(WorkState::Idle as u8)) };
        Ffi::put(result, Box::into_raw(Box::new(w)))
    }

    fn napi_delete_async_work(env, work: *mut Work) {
        let _ = env;
        if work.is_null() {
            return Err(Status::InvalidArg);
        }
        drop(unsafe { Box::from_raw(work) });
        Ok(())
    }

    fn napi_queue_async_work(env, work: *mut Work) {
        let a = env.scope()?;
        unsafe { work.as_ref() }.ok_or(Status::InvalidArg)?.queue(a)
    }

    fn napi_cancel_async_work(env, work: *mut Work) {
        let _ = env;
        let w = unsafe { work.as_ref() }.ok_or(Status::InvalidArg)?;
        w.state.compare_exchange(WorkState::Queued as u8, WorkState::Cancelled as u8, Ordering::AcqRel, Ordering::Acquire).map(drop).map_err(|_| Status::GenericFailure)
    }

    fn napi_create_threadsafe_function(
        env,
        func: Handle,
        async_resource: Handle,
        async_resource_name: Handle,
        max_queue_size: usize,
        initial_thread_count: usize,
        thread_finalize_data: *mut c_void,
        thread_finalize_cb: Finalize,
        context: *mut c_void,
        call_js_cb: Option<CallJs>,
        result: *mut *const Tsfn,
    ) {
        let _ = (async_resource, async_resource_name);
        let a = env.scope()?;
        let func = if func.0.is_none() {
            if call_js_cb.is_none() {
                return Err(Status::InvalidArg);
            }
            None
        } else {
            Some(a.global(env.callable(a, func)?))
        };
        if initial_thread_count == 0 || result.is_null() {
            return Err(Status::InvalidArg);
        }
        let js = Box::into_raw(Box::new(TsfnJs { env: NonNull::from(env), func, call_js: call_js_cb, finalize: thread_finalize_cb, finalize_data: thread_finalize_data })).cast::<c_void>();
        let t = Arc::new(Tsfn {
            queue: Fifo::default(),
            queued: AtomicUsize::new(0),
            max: max_queue_size,
            threads: AtomicUsize::new(initial_thread_count),
            closing: AtomicBool::new(false),
            aborted: AtomicBool::new(false),
            refed: AtomicBool::new(true),
            armed: AtomicPtr::new(core::ptr::null_mut()),
            context,
            js,
        });
        if let Err(s) = t.arm(a) {
            drop(unsafe { Box::from_raw(js.cast::<TsfnJs<'_>>()) });
            return Err(s);
        }
        Ffi::put(result, Arc::into_raw(t))
    }

    fn napi_get_threadsafe_function_context(env, func: *const Tsfn, result: *mut *mut c_void) {
        let _ = env;
        Ffi::put(result, Tsfn::of(func)?.context)
    }

    fn napi_ref_threadsafe_function(env, func: *const Tsfn) {
        let _ = env;
        Tsfn::of(func).map(|t| t.set_ref(true))
    }

    fn napi_unref_threadsafe_function(env, func: *const Tsfn) {
        let _ = env;
        Tsfn::of(func).map(|t| t.set_ref(false))
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_call_threadsafe_function(func: *const Tsfn, data: *mut c_void, is_blocking: i32) -> Status {
    let (Ok(t), Ok(is_blocking)) = (Tsfn::of(func), CallMode::try_from(is_blocking)) else { return Status::InvalidArg };
    loop {
        if t.closing.load(Ordering::Acquire) {
            return Status::Closing;
        }
        if t.max == 0 || t.queued.load(Ordering::Acquire) < t.max {
            break;
        }
        if is_blocking == CallMode::NonBlocking {
            return Status::QueueFull;
        }
        std::thread::yield_now();
    }
    t.queued.fetch_add(1, Ordering::AcqRel);
    t.queue.push(Raw(data));
    t.trigger();
    Status::Ok
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_acquire_threadsafe_function(func: *const Tsfn) -> Status {
    let Ok(t) = Tsfn::of(func) else { return Status::InvalidArg };
    if t.closing.load(Ordering::Acquire) {
        return Status::Closing;
    }
    t.threads.fetch_add(1, Ordering::AcqRel);
    Status::Ok
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_release_threadsafe_function(func: *const Tsfn, mode: i32) -> Status {
    let (Ok(t), Ok(mode)) = (Tsfn::of(func), ReleaseMode::try_from(mode)) else { return Status::InvalidArg };
    let left = t.threads.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
    let Ok(before) = left else { return Status::InvalidArg };
    if mode == ReleaseMode::Abort {
        t.aborted.store(true, Ordering::Release);
    }
    if before == 1 || mode == ReleaseMode::Abort {
        t.closing.store(true, Ordering::Release);
        t.trigger();
    }
    Status::Ok
}
