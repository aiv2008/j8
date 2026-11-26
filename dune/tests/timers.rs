use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const DUNE: &str = env!("CARGO_BIN_EXE_dune");
const SCRATCH: &str = env!("CARGO_TARGET_TMPDIR");
const LIMIT: Duration = Duration::from_secs(12);
const SAMPLE: Duration = Duration::from_millis(1);

const TIMERS: usize = 10_000;
const PERIODS: usize = 10;
const PERIOD_STEP_MS: usize = 20;
const FIRES: usize = 3;
const CLOCK_MS: usize = 1;

const BASELINE: &str = "setTimeout(() => console.log('ok'), 300);\n";

struct Run {
    stdout: String,
    stderr: String,
    success: bool,
    peak_threads: usize,
}

impl Run {
    fn of(name: &str, script: &str) -> Self {
        let path = PathBuf::from(SCRATCH).join(name);
        fs::write(&path, script).expect("write the script");
        let mut child = Command::new(DUNE).arg(&path).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("spawn dune");
        let tasks = PathBuf::from(format!("/proc/{}/task", child.id()));
        let start = Instant::now();
        let mut peak_threads = 0;
        let status = loop {
            if let Ok(dir) = fs::read_dir(&tasks) {
                peak_threads = peak_threads.max(dir.count());
            }
            if let Some(status) = child.try_wait().expect("wait for dune") {
                break Some(status);
            }
            if start.elapsed() > LIMIT {
                let _ = child.kill();
                break None;
            }
            thread::sleep(SAMPLE);
        };
        let out = child.wait_with_output().expect("read dune's output");
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let stderr = if status.is_some() { stderr } else { format!("{stderr}\n{name} did not finish within {LIMIT:?}") };
        Self { stdout: String::from_utf8_lossy(&out.stdout).into_owned(), stderr, success: status.is_some_and(|s| s.success()), peak_threads }
    }
}

fn many_timers() -> String {
    format!(
        r"
const TIMERS = {TIMERS}, PERIODS = {PERIODS}, STEP = {PERIOD_STEP_MS}, FIRES = {FIRES}, CLOCK_MS = {CLOCK_MS};
const firsts = [];
const fired = new Array(TIMERS).fill(0);
const errors = [];
for (let i = 0; i < TIMERS; i++) {{
  const period = STEP * (1 + (i * 7919) % PERIODS);
  const created = Date.now();
  const id = setInterval(() => {{
    fired[i]++;
    if (fired[i] === 1) firsts.push([created + period, i]);
    if (Date.now() - created < fired[i] * period - CLOCK_MS) errors.push(`timer ${{i}} fired early: ${{Date.now() - created}} ms for firing ${{fired[i]}} of period ${{period}}`);
    if (fired[i] === FIRES) clearInterval(id);
  }}, period);
}}
setTimeout(() => {{
  const missing = fired.findIndex((n) => n !== FIRES);
  if (missing >= 0) errors.push(`timer ${{missing}} fired ${{fired[missing]}} times`);
  for (let k = 1; k < firsts.length; k++) {{
    const [d0, i0] = firsts[k - 1], [d1, i1] = firsts[k];
    if (d1 < d0 - CLOCK_MS) {{ errors.push(`out of due order: timer ${{i0}} due at ${{d0}} ran before timer ${{i1}} due at ${{d1}}`); break; }}
  }}
  if (errors.length) throw new Error(errors.slice(0, 5).join('\n'));
  console.log('ok');
}}, STEP * PERIODS * (FIRES + 2));
"
    )
}

#[test]
fn ten_thousand_timers_run_on_the_loop_thread() {
    let base = Run::of("timers-baseline.js", BASELINE);
    assert!(base.success && base.stdout.trim() == "ok", "baseline failed:\n{}{}", base.stdout, base.stderr);
    let run = Run::of("timers-many.js", &many_timers());
    assert!(run.success && run.stdout.trim() == "ok", "timers failed (peak {} threads, one timer {}):\n{}{}", run.peak_threads, base.peak_threads, run.stdout, run.stderr);
    assert!(run.peak_threads <= base.peak_threads, "{TIMERS} timers peaked at {} threads, one timer at {}", run.peak_threads, base.peak_threads);
}
