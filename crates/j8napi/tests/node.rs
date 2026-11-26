use core::ffi::{c_char, c_void};
use core::num::NonZeroUsize;
use core::ptr::{null, null_mut};
use core::time::Duration;

use j8builtins::Builtins;
use j8executor::{EventLoopSet, Executor};
use j8framework::{ScriptKind, Source};
use j8napi::{CallbackInfo, Env, Handle, Status};
use j8platform::DefaultPlatform;

type Cb = Option<unsafe extern "C" fn(*mut Env, *mut CallbackInfo) -> Handle>;
type Execute = Option<unsafe extern "C" fn(*mut Env, *mut c_void)>;
type Complete = Option<unsafe extern "C" fn(*mut Env, Status, *mut c_void)>;
type CallJs = Option<unsafe extern "C" fn(*mut Env, Handle, *mut c_void, *mut c_void)>;

#[allow(improper_ctypes, reason = "the addon treats env / info / work / thread-safe functions as opaque pointers")]
unsafe extern "C" {
    fn napi_create_function(env: *mut Env, name: *const c_char, len: usize, cb: Cb, data: *mut c_void, result: *mut Handle) -> Status;
    fn napi_set_named_property(env: *mut Env, o: Handle, name: *const c_char, v: Handle) -> Status;
    fn napi_get_cb_info(env: *mut Env, info: *mut CallbackInfo, argc: *mut usize, argv: *mut Handle, this: *mut Handle, data: *mut *mut c_void) -> Status;
    fn napi_create_double(env: *mut Env, x: f64, result: *mut Handle) -> Status;
    fn napi_get_undefined(env: *mut Env, result: *mut Handle) -> Status;
    fn napi_call_function(env: *mut Env, recv: Handle, f: Handle, argc: usize, argv: *const Handle, result: *mut Handle) -> Status;
    fn napi_create_reference(env: *mut Env, v: Handle, count: u32, result: *mut *mut c_void) -> Status;
    fn napi_get_reference_value(env: *mut Env, r: *mut c_void, result: *mut Handle) -> Status;
    fn napi_delete_reference(env: *mut Env, r: *mut c_void) -> Status;
    fn napi_create_async_work(env: *mut Env, resource: Handle, name: Handle, execute: Execute, complete: Complete, data: *mut c_void, result: *mut *mut c_void) -> Status;
    fn napi_queue_async_work(env: *mut Env, work: *mut c_void) -> Status;
    fn napi_delete_async_work(env: *mut Env, work: *mut c_void) -> Status;
    fn napi_create_threadsafe_function(env: *mut Env, f: Handle, resource: Handle, name: Handle, max: usize, threads: usize, fin_data: *mut c_void, fin: Option<unsafe extern "C" fn(*mut Env, *mut c_void, *mut c_void)>, context: *mut c_void, call_js: CallJs, result: *mut *mut c_void) -> Status;
    fn napi_call_threadsafe_function(f: *mut c_void, data: *mut c_void, blocking: i32) -> Status;
    fn napi_release_threadsafe_function(f: *mut c_void, mode: i32) -> Status;
    fn napi_ref_threadsafe_function(env: *mut Env, f: *mut c_void) -> Status;
    fn napi_unref_threadsafe_function(env: *mut Env, f: *mut c_void) -> Status;
}

const SUM_TO: u32 = 1000;

struct Job {
    sum: f64,
    cb: *mut c_void,
    work: *mut c_void,
}

unsafe fn first(env: *mut Env, info: *mut CallbackInfo) -> Handle {
    let (mut argc, mut argv) = (1usize, [Handle::default()]);
    unsafe { napi_get_cb_info(env, info, &raw mut argc, argv.as_mut_ptr(), null_mut(), null_mut()) };
    argv[0]
}

unsafe extern "C" fn execute(_: *mut Env, data: *mut c_void) {
    let job = unsafe { &mut *data.cast::<Job>() };
    job.sum = (1..=SUM_TO).map(f64::from).sum();
}

unsafe extern "C" fn complete(env: *mut Env, status: Status, data: *mut c_void) {
    assert_eq!(status, Status::Ok);
    let job = unsafe { Box::from_raw(data.cast::<Job>()) };
    let (mut f, mut this, mut x) = (Handle::default(), Handle::default(), Handle::default());
    unsafe {
        napi_get_reference_value(env, job.cb, &raw mut f);
        napi_get_undefined(env, &raw mut this);
        napi_create_double(env, job.sum, &raw mut x);
        napi_call_function(env, this, f, 1, &raw const x, null_mut());
        napi_delete_reference(env, job.cb);
        napi_delete_async_work(env, job.work);
    }
}

unsafe extern "C" fn work(env: *mut Env, info: *mut CallbackInfo) -> Handle {
    let cb = unsafe { first(env, info) };
    let job = Box::into_raw(Box::new(Job { sum: 0.0, cb: null_mut(), work: null_mut() }));
    unsafe {
        napi_create_reference(env, cb, 1, &raw mut (*job).cb);
        napi_create_async_work(env, Handle::default(), Handle::default(), Some(execute), Some(complete), job.cast(), &raw mut (*job).work);
        assert_eq!(napi_queue_async_work(env, (*job).work), Status::Ok);
    }
    Handle::default()
}

