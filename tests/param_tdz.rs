use j8executor::Executor;
use j8platform::DefaultPlatform;

j8::runtime!{
    ParamTdzRt{}
}

struct Case {
    script: &'static str,
    expect: &'static str,
}

const CASES: &[Case] = &[
    Case {
        script: "(() => { try { return String((function f(a = b, b) { return a })()); } catch (e) { return e.name + ': ' + e.message; } })()",
        expect: "ReferenceError: Cannot access 'b' before initialization",
    },
    Case {
        script: "(() => { try { return String((function f(a = b, b) { return a })(undefined, 2)); } catch (e) { return e.name + ': ' + e.message; } })()",
        expect: "ReferenceError: Cannot access 'b' before initialization",
    },
    Case {
        script: "(() => { try { return String((function f(a = a) { return a })()); } catch (e) { return e.name + ': ' + e.message; } })()",
        expect: "ReferenceError: Cannot access 'a' before initialization",
    },
    Case { script: "(function f(a = 1, b = a) { return [a, b].join() })()", expect: "1,1" },
    Case { script: "(function f(a = () => b, b) { return a() })()", expect: "undefined" },
];

#[test]
fn parameter_dead_zone_follows_initialization_order() {
    let platform = ParamTdzRt::platform(DefaultPlatform::default());
    j8::EventLoopSet::scope(&platform, std::num::NonZeroUsize::MIN, |set| {
        let rt = ParamTdzRt::runtime(set, Executor::default()).expect("runtime");
        let mut failures: Vec<String> = Vec::new();
        for (i, case) in CASES.iter().enumerate() {
            let got = match rt.eval(case.script, "params.js") {
                Ok(got) => rt.text(&got),
                Err(e) => format!("threw {e}"),
            };
            println!("case {i}: {got:?}");
            if got != case.expect {
                failures.push(format!("case {i}: got {got:?}, want {:?}", case.expect));
            }
        }
        if !failures.is_empty() {
            panic!("{} of {} cases fail:\n{}", failures.len(), CASES.len(), failures.join("\n"));
        }
    });
}
