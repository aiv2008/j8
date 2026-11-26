use std::time::Duration;

use j8builtins::Builtins;
use j8executor::Executor;
use j8framework::{CreateParams, Error, Isolate, Platform, Pool, Source, Task};
use j8platform::DefaultPlatform;

const CASES: &[(&str, &str)] = &[
    // 默认比较函数按 ToString 的 UTF-16 码元序排序，不是数值序
    ("[10, 9, 1].sort().join()", "1,10,9"),
    // map 跳过空洞，结果里保留空洞
    ("[1, , 3].map(x => x * 2).join('|')", "2||6"),
    // 显式 String() 允许把 Symbol 转成字符串（对应隐式 ToString 会抛错）
    ("String(Symbol('x'))", "Symbol(x)"),
    ("'' + Symbol('x')", "Uncaught TypeError: Cannot convert a Symbol value to a string"),
    // 函数里 try、catch、finally、with 块中的 var 属于该函数（曾写错变量或触发编译器断言）
    (
        "(function () { var a = 0, b = 0, c = 0, d = 0; (function () { \
         try { var a = 1; throw 0 } catch (e) { var b = 1 } finally { var c = 1 } \
         with ({}) { var d = 1 } })(); return [a, b, c, d].join() })()",
        "0,0,0,0",
    ),
    // 正在 join 的数组再遇到自己时给空串，和 V8、SpiderMonkey 一样
    ("var a = [1]; a.push(a, 2); String(a)", "1,,2"),
    ("var a = [1]; a.push(a, 2); a.toLocaleString()", "1,,2"),
    ("var a = [1]; var b = [a, [a, a]]; b.push(b); b.join(';')", "1;1,1;"),
    // join 中途抛错后不留下正在 join 的记录
    ("var bad = [{ toString() { throw 0; } }]; try { String(bad); } catch (e) {} bad[0] = 1; String([bad, bad])", "1,1"),
    ("var o = {}; o.o = o; try { JSON.stringify(o); } catch (e) { e.name; }", "TypeError"),
];

struct Host {
    inner: DefaultPlatform,
    locale: &'static str,
    time_zone: &'static str,
}

impl Pool for Host {
    fn threads(&self) -> usize {
        self.inner.threads()
    }

    fn run(&self, tasks: Vec<Task<'_>>) {
        self.inner.run(tasks);
    }
}

impl Platform for Host {
    fn now(&self) -> Duration {
        self.inner.now()
    }

    fn wall_clock(&self) -> Duration {
        self.inner.wall_clock()
    }

    fn locale(&self) -> Option<String> {
        Some(self.locale.into())
    }

    fn time_zone(&self) -> Option<String> {
        Some(self.time_zone.into())
    }

    fn print(&self, line: &str) {
        self.inner.print(line);
    }
}