unsafe extern "C" fn call_js(env: *mut Env, f: Handle, _: *mut c_void, data: *mut c_void) {
    let (mut this, mut x) = (Handle::default(), Handle::default());
    unsafe {
        napi_get_undefined(env, &raw mut this);
        #[expect(clippy::cast_precision_loss, reason = "the values sent are 1, 2, 3")]
        napi_create_double(env, data.addr() as f64, &raw mut x);
        napi_call_function(env, this, f, 1, &raw const x, null_mut());
    }
}

struct Remote(*mut c_void);

unsafe impl Send for Remote {}

unsafe extern "C" fn feed(env: *mut Env, info: *mut CallbackInfo) -> Handle {
    let cb = unsafe { first(env, info) };
    let mut t = null_mut();
    unsafe { assert_eq!(napi_create_threadsafe_function(env, cb, Handle::default(), Handle::default(), 0, 1, null_mut(), None, null_mut(), Some(call_js), &raw mut t), Status::Ok) };
    let remote = Remote(t);
    std::thread::spawn(move || {
        let r = remote;
        for i in 1..=3usize {
            unsafe { napi_call_threadsafe_function(r.0, core::ptr::without_provenance_mut(i), 0) };
        }
        unsafe { napi_release_threadsafe_function(r.0, 0) };
    });
    Handle::default()
}

unsafe fn held<'h>(env: *mut Env, info: *mut CallbackInfo) -> &'h mut *mut c_void {
    let mut data = null_mut();
    unsafe {
        napi_get_cb_info(env, info, null_mut(), null_mut(), null_mut(), &raw mut data);
        &mut *data.cast::<*mut c_void>()
    }
}

unsafe extern "C" fn hold(env: *mut Env, info: *mut CallbackInfo) -> Handle {
    let cb = unsafe { first(env, info) };
    let mut t = null_mut();
    unsafe {
        assert_eq!(napi_create_threadsafe_function(env, cb, Handle::default(), Handle::default(), 0, 1, null_mut(), None, null_mut(), Some(call_js), &raw mut t), Status::Ok);
        assert_eq!(napi_unref_threadsafe_function(env, t), Status::Ok);
    }
    unsafe { *held(env, info) = t };
    Handle::default()
}

unsafe extern "C" fn rehold(env: *mut Env, info: *mut CallbackInfo) -> Handle {
    let t = unsafe { *held(env, info) };
    unsafe { assert_eq!(napi_ref_threadsafe_function(env, t), Status::Ok) };
    let remote = Remote(t);
    std::thread::spawn(move || {
        let r = remote;
        std::thread::sleep(Duration::from_millis(50));
        unsafe { napi_call_threadsafe_function(r.0, core::ptr::without_provenance_mut(4), 0) };
        unsafe { napi_release_threadsafe_function(r.0, 0) };
    });
    Handle::default()
}

unsafe extern "C" fn register(env: *mut Env, exports: Handle) -> Handle {
    let slot = Box::into_raw(Box::new(null_mut::<c_void>())).cast::<c_void>();
    let methods: [(&core::ffi::CStr, unsafe extern "C" fn(*mut Env, *mut CallbackInfo) -> Handle, *mut c_void); 4] =
        [(c"work", work, null_mut()), (c"feed", feed, null_mut()), (c"hold", hold, slot), (c"rehold", rehold, slot)];
    for (name, cb, data) in methods {
        let mut f = Handle::default();
        unsafe {
            napi_create_function(env, null(), 0, Some(cb), data, &raw mut f);
            napi_set_named_property(env, exports, name.as_ptr(), f);
        }
    }
    exports
}

fn text(l: &j8executor::EventLoop<'_>, script: &str) -> String {
    let g = l.eval(&Source { text: script, file: "text.js" }, ScriptKind::Classic).expect("eval");
    l.enter(|a| {
        let v = a.local(&g);
        String::from(&**a.to_string(v).expect("ToString"))
    })
}

#[test]
fn an_unrefd_threadsafe_function_does_not_keep_the_loop_running() {
    let platform = DefaultPlatform::default();
    EventLoopSet::scope(&platform, NonZeroUsize::MIN, |set| {
        let l = set.spawn(Executor::default(), &Builtins);
        l.enter(|a| {
            let exports = Env::open(a, register).expect("register");
            a.set_named(a.context.global.into(), "addon", exports).expect("global");
        });
        assert_eq!(text(&l, "globalThis.got = []; addon.hold(x => got.push(x)); 'held'"), "held");
        l.run().expect("an unref'd thread-safe function lets the loop finish");
        assert_eq!(text(&l, "addon.rehold(); 'refed'"), "refed");
        l.run().expect("loop");
        assert_eq!(text(&l, "got.join(' ')"), "4");
    });
}

#[test]
fn async_work_and_threadsafe_functions() {
    let platform = DefaultPlatform::default();
    EventLoopSet::scope(&platform, NonZeroUsize::MIN, |set| {
        let l = set.spawn(Executor::default(), &Builtins);
        l.enter(|a| {
            let exports = Env::open(a, register).expect("register");
            a.set_named(a.context.global.into(), "addon", exports).expect("global");
        });
        let start = "globalThis.got = []; addon.work(s => got.push(`sum ${s}`)); addon.feed(x => got.push(x));";
        l.eval(&Source { text: start, file: "start.js" }, ScriptKind::Classic).expect("start");
        l.run().expect("loop");
        let g = l.eval(&Source { text: "got.sort().join(' ')", file: "end.js" }, ScriptKind::Classic).expect("end");
        let text = l.enter(|a| {
            let v = a.local(&g);
            String::from(&**a.to_string(v).expect("ToString"))
        });
        assert_eq!(text, "1 2 3 sum 500500");
    });
}
