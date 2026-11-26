# Async and promises

j8 has no async runtime (no tokio). Each `j8::Runtime` is one [event loop](event-loop.md), driven by `eval` and
`run_until_idle`.
Async iterators and calling back into JS when Rust work finishes: [Custom content with the macros](custom-host.md).

## The core rule: a future must wake its waker

When nothing can run, the loop thread parks until a waker fires. So **a future must wake its waker when it can make
progress**, or the loop sleeps forever. Futures from crates that need their own reactor (tokio's sockets and timers)
don't work as they are: run that work on a thread or a reactor of your own, and wake the waker from there.

## `async fn` bindings

An `async fn` takes **owned** parameters only: no `self`, `&str`, `&mut Scope`, `This` or `Rest`. Its future runs after
the call has returned. The call returns a promise at once, and the result is converted with `IntoJs` when the promise
settles (so `Io<T>`-style errors reject it).

```rust,ignore
#[j8::object("later")]
impl Later {
    async fn fetch_text(url: String) -> String { … }
}
```

## Blocking work: offload to a thread

dune runs every blocking call (files, sockets, `sleep`) on a thread of its own (`host::Offload`). The core:

```rust,ignore
fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
    let this = self.get_mut();
    if let Some(work) = this.work.take() {                 // first poll: start the thread
        let (tx, rx) = mpsc::channel();
        let waker = cx.waker().clone();
        std::thread::spawn(move || {
            let _ = tx.send(work());
            waker.wake();                                  // ⇐ without this, run_until_idle never returns
        });
        this.done = Some(rx);
    }
    match this.done.as_ref().map(Receiver::try_recv) {
        Some(Ok(v)) => Poll::Ready(v),
        Some(Err(TryRecvError::Empty)) => Poll::Pending,
        _ => panic!("offloaded work panicked"),
    }
}
```

```rust,ignore
#[j8::object("fs")]
impl Fs {
    async fn read_file(path: String) -> Io<j8::Bytes> {
        Offload::new(move || Io::new(std::fs::read(&path).map(j8::Bytes), "open", &path)).await
    }
}
```

## Hand-made promises: `j8::Glue`

When the async work needs `&self`, the `Scope`, or should be cancellable, write a normal method and make the promise
yourself:

- `Glue::spawn(a, future)` creates a pending promise, hands `future` to the event loop and returns the promise;
- `Glue::settle(value)` is what the future must produce: `value` is converted with `IntoJs` when the promise settles.

```rust,ignore
#[j8::class]
impl Timer {
    fn wait<'scope>(&self, a: &mut Scope<'scope, '_>, ms: f64) -> Result<Value<'scope>, Error> {
        let sleep = Sleep { cancelled: self.cancelled.clone(), /* … */ };
        Glue::spawn(a, async move { Glue::settle(sleep.await) })
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}
```

The future must be `Send + 'static`: clone what it needs out of `self` (here an `Arc<AtomicBool>`). A future that
becomes ready because of JS (like `cancel` above) needs no wake-up: the loop polls everything again after every
settlement, before it parks.

Async work belongs to the isolate that started it: each `Runtime` has its own queue, settled only by its own
`run_until_idle`, so runtimes can share a platform. An isolate made without an event loop (by hand, without
`CreateParams { event_loop: true, .. }`) refuses the work, and the call throws a `TypeError`.

## Awaiting a JS promise from Rust: `j8::Promise<T>`

The other direction: a parameter of this type takes a JS promise, and Rust `.await`s it. It is an owned parameter, so
`async fn` bindings can take it:

```rust,ignore
#[j8::object("math")]
impl Math {
    async fn double(p: j8::Promise<f64>) -> Result<f64, Error> {
        Ok(p.await? * 2.0)          // a rejection passes on to this function's own promise
    }
}
```

- It takes its argument the way `await` does: a promise is watched directly (a `then` patched onto it is not called), a
  thenable is adopted, any other value counts as already fulfilled;
- the fulfilled value is converted with `FromJs` into `T` on the runtime thread; a rejection, or a value that doesn't
  convert, comes out as `Err(Error::Parked(ticket))` (the reason parked in the isolate), so `?` rejects the binding's own
  promise with the same reason;
- settling wakes the future, so it works with `run_until_idle`, and when polled elsewhere;
- a promise that never settles keeps the future pending, like `await` in JS: the event loop keeps waiting.

For a JS callback that runs more than once (an event handler), keep a `j8::Function` and call it inside `rt.enter` or
another binding.

## Keeping the process alive

`run_until_idle` returns once nothing is pending. Repeating timers, servers and the like stay alive by **always keeping a
future pending**, and stop by settling it. A cleared timer should settle its promise (dune's `Timer::cancel`), or the
loop waits for it.

## Unhandled rejections

When the microtasks have drained, every promise that was rejected and still has no handler goes to
`Platform::unhandled_rejection(scope, promise, reason)` (Node's `unhandledRejection`); a handler attached before that
(`await`, `then`, `catch`) takes it off the list, one attached on a later turn doesn't take a report back. The default
does nothing and `HostPlatform` passes it to the platform it wraps. `scope.stack_text(reason)` gives the text to print.

## Not yet

- **No reactor**: futures needing tokio's (or another runtime's) reactor never wake.
- **Settlements follow the polling order**: futures ready in the same turn settle in the order they were polled, not
  finished; JS that cares (timers) sorts it out itself.
- **No timers in the loop**: a timer is host work like any other (a sleeping thread that wakes its future).
- **No cancellation from JS**: a future can't be dropped from the JS side; watch a flag of your own (dune's `Timer::cancel`).
- **`async fn` can't take `&self`**: use `Glue::spawn`.
- **A panic in a future propagates out of the loop** (out of `eval` / `run_until_idle`); an error while settling stops
  the loop with the remaining work still queued.
- **Microtasks aren't sliced**: a settlement drains its microtasks in one turn.