const INTL_FORMATS: &[(&str, &str)] = &[
    ("new Intl.NumberFormat('en-US').format(1234567.891)", "1,234,567.891"),
    ("new Intl.NumberFormat('de-DE').format(1234567.891)", "1.234.567,891"),
    ("new Intl.NumberFormat('en-IN').format(1234567)", "12,34,567"),
    ("new Intl.NumberFormat('en', { maximumSignificantDigits: 2 }).format(1234)", "1,200"),
    ("new Intl.NumberFormat('en', { minimumFractionDigits: 2 }).format(1.5)", "1.50"),
    ("new Intl.NumberFormat('en', { notation: 'compact' }).format(12345)", "12K"),
    ("new Intl.NumberFormat('en', { notation: 'scientific' }).format(123456)", "1.235E5"),
    ("new Intl.NumberFormat('en', { style: 'percent' }).format(0.256)", "26%"),
    ("new Intl.NumberFormat('en', { signDisplay: 'always' }).format(5)", "+5"),
    ("new Intl.NumberFormat('ar-EG').format(123)", "١٢٣"),
    ("new Intl.NumberFormat('en').formatToParts(-1234.5).map(p => p.type).join()", "minusSign,integer,group,integer,decimal,fraction"),
    ("(1234.5).toLocaleString('en-US')", "1,234.5"),
    ("(12345678901234567890n).toLocaleString('en-US')", "12,345,678,901,234,567,890"),
    ("['b', 'a', 'ä'].sort(new Intl.Collator('de').compare).join()", "a,ä,b"),
    ("['z', 'ä', 'a'].sort(new Intl.Collator('sv').compare).join()", "a,z,ä"),
    ("['a10', 'a9'].sort(new Intl.Collator('en', { numeric: true }).compare).join()", "a9,a10"),
    ("'a'.localeCompare('A', 'en', { sensitivity: 'base' })", "0"),
    ("'I'.toLocaleLowerCase('tr')", "ı"),
    ("'i'.toLocaleUpperCase('tr')", "İ"),
    ("new Intl.DateTimeFormat('en-US', { timeZone: 'UTC' }).format(Date.UTC(2020, 0, 2))", "1/2/2020"),
    ("new Intl.DateTimeFormat('en-US', { timeZone: 'UTC', dateStyle: 'long' }).format(Date.UTC(2020, 0, 2))", "January 2, 2020"),
    ("new Intl.DateTimeFormat('en-US', { timeZone: 'America/New_York', hour: 'numeric', minute: '2-digit' }).format(Date.UTC(2020, 6, 1, 16, 5))", "12:05\u{202f}PM"),
    ("new Intl.DateTimeFormat('en-US', { timeZone: 'UTC', hour: 'numeric', hour12: false }).resolvedOptions().hourCycle", "h23"),
    ("new Intl.DateTimeFormat('en', { timeZone: 'asia/kolkata' }).resolvedOptions().timeZone", "Asia/Kolkata"),
    ("new Intl.DateTimeFormat('en', { timeZone: '+05:30' }).resolvedOptions().timeZone", "+05:30"),
    ("Intl.getCanonicalLocales('EN-us')[0]", "en-US"),
    ("Intl.getCanonicalLocales('zh-u-co-pinyin-kn-true')[0]", "zh-u-co-pinyin-kn"),
    ("new Intl.PluralRules('en').select(1)", "one"),
    ("new Intl.PluralRules('en', { type: 'ordinal' }).select(22)", "two"),
    ("new Intl.PluralRules('ar').resolvedOptions().pluralCategories.join()", "zero,one,two,few,many,other"),
];

const INTL_HOST: &[(&str, &str)] = &[
    ("new Intl.NumberFormat().resolvedOptions().locale", "de-DE"),
    ("new Intl.DateTimeFormat().resolvedOptions().timeZone", "Asia/Shanghai"),
    ("(1234.5).toLocaleString()", "1.234,5"),
    ("new Date(0).getHours()", "8"),
    ("new Date(0).getTimezoneOffset()", "-480"),
    ("new Date(1970, 0, 1).getTime()", "-28800000"),
    ("new Date('1970-01-01T00:00').getTime()", "-28800000"),
    ("new Date('1970-01-01').getTime()", "0"),
    ("new Date(0).toLocaleTimeString('en-US')", "8:00:00\u{202f}AM"),
];

fn outcome<P: Platform>(platform: &P, text: &str) -> String {
    let mut isolate = Isolate::new(platform, CreateParams::default());
    let executor = Executor::default();
    let cx = isolate.create_context(&Builtins);
    let r = executor.eval(&mut isolate, &cx, &Source { text, file: "semantics.js" });
    isolate.enter(&cx, &executor, |a| {
        match r {
            Ok(g) => {
                let v = a.local(&g);
                String::from(&**a.to_string(v).expect("ToString"))
            }
            Err(Error::Exception) => {
                let v = a.take_exception();
                format!("Uncaught {}", String::from(&**a.to_string(v).expect("ToString")))
            }
            Err(e) => panic!("not a completion: {e}"),
        }
    })
}

fn check<P: Platform>(platform: &P, cases: &[(&str, &str)]) {
    let bad: Vec<String> = cases.iter().filter_map(|(text, want)| {
        let got = outcome(platform, text);
        (got != *want).then(|| format!("{text}\n  want: {want}\n  got:  {got}"))
    }).collect();
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn javascript_semantics() {
    check(&DefaultPlatform::default(), CASES);
}

#[test]
fn intl_formats() {
    check(&Host { inner: DefaultPlatform::default(), locale: "en-US", time_zone: "UTC" }, INTL_FORMATS);
}

#[test]
fn intl_host_locale_and_time_zone() {
    check(&Host { inner: DefaultPlatform::default(), locale: "de-DE", time_zone: "Asia/Shanghai" }, INTL_HOST);
}
