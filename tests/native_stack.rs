use j8builtins::Builtins;
use j8executor::Executor;
use j8framework::{CreateParams, Isolate, Source};
use j8platform::DefaultPlatform;

const THREAD_STACK: usize = 2 << 20;

const DEEP: &str = "var deep = []; for (var i = 0; i < 100000; i++) deep = [deep];";

const CASES: &[(&str, &str)] = &[
    ("try { String(deep); 'returned'; } catch (e) { e instanceof RangeError ? 'range' : 'other'; }", "range"),
    ("try { JSON.stringify(deep); 'returned'; } catch (e) { e instanceof RangeError ? 'range' : 'other'; }", "range"),
    ("try { JSON.parse('['.repeat(100000) + ']'.repeat(100000)); 'returned'; } catch (e) { e instanceof RangeError ? 'range' : 'other'; }", "range"),
    ("try { deep.flat(Infinity); 'returned'; } catch (e) { e instanceof RangeError ? 'range' : 'other'; }", "range"),
    ("var p = new Proxy({}, { get: (t, k, r) => r[k] }); try { p.x; 'returned'; } catch (e) { e instanceof RangeError ? 'range' : 'other'; }", "range"),
    ("var o = { toString() { return String(o); } }; try { String(o); 'returned'; } catch (e) { e instanceof RangeError ? 'range' : 'other'; }", "range"),
    ("var q = {}; for (var i = 0; i < 100000; i++) q = new Proxy(q, {}); try { q.x; 'returned'; } catch (e) { e instanceof RangeError ? 'range' : 'other'; }", "range"),
];

const SMALL_STACK: usize = 64 << 10;

fn eval(text: &str, jit: bool) -> String {
    eval_with(text, jit, CreateParams::default())
}

fn eval_with(text: &str, jit: bool, params: CreateParams) -> String {
    let platform = DefaultPlatform::default();
    let mut isolate = Isolate::new(&platform, params);
    let executor = if jit { Executor::default().with_tier(j8jit::Jit::from(0)) } else { Executor::default() };
    let cx = isolate.create_context(&Builtins);
    let text = format!("{DEEP}\n{text}");
    let result = executor.eval(&mut isolate, &cx, &Source { text: &text, file: "native_stack.js" }).expect("the script runs");
    isolate.enter(&cx, &executor, |a| {
        let v = a.local(&result);
        String::from(&**a.to_string(v).expect("ToString"))
    })
}

#[test]
fn native_recursion_ends_in_a_range_error_on_a_two_mib_thread() {
    let ran = std::thread::Builder::new()
        .stack_size(THREAD_STACK)
        .spawn(|| {
            let mut bad = Vec::new();
            for jit in [false, true] {
                for (text, want) in CASES {
                    let got = eval(text, jit);
                    if got != *want {
                        bad.push(format!("{} {text}: {got} (want {want})", if jit { "jit" } else { "interpreter" }));
                    }
                }
            }
            bad
        })
        .expect("a thread with 2 MiB of stack")
        .join();
    let bad = ran.expect("the thread finishes without overflowing its stack");
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn a_smaller_stack_size_ends_native_recursion_sooner() {
    let text = "var d = []; for (var i = 0; i < 500; i++) d = [d]; try { JSON.stringify(d); 'returned'; } catch (e) { e instanceof RangeError ? 'range' : 'other'; }";
    assert_eq!(eval_with(text, false, CreateParams::default()), "returned");
    assert_eq!(eval_with(text, false, CreateParams { stack_size: std::num::NonZeroUsize::new(SMALL_STACK), ..CreateParams::default() }), "range");
}
