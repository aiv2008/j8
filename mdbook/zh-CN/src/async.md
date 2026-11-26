# 异步与 Promise

j8 不带异步运行时（没有 tokio）。每个 `j8::Runtime` 就是一个[事件循环](event-loop.md)：`eval` 与 `run_until_idle` 驱动它。
异步迭代器、Rust 活干完时回调 JS：见[用宏开发自定义内容](custom-host.md)。

## 核心规则：future 必须唤醒它的 waker

事件循环什么都跑不了时线程会停泊，直到 waker 被唤醒。所以**一个 future 能推进时必须唤醒它的 waker**，否则循环永远睡下去。
需要自己反应堆的 crate 的 future（tokio 的套接字和定时器）直接拿来用是不会醒的：把那种活放到线程上、或放到你自己的反应堆上，
从那里唤醒 waker。

## `async fn` 绑定

`async fn` 只收**拥有所有权**的参数：不能有 `self`、`&str`、`&mut Scope`、`This`、`Rest`。它的 future 在调用返回之后才跑。
调用立刻返回一个 promise，兑现时结果用 `IntoJs` 转换（所以 `Io<T>` 式的错误会拒绝它）。

```rust,ignore
#[j8::object("later")]
impl Later {
    async fn fetch_text(url: String) -> String { … }
}
```

## 阻塞的活：放到线程上

dune 把每个阻塞调用（文件、套接字、`sleep`）放到单独的线程上跑（`host::Offload`）。核心是：

```rust,ignore
fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
    let this = self.get_mut();
    if let Some(work) = this.work.take() {                 // 第一次 poll：起线程
        let (tx, rx) = mpsc::channel();
        let waker = cx.waker().clone();
        std::thread::spawn(move || {
            let _ = tx.send(work());
            waker.wake();                                  // ⇐ 没有这一行，run_until_idle 永远不返回
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

## 自己造 promise：`j8::Glue`

异步活需要 `&self`、`Scope`、或者要能取消时，写普通方法，用 `j8::Glue` 自己造 promise：

- `Glue::spawn(a, future)` 建一个待定的 promise，把 `future` 交给事件循环，返回这个 promise；
- `Glue::settle(value)` 是 future 必须产出的东西：兑现时 `value` 用 `IntoJs` 转换。

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

future 必须 `Send + 'static`：从 `self` 里克隆出它要的东西（这里是 `Arc<AtomicBool>`）。因 JS 而就绪的 future（如上面的
`cancel`）不需要唤醒：每次兑现之后、停泊之前，循环会把所有 future 再 poll 一遍。

异步活属于发起它的 isolate：每个 `Runtime` 有自己的队列，只有它自己的 `run_until_idle` 兑现它，所以多个运行时可以共享一个平台。
没有事件循环的 isolate（手工建、没有 `CreateParams { event_loop: true, .. }`）拒绝这种活，调用抛 `TypeError`。

## 在 Rust 里 await JS promise：`j8::Promise<T>`

反方向：这种类型的参数接收一个 JS promise，Rust 侧 `.await` 它。它是拥有所有权的参数，所以 `async fn` 绑定可以收：

```rust,ignore
#[j8::object("math")]
impl Math {
    async fn double(p: j8::Promise<f64>) -> Result<f64, Error> {
        Ok(p.await? * 2.0)          // 拒绝原样传给这个函数自己的 promise
    }
}
```

- 像 `await` 一样接收参数：promise 直接监视（补丁上去的 `then` 不会被调用），thenable 被采纳，别的值算已经兑现；
- 兑现的值在运行时线程上用 `FromJs` 转成 `T`；拒绝、或者转不过来，都以 `Err(Error::Parked(ticket))`（原因暂存在 isolate 里）出来，
  所以 `?` 用同一个理由拒绝绑定自己的 promise；
- 兑现会唤醒 future，所以它能配合 `run_until_idle`，在别处 poll 也行；
- 永不兑现的 promise 让 future 一直待定，和 JS 里的 `await` 一样，事件循环会一直等。

会被多次调用的 JS 回调（事件处理器），存一个 `j8::Function`，在 `rt.enter` 或另一个绑定里调用。

## 保持进程存活

`run_until_idle` 在没有待定的东西时返回。重复定时器、服务器之类靠**一直留着一个待定的 future** 保活，靠兑现它停止。
清掉的定时器应该兑现它的 promise（dune 的 `Timer::cancel`），否则循环会一直等它。

## 未处理的拒绝

微任务排空后，被拒绝、到这时还没有处理函数的 promise 逐个交给 `Platform::unhandled_rejection(scope, promise, reason)`（相当于 Node 的 `unhandledRejection`）；在这之前挂上的处理函数（`await`、`then`、`catch`）会把它从名单里拿掉，之后的回合才挂上的不会撤回已经报告的。默认什么也不做，`HostPlatform` 转给它包着的平台。要打印的文字用 `scope.stack_text(reason)` 取。

## 事件循环还不做的事

- **没有反应堆**：需要 tokio（或别的运行时）反应堆的 future 永远不醒。
- **兑现按 poll 的顺序**：同一回合就绪的 future 按被 poll 的顺序兑现，而不是完成的顺序；在乎顺序的 JS（定时器）要自己排。
- **循环里没有定时器**：定时器和别的宿主任务一样（一个睡觉的线程，醒来唤醒它的 future）。
- **不能从 JS 取消**：future 不能从 JS 一侧丢掉；用你自己的标志（dune 的 `Timer::cancel`）。
- **`async fn` 不能收 `&self`**：用 `Glue::spawn`。
- **future 里的 panic 会传出循环**（从 `eval` / `run_until_idle` 出去）；兑现时出错会停下循环，剩下的活留在队列里。
- **微任务不分片**：一次兑现把它的微任务一口气排空。
