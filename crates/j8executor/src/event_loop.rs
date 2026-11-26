use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BinaryHeap, VecDeque};
use core::cmp::Reverse;
use core::time::Duration;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::task::Wake;
use alloc::vec::Vec;
use core::future::Future;
use core::num::NonZeroUsize;
use core::cell::UnsafeCell;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use core::task::{Context, Poll, Waker};
use std::thread::Thread;

use j8lockfree::{AtomicRefCell, Fifo, Oneshot, Receiver, SegVec, Sender};
use j8taskqueue::{Flow, ITask, ITaskPoll, Queue, Task, TaskId};

use j8framework::{Scope, AsyncJob, TimerRequest, CreateParams, Error, ErrorKind, Intrinsics, Isolate, IsolateHandle, HeapLimitCallback, Platform, Quota, ContextBrand, ScopeGlobal, ScriptKind, Settle, Source, Value, ValueBrand};

use crate::interrupt::Interrupts;
use crate::{Executor, Interrupter, Job};

pub const SLICE: usize = 10_000;

pub const CHECK: usize = 1_000;

const PULL_GAP: usize = 2;

struct Door {
    wakes: Fifo<Arc<Bell>>,
    thread: AtomicRefCell<Thread>,
    closing: AtomicBool,
    lanes: AtomicUsize,
    queued: AtomicUsize,
    spent: AtomicU64,
    asked: AtomicBool,
}

impl Door {
    fn new() -> Self {
        Self {
            wakes: Fifo::default(),
            thread: AtomicRefCell::new(std::thread::current()),
            closing: AtomicBool::new(false),
            lanes: AtomicUsize::new(0),
            queued: AtomicUsize::new(0),
            spent: AtomicU64::new(0),
            asked: AtomicBool::new(false),
        }
    }

    fn load(&self) -> usize {
        self.lanes.load(Ordering::Relaxed) + self.queued.load(Ordering::Relaxed)
    }

    fn unpark(&self) {
        if let Some(t) = self.thread.try_borrow() {
            t.unpark();
        }
    }

    fn watch(&self) {
        let me = std::thread::current();
        if self.thread.try_borrow().is_some_and(|t| t.id() == me.id()) {
            return;
        }
        loop {
            if let Some(mut t) = self.thread.try_borrow_mut() {
                *t = me;
                return;
            }
            core::hint::spin_loop();
        }
    }
}

#[derive(Default)]
struct Doors(SegVec<Arc<Door>>);

impl Doors {
    fn get(&self, i: usize) -> &Door {
        self.0.get(i).expect("a thread of the set")
    }
}

pub(crate) struct Bell {
    doors: Arc<Doors>,
    home: AtomicUsize,
    slot: AtomicUsize,
    scheduled: AtomicBool,
    woken: Fifo<usize>,
    outstanding: AtomicUsize,
    faults: Fifo<Error>,
    idle: Fifo<Thread>,
    weight: AtomicUsize,
    spent: AtomicU64,
    handle: IsolateHandle,
}

impl Bell {
    fn door(&self) -> &Door {
        self.doors.get(self.home.load(Ordering::Acquire))
    }

    fn ring(self: &Arc<Self>) {
        if !self.scheduled.swap(true, Ordering::AcqRel) {
            let door = self.door();
            door.wakes.push(Arc::clone(self));
            door.unpark();
        }
    }

    pub(crate) fn interrupt(self: &Arc<Self>) {
        self.handle.0.interrupt();
        self.ring();
    }

    fn begin(&self) {
        self.outstanding.fetch_add(1, Ordering::AcqRel);
    }

    fn end(&self) {
        if self.outstanding.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.wake_idle();
        }
    }

    fn fault(&self, e: Error) {
        self.faults.push(e);
        self.wake_idle();
    }

    fn fault_unless_terminated(&self, e: Error) {
        if !matches!(e, Error::Terminated) {
            self.fault(e);
        }
    }

    fn wake_idle(&self) {
        while let Some(t) = self.idle.pop() {
            t.unpark();
        }
    }

    fn quiet(&self) -> bool {
        self.outstanding.load(Ordering::Acquire) == 0 || !self.faults.is_empty()
    }
}

struct JobWaker {
    job: usize,
    bell: Arc<Bell>,
}

impl Wake for JobWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.bell.woken.push(self.job);
        self.bell.ring();
    }
}

struct ThreadWaker(Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<T>(mut rx: Receiver<T>) -> T {
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(v) = Pin::new(&mut rx).poll(&mut cx) {
            return v;
        }
        std::thread::park();
    }
}

struct Reply<R>(Option<Sender<Option<R>>>);

impl<R> Reply<R> {
    fn send(mut self, out: R) {
        if let Some(tx) = self.0.take() {
            tx.send(Some(out));
        }
    }
}

impl<R> Drop for Reply<R> {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            tx.send(None);
        }
    }
}

type Answer<'p> = Sender<Result<ScopeGlobal<'p, ValueBrand>, Error>>;

struct Submitted<'p> {
    text: String,
    file: String,
    kind: ScriptKind,
    reply: Answer<'p>,
}

struct Pending<'p> {
    resolve: ScopeGlobal<'p, ValueBrand>,
    reject: ScopeGlobal<'p, ValueBrand>,
    future: Pin<Box<dyn Future<Output = Settle> + Send>>,
    waker: Waker,
    keep_alive: bool,
}

struct JsState<'p> {
    bell: Arc<Bell>,
    isolate: Isolate<'p>,
    engine: Executor<'p>,
    cx: ScopeGlobal<'p, ContextBrand>,
    pending: Vec<Option<Pending<'p>>>,
    holes: Vec<usize>,
    last: Option<TaskId<'p>>,
    platform: &'p dyn Platform,
    due: BinaryHeap<Reverse<(Duration, u64)>>,
    armed: BTreeMap<u64, (ScopeGlobal<'p, ValueBrand>, bool)>,
}

struct Js<'p> {
    state: UnsafeCell<JsState<'p>>,
}

unsafe impl<'p> Sync for Js<'p> where JsState<'p>: Send {}

impl<'p> Js<'p> {
    #[expect(clippy::mut_from_ref, reason = "exclusive by the queue's protocol, see the type")]
    unsafe fn state(&self) -> &mut JsState<'p> {
        unsafe { &mut *self.state.get() }
    }
}

impl<'p> JsState<'p> {
    fn chain(&mut self, add: impl FnOnce(Task<'p>) -> TaskId<'p>, make: impl FnOnce(Vec<TaskId<'p>>) -> Task<'p>) {
        let after = self.last.take().into_iter().collect();
        self.last = Some(add(make(after)));
    }

    fn take_timers(&mut self) {
        for request in self.isolate.take_timers() {
            match request {
                TimerRequest::Arm { id, due, keep_alive, resolve } => {
                    if keep_alive {
                        self.bell.begin();
                    }
                    self.due.push(Reverse((due, id)));
                    self.armed.insert(id, (resolve, keep_alive));
                }
                TimerRequest::Cancel(id) => {
                    if let Some((_, true)) = self.armed.remove(&id) {
                        self.bell.end();
                    }
                }
            }
        }
    }

    fn next_due(&self) -> Option<Duration> {
        self.due.peek().map(|Reverse((at, _))| *at)
    }

    fn fire_due(&mut self, js: &Arc<Js<'p>>, mut add: impl FnMut(Task<'p>) -> TaskId<'p>) {
        let now = self.platform.now();
        while let Some(&Reverse((at, id))) = self.due.peek()
            && at <= now
        {
            self.due.pop();
            if let Some((resolve, keep_alive)) = self.armed.remove(&id) {
                let js = Arc::clone(js);
                self.chain(&mut add, |after| Box::new(SettleTask { js, after, resolve, reject: None, settle: Some(Box::new(|_| Ok(Value::Bool(true)))), checkpoint: None, keep_alive }));
            }
        }
    }

    fn adopt(&mut self, js: &Arc<Js<'p>>, mut add: impl FnMut(Task<'p>)) {
        self.take_timers();
        for AsyncJob { resolve, reject, future, keep_alive } in self.isolate.take_spawned() {
            let job = self.holes.pop().unwrap_or(self.pending.len());
            let waker = Waker::from(Arc::new(JobWaker { job, bell: Arc::clone(&self.bell) }));
            let p = Some(Pending { resolve, reject, future, waker, keep_alive });
            match self.pending.get_mut(job) {
                Some(hole) => *hole = p,
                None => self.pending.push(p),
            }
            if keep_alive {
                self.bell.begin();
            }
            add(Box::new(PollTask { js: Arc::clone(js), job }));
        }
    }

    fn take_woken(&mut self, js: &Arc<Js<'p>>, mut add: impl FnMut(Task<'p>)) {
        let mut woken: Vec<usize> = core::iter::from_fn(|| self.bell.woken.pop()).collect();
        woken.sort_unstable();
        woken.dedup();
        for job in woken {
            add(Box::new(PollTask { js: Arc::clone(js), job }));
        }
    }

    fn settle(&mut self, resolve: &ScopeGlobal<'p, ValueBrand>, reject: Option<&ScopeGlobal<'p, ValueBrand>>, settle: Settle) -> Result<(), Error> {
        self.enter(|a| {
            let (target, value) = match settle(a) {
                Ok(v) => (resolve, v),
                Err(Error::Exception) => (reject.ok_or(Error::Exception)?, a.take_exception()),
                Err(Error::Parked(ticket)) => {
                    let _ = a.unpark(ticket);
                    (reject.ok_or(Error::Exception)?, a.take_exception())
                }
                Err(e) => {
                    let message = alloc::string::ToString::to_string(&e);
                    let _ = a.throw(ErrorKind::Error, &message);
                    (reject.ok_or(Error::Exception)?, a.take_exception())
                }
            };
            let f = a.local(target);
            a.call(f, Value::Undefined, &[value]).map(drop)
        })
    }

    fn enter<R>(&mut self, f: impl for<'scope> FnOnce(&mut Scope<'scope, '_>) -> R) -> R {
        self.isolate.enter(&self.cx, &self.engine, f)
    }
}

fn add_to<'p>(poll: &mut dyn ITaskPoll<'p>) -> impl FnMut(Task<'p>) -> TaskId<'p> {
    move |t| match poll.add(t) {
        Ok(id) => id,
        Err(_) => unreachable!("event loop tasks that add are recursable"),
    }
}

struct ScriptTask<'p> {
    js: Arc<Js<'p>>,
    after: Vec<TaskId<'p>>,
    script: Option<(String, String, ScriptKind)>,
    job: Option<Job<'p>>,
    reply: Option<Answer<'p>>,
}

impl<'p> ITask<'p> for ScriptTask<'p> {
    fn recursable(&self) -> bool {
        true
    }

    fn depend(&self) -> &[TaskId<'p>] {
        &self.after
    }

    fn exec(&mut self, poll: &mut dyn ITaskPoll<'p>, quota: &mut Quota) -> Flow {
        let js = Arc::clone(&self.js);
        let st = unsafe { js.state() };
        if let Some((text, file, kind)) = self.script.take() {
            match st.engine.start(&Source { text: &text, file: &file }, kind) {
                Ok(job) => self.job = Some(job),
                Err(e) => return self.finish(st, Err(e)),
            }
        }
        let Some(job) = &mut self.job else { return Flow::Done };
        let r = st.engine.resume(&mut st.isolate, &st.cx, job, quota);
        let mut add = add_to(poll);
        st.adopt(&self.js, |t| drop(add(t)));
        st.take_woken(&self.js, |t| drop(add(t)));
        match r {
            Ok(None) => Flow::Yield,
            Ok(Some(g)) => self.finish(st, Ok(g)),
            Err(e) => self.finish(st, Err(e)),
        }
    }
}

impl<'p> ScriptTask<'p> {
    fn finish(&mut self, st: &mut JsState<'p>, done: Result<ScopeGlobal<'p, ValueBrand>, Error>) -> Flow {
        if let Some(reply) = self.reply.take() {
            reply.send(done);
        }
        st.bell.end();
        Flow::Done
    }
}

struct SettleTask<'p> {
    js: Arc<Js<'p>>,
    after: Vec<TaskId<'p>>,
    resolve: ScopeGlobal<'p, ValueBrand>,
    reject: Option<ScopeGlobal<'p, ValueBrand>>,
    settle: Option<Settle>,
    checkpoint: Option<Job<'p>>,
    keep_alive: bool,
}

impl<'p> ITask<'p> for SettleTask<'p> {
    fn recursable(&self) -> bool {
        true
    }

    fn depend(&self) -> &[TaskId<'p>] {
        &self.after
    }

    fn exec(&mut self, poll: &mut dyn ITaskPoll<'p>, quota: &mut Quota) -> Flow {
        let st = unsafe { self.js.state() };
        if let Some(settle) = self.settle.take() {
            quota.spend(1);
            match st.settle(&self.resolve, self.reject.as_ref(), settle) {
                Ok(()) => self.checkpoint = Some(Executor::checkpoint()),
                Err(e) => st.bell.fault_unless_terminated(e),
            }
        }
        let flow = match &mut self.checkpoint {
            Some(job) => match st.engine.resume(&mut st.isolate, &st.cx, job, quota) {
                Ok(None) => Flow::Yield,
                Ok(Some(_)) => Flow::Done,
                Err(e) => {
                    st.bell.fault_unless_terminated(e);
                    Flow::Done
                }
            },
            None => Flow::Done,
        };
        let mut add = add_to(poll);
        st.adopt(&self.js, |t| drop(add(t)));
        st.take_woken(&self.js, |t| drop(add(t)));
        if flow == Flow::Done && self.keep_alive {
            st.bell.end();
        }
        flow
    }
}

struct PollTask<'p> {
    js: Arc<Js<'p>>,
    job: usize,
}

impl<'p> ITask<'p> for PollTask<'p> {
    fn recursable(&self) -> bool {
        true
    }

    fn exec(&mut self, poll: &mut dyn ITaskPoll<'p>, _: &mut Quota) -> Flow {
        let st = unsafe { self.js.state() };
        let Some(Some(p)) = st.pending.get_mut(self.job) else { return Flow::Done };
        let ready = p.future.as_mut().poll(&mut Context::from_waker(&p.waker));
        if let Poll::Ready(settle) = ready
            && let Some(Pending { resolve, reject, keep_alive, .. }) = st.pending[self.job].take()
        {
            st.holes.push(self.job);
            let js = Arc::clone(&self.js);
            st.chain(add_to(poll), |after| Box::new(SettleTask { js, after, resolve, reject: Some(reject), settle: Some(settle), checkpoint: None, keep_alive }));
        }
        Flow::Done
    }
}

struct Lane<'p> {
    tasks: Queue<'p>,
    js: Arc<Js<'p>>,
    bell: Arc<Bell>,
    submitted: Arc<Fifo<Submitted<'p>>>,
}

impl<'p> Lane<'p> {
    fn new(platform: &'p dyn Platform, engine: Executor<'p>, intrinsics: &dyn Intrinsics, doors: &Arc<Doors>, home: usize) -> Box<Self> {
        let mut isolate = Isolate::new(platform, CreateParams { event_loop: true, collector_threads: Some(NonZeroUsize::MIN), ..CreateParams::default() });
        let cx = isolate.create_context(intrinsics);
        let bell = Arc::new(Bell {
            doors: Arc::clone(doors),
            home: AtomicUsize::new(home),
            slot: AtomicUsize::new(0),
            scheduled: AtomicBool::new(false),
            woken: Fifo::default(),
            outstanding: AtomicUsize::new(0),
            faults: Fifo::default(),
            idle: Fifo::default(),
            weight: AtomicUsize::new(1),
            spent: AtomicU64::new(0),
            handle: isolate.handle(),
        });
        let state = JsState { bell: Arc::clone(&bell), isolate, engine, cx, pending: Vec::new(), holes: Vec::new(), last: None, platform, due: BinaryHeap::new(), armed: BTreeMap::new() };
        Box::new(Self { tasks: Queue::default(), js: Arc::new(Js { state: UnsafeCell::new(state) }), bell, submitted: Arc::default() })
    }

    fn has_work(&self) -> bool {
        !self.tasks.is_empty() || !self.submitted.is_empty() || !self.bell.woken.is_empty() || self.next_due().is_some_and(|at| at <= self.now())
    }

    fn state(&self) -> &JsState<'p> {
        unsafe { self.js.state() }
    }

    fn next_due(&self) -> Option<Duration> {
        self.state().next_due()
    }

    fn now(&self) -> Duration {
        self.state().platform.now()
    }

    fn take_in(&mut self) {
        let st = unsafe { self.js.state() };
        let tasks = &mut self.tasks;
        while let Some(Submitted { text, file, kind, reply }) = self.submitted.pop() {
            let js = Arc::clone(&self.js);
            st.chain(|t| tasks.add(t), |after| Box::new(ScriptTask { js, after, script: Some((text, file, kind)), job: None, reply: Some(reply) }));
        }
        st.take_woken(&self.js, |t| drop(tasks.add(t)));
        st.fire_due(&self.js, |t| tasks.add(t));
    }

    fn turn(&mut self, budget: NonZeroUsize, check: NonZeroUsize) -> u64 {
        if self.bell.handle.0.has_interrupt() {
            let st = unsafe { self.js.state() };
            let JsState { isolate, engine, cx, .. } = st;
            isolate.enter(cx, engine, |a| engine.serve_interrupts(a));
        }
        self.take_in();
        self.tasks.set_slice(check);
        self.tasks.set_budget(budget);
        match self.tasks.run() {
            Ok(ran) => ran.spent as u64,
            Err(fault) => panic!("an event loop task broke the task queue's contract: {fault:?}"),
        }
    }

    fn enter<R>(&mut self, f: impl for<'scope> FnOnce(&mut Scope<'scope, '_>) -> R) -> R {
        let st = unsafe { self.js.state() };
        let r = st.enter(f);
        let tasks = &mut self.tasks;
        st.adopt(&self.js, |t| drop(tasks.add(t)));
        r
    }
}

type WithLane<'a, 'p> = Box<dyn FnOnce(&mut Lane<'p>) + Send + 'a>;

type OnThread<'a, 'p> = Box<dyn FnOnce(&mut LoopState<'p>) + Send + 'a>;

enum Msg<'p> {
    Spawn(OnThread<'p, 'p>),
    With(Arc<Bell>, WithLane<'p, 'p>),
    Arrive(Box<Lane<'p>>),
    Give { n: usize, to: usize },
    Remove(Arc<Bell>),
}

impl Msg<'_> {
    fn bell(&self) -> Option<&Arc<Bell>> {
        match self {
            Self::With(b, _) | Self::Remove(b) => Some(b),
            Self::Arrive(l) => Some(&l.bell),
            Self::Spawn(_) | Self::Give { .. } => None,
        }
    }
}

struct Port<'p> {
    inbox: Fifo<Msg<'p>>,
    door: Arc<Door>,
}

impl<'p> Port<'p> {
    fn send(&self, m: Msg<'p>) {
        self.inbox.push(m);
        self.door.unpark();
    }
}

struct Set<'p> {
    platform: &'p dyn Platform,
    doors: Arc<Doors>,
    ports: SegVec<Arc<Port<'p>>>,
    growing: AtomicBool,
    next: AtomicUsize,
    slice: AtomicUsize,
    check: AtomicUsize,
}

impl<'p> Set<'p> {
    fn new(platform: &'p dyn Platform) -> Self {
        Self { platform, doors: Arc::default(), ports: SegVec::default(), growing: AtomicBool::new(false), next: AtomicUsize::new(0), slice: AtomicUsize::new(SLICE), check: AtomicUsize::new(CHECK) }
    }

    fn port(&self, i: usize) -> &Arc<Port<'p>> {
        self.ports.get(i).expect("a thread of the set")
    }

    fn threads(&self) -> usize {
        self.ports.len()
    }

    fn grow(&self) -> usize {
        while self.growing.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            core::hint::spin_loop();
        }
        let door = Arc::new(Door::new());
        let i = self.doors.0.push(Arc::clone(&door));
        let j = self.ports.push(Arc::new(Port { inbox: Fifo::default(), door }));
        self.growing.store(false, Ordering::Release);
        debug_assert_eq!(i, j, "doors and ports grow together");
        i
    }

    fn least_loaded(&self) -> usize {
        let n = self.threads();
        let start = self.next.fetch_add(1, Ordering::Relaxed);
        (0..n).map(|k| (start + k) % n).min_by_key(|&i| self.doors.get(i).load()).expect("at least one thread")
    }

    fn call<R: Send>(&self, bell: &Arc<Bell>, f: impl FnOnce(&mut Lane<'p>) -> R + Send) -> R {
        let (tx, rx) = Oneshot::new();
        let reply = Reply(Some(tx));
        let with: WithLane<'_, 'p> = Box::new(move |l| reply.send(f(l)));
        let with = unsafe { core::mem::transmute::<WithLane<'_, 'p>, WithLane<'p, 'p>>(with) };
        self.port(bell.home.load(Ordering::Acquire)).send(Msg::With(Arc::clone(bell), with));
        block_on(rx).expect("the event loop's thread has shut down")
    }

    fn give(&self, from: usize, to: usize, n: usize) {
        if n > 0 && from != to {
            self.port(from).send(Msg::Give { n, to });
        }
    }
}

struct LoopState<'p> {
    me: usize,
    set: Arc<Set<'p>>,
    lanes: Vec<Option<Box<Lane<'p>>>>,
    holes: Vec<usize>,
    ready: VecDeque<Arc<Bell>>,
    early: Vec<Msg<'p>>,
}

impl<'p> LoopState<'p> {
    fn new(me: usize, set: Arc<Set<'p>>) -> Self {
        Self { me, set, lanes: Vec::new(), holes: Vec::new(), ready: VecDeque::new(), early: Vec::new() }
    }

    fn door(&self) -> &Door {
        self.set.doors.get(self.me)
    }

    fn lane(&mut self, bell: &Arc<Bell>) -> Option<&mut Box<Lane<'p>>> {
        if bell.home.load(Ordering::Acquire) != self.me {
            return None;
        }
        self.lanes.get_mut(bell.slot.load(Ordering::Acquire))?.as_mut().filter(|l| Arc::ptr_eq(&l.bell, bell))
    }

    fn schedule(&mut self, bell: &Arc<Bell>) {
        if !bell.scheduled.swap(true, Ordering::AcqRel) {
            self.ready.push_back(Arc::clone(bell));
        }
    }

    fn insert(&mut self, lane: Box<Lane<'p>>) -> Arc<Bell> {
        let bell = Arc::clone(&lane.bell);
        let slot = self.holes.pop().unwrap_or(self.lanes.len());
        bell.slot.store(slot, Ordering::Release);
        bell.home.store(self.me, Ordering::Release);
        let work = lane.has_work();
        match self.lanes.get_mut(slot) {
            Some(hole) => *hole = Some(lane),
            None => self.lanes.push(Some(lane)),
        }
        self.door().lanes.fetch_add(1, Ordering::Relaxed);
        if work {
            bell.scheduled.store(true, Ordering::Release);
            self.ready.push_back(Arc::clone(&bell));
        }
        let early = core::mem::take(&mut self.early);
        for m in early {
            self.handle(m);
        }
        bell
    }

    fn take(&mut self, bell: &Arc<Bell>) -> Option<Box<Lane<'p>>> {
        self.lane(bell)?;
        let slot = bell.slot.load(Ordering::Acquire);
        let lane = self.lanes[slot].take()?;
        self.holes.push(slot);
        self.door().lanes.fetch_sub(1, Ordering::Relaxed);
        Some(lane)
    }

    fn give(&mut self, n: usize, to: usize) {
        let mut moving = Vec::with_capacity(n);
        while moving.len() < n
            && let Some(bell) = self.ready.pop_back()
        {
            if let Some(l) = self.take(&bell) {
                moving.push(l);
            }
        }
        let mut slot = self.lanes.len();
        while moving.len() < n && slot > 0 {
            slot -= 1;
            let bell = self.lanes[slot].as_ref().map(|l| Arc::clone(&l.bell));
            if let Some(bell) = bell
                && let Some(l) = self.take(&bell)
            {
                moving.push(l);
            }
        }
        let port = Arc::clone(self.set.port(to));
        for lane in moving {
            lane.bell.home.store(to, Ordering::Release);
            lane.bell.scheduled.store(false, Ordering::Release);
            port.send(Msg::Arrive(lane));
        }
        self.door().asked.store(false, Ordering::Release);
    }

    fn handle(&mut self, m: Msg<'p>) {
        if let Some(bell) = m.bell()
            && !matches!(m, Msg::Arrive(_))
            && self.lane(bell).is_none()
        {
            let home = bell.home.load(Ordering::Acquire);
            if home == self.me {
                self.early.push(m);
            } else {
                Arc::clone(self.set.port(home)).send(m);
            }
            return;
        }
        match m {
            Msg::Spawn(f) => f(self),
            Msg::With(bell, f) => {
                if let Some(l) = self.lane(&bell) {
                    f(l);
                    if l.has_work() {
                        self.schedule(&bell);
                    }
                }
            }
            Msg::Arrive(lane) => {
                self.insert(lane);
            }
            Msg::Give { n, to } => self.give(n, to),
            Msg::Remove(bell) => {
                self.take(&bell);
            }
        }
    }

    fn pump(&mut self) {
        let port = Arc::clone(self.set.port(self.me));
        while let Some(m) = port.inbox.pop() {
            self.handle(m);
        }
        while let Some(bell) = port.door.wakes.pop() {
            let home = bell.home.load(Ordering::Acquire);
            if self.lane(&bell).is_some() {
                self.ready.push_back(bell);
            } else if home == self.me {
                bell.scheduled.store(false, Ordering::Release);
            } else {
                let door = self.set.doors.get(home);
                door.wakes.push(bell);
                door.unpark();
            }
        }
        self.door().queued.store(self.ready.len(), Ordering::Relaxed);
    }

    fn turn(&mut self) -> bool {
        let Some(bell) = self.ready.pop_front() else { return false };
        let (slice, check) = (self.set.slice.load(Ordering::Relaxed), self.set.check.load(Ordering::Relaxed));
        let Some(l) = self.lane(&bell) else { return true };
        l.bell.scheduled.store(false, Ordering::Release);
        let budget = slice.saturating_mul(l.bell.weight.load(Ordering::Relaxed)).max(1);
        let spent = l.turn(NonZeroUsize::new(budget).unwrap_or(NonZeroUsize::MIN), NonZeroUsize::new(check).unwrap_or(NonZeroUsize::MIN));
        l.bell.spent.fetch_add(spent, Ordering::Relaxed);
        let work = l.has_work();
        self.door().spent.fetch_add(spent, Ordering::Relaxed);
        if work {
            self.schedule(&bell);
        }
        true
    }

    fn wake_due(&mut self) -> Option<Duration> {
        let now = self.set.platform.now();
        let mut next: Option<Duration> = None;
        let mut due = Vec::new();
        for lane in self.lanes.iter().flatten() {
            match lane.next_due() {
                Some(at) if at <= now => due.push(Arc::clone(&lane.bell)),
                Some(at) => next = Some(next.map_or(at, |n| n.min(at))),
                None => {}
            }
        }
        for bell in due {
            self.schedule(&bell);
        }
        next
    }

    fn pull(&self) {
        let n = self.set.threads();
        if n < 2 {
            return;
        }
        let Some(busiest) = (0..n).filter(|&i| i != self.me).max_by_key(|&i| self.set.doors.get(i).load()) else { return };
        let them = self.set.doors.get(busiest);
        let waiting = them.queued.load(Ordering::Relaxed);
        let (theirs, mine) = (them.lanes.load(Ordering::Relaxed), self.door().lanes.load(Ordering::Relaxed));
        let want = if waiting >= PULL_GAP { waiting / 2 } else if theirs >= mine + PULL_GAP { (theirs - mine) / 2 } else { 0 };
        if want > 0 && !them.asked.swap(true, Ordering::AcqRel) {
            self.set.give(busiest, self.me, want);
        }
    }

    fn serve(&mut self, mut done: impl FnMut(&mut Self) -> bool) {
        self.door().watch();
        loop {
            self.pump();
            if done(self) {
                return;
            }
            if self.turn() {
                continue;
            }
            self.pull();
            let next = self.wake_due();
            if !self.ready.is_empty() {
                continue;
            }
            let port = self.set.port(self.me);
            if port.inbox.is_empty() && port.door.wakes.is_empty() {
                match next {
                    Some(at) => std::thread::park_timeout(at.saturating_sub(self.set.platform.now())),
                    None => std::thread::park(),
                }
            }
        }
    }
}

pub struct Ticket<'p>(Receiver<Result<ScopeGlobal<'p, ValueBrand>, Error>>);

impl<'p> Ticket<'p> {
    pub fn try_take(&mut self) -> Option<Result<ScopeGlobal<'p, ValueBrand>, Error>> {
        match Pin::new(&mut self.0).poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(r) => Some(r),
            Poll::Pending => None,
        }
    }
}

pub struct EventLoop<'p> {
    bell: Arc<Bell>,
    submitted: Arc<Fifo<Submitted<'p>>>,
    interrupts: Interrupts<'p>,
    set: Arc<Set<'p>>,
}

impl<'p> EventLoop<'p> {
    pub fn submit(&self, src: &Source<'_>, kind: ScriptKind) -> Ticket<'p> {
        let (reply, rx) = Oneshot::new();
        self.bell.begin();
        self.submitted.push(Submitted { text: src.text.into(), file: src.file.into(), kind, reply });
        self.bell.ring();
        Ticket(rx)
    }

    pub fn wait(&self, ticket: Ticket<'p>) -> Result<ScopeGlobal<'p, ValueBrand>, Error> {
        block_on(ticket.0)
    }

    pub fn eval(&self, src: &Source<'_>, kind: ScriptKind) -> Result<ScopeGlobal<'p, ValueBrand>, Error> {
        let ticket = self.submit(src, kind);
        self.wait(ticket)
    }

    pub fn run(&self) -> Result<(), Error> {
        let bell = &self.bell;
        while !bell.quiet() {
            bell.idle.push(std::thread::current());
            if bell.quiet() {
                break;
            }
            std::thread::park();
        }
        bell.faults.pop().map_or(Ok(()), Err)
    }

    pub fn enter<R: Send>(&self, f: impl for<'scope> FnOnce(&mut Scope<'scope, '_>) -> R + Send) -> R {
        self.set.call(&self.bell, move |l| l.enter(f))
    }

    pub fn set_weight(&self, weight: NonZeroUsize) {
        self.bell.weight.store(weight.get(), Ordering::Relaxed);
    }

    pub fn spent(&self) -> u64 {
        self.bell.spent.load(Ordering::Relaxed)
    }

    pub fn thread(&self) -> usize {
        self.bell.home.load(Ordering::Acquire)
    }

    pub fn handle(&self) -> IsolateHandle {
        self.bell.handle.clone()
    }

    pub fn interrupter(&self) -> Interrupter<'p> {
        Interrupter::new(Arc::clone(&self.bell), Arc::clone(&self.interrupts))
    }

    pub fn set_heap_limit(&self, limit: Option<NonZeroUsize>, callback: Option<HeapLimitCallback<'p>>) {
        let interrupter = self.interrupter();
        self.set.call(&self.bell, move |l| {
            let interrupt = move || interrupter.interrupt(|exe, a| if exe.is_running() { a.check_heap_limit() });
            unsafe { l.js.state() }.isolate.set_heap_limit(limit, callback, interrupt);
        });
    }

    pub fn heap_used(&self) -> usize {
        self.set.call(&self.bell, |l| unsafe { l.js.state() }.isolate.heap_used())
    }
}

impl Drop for EventLoop<'_> {
    fn drop(&mut self) {
        self.set.port(self.bell.home.load(Ordering::Acquire)).send(Msg::Remove(Arc::clone(&self.bell)));
    }
}

pub struct EventLoopSet<'s, 'p> {
    set: Arc<Set<'p>>,
    scope: &'s std::thread::Scope<'s, 'p>,
}

struct Close<'p>(Arc<Set<'p>>);

impl Drop for Close<'_> {
    fn drop(&mut self) {
        for door in self.0.doors.0.iter() {
            door.closing.store(true, Ordering::Release);
            door.unpark();
        }
    }
}

impl<'p> EventLoopSet<'_, 'p> {
    pub fn scope<R>(platform: &'p dyn Platform, threads: NonZeroUsize, body: impl for<'a> FnOnce(&EventLoopSet<'a, 'p>) -> R) -> R {
        let set = Arc::new(Set::new(platform));
        std::thread::scope::<'p, _, _>(|scope| {
            let lp = EventLoopSet { set: Arc::clone(&set), scope };
            for _ in 0..threads.get() {
                lp.launch();
            }
            let _close = Close(Arc::clone(&set));
            body(&lp)
        })
    }

    fn launch(&self) -> usize {
        let me = self.set.grow();
        let set = Arc::clone(&self.set);
        self.scope.spawn(move || {
            let mut state = LoopState::new(me, Arc::clone(&set));
            state.serve(|s| s.door().closing.load(Ordering::Acquire));
        });
        me
    }

    pub fn threads(&self) -> usize {
        self.set.threads()
    }

    pub fn loads(&self) -> Vec<usize> {
        (0..self.threads()).map(|i| self.set.doors.get(i).load()).collect()
    }

    pub fn spent(&self) -> Vec<u64> {
        (0..self.threads()).map(|i| self.set.doors.get(i).spent.load(Ordering::Relaxed)).collect()
    }

    pub fn set_slice(&self, steps: NonZeroUsize) {
        self.set.slice.store(steps.get(), Ordering::Relaxed);
    }

    pub fn set_check(&self, steps: NonZeroUsize) {
        self.set.check.store(steps.get(), Ordering::Relaxed);
    }

    pub fn spawn(&self, engine: Executor<'p>, intrinsics: &(dyn Intrinsics + Sync)) -> EventLoop<'p> {
        let home = self.set.least_loaded();
        let (tx, rx) = Oneshot::new();
        let reply = Reply(Some(tx));
        let platform = self.set.platform;
        let interrupts = Arc::clone(&engine.interrupts);
        let make: OnThread<'_, 'p> = Box::new(move |s| {
            let lane = Lane::new(platform, engine, intrinsics, &s.set.doors, s.me);
            let submitted = Arc::clone(&lane.submitted);
            reply.send((s.insert(lane), submitted));
        });
        let make = unsafe { core::mem::transmute::<OnThread<'_, 'p>, OnThread<'p, 'p>>(make) };
        self.set.port(home).send(Msg::Spawn(make));
        let (bell, submitted) = block_on(rx).expect("the loop thread has shut down");
        EventLoop { bell, submitted, interrupts, set: Arc::clone(&self.set) }
    }

    pub fn rebalance(&self) -> usize {
        let mut loads = self.loads();
        let n = loads.len();
        if n < 2 {
            return 0;
        }
        let avg = loads.iter().sum::<usize>().div_ceil(n);
        let mut moved = 0;
        for from in 0..n {
            for to in 0..n {
                if loads[from] <= avg {
                    break;
                }
                let k = (loads[from] - avg).min(avg.saturating_sub(loads[to]));
                if k > 0 {
                    self.set.give(from, to, k);
                    loads[from] -= k;
                    loads[to] += k;
                    moved += k;
                }
            }
        }
        moved
    }

    pub fn split(&self, thread: usize) -> usize {
        let new = self.launch();
        let half = self.set.doors.get(thread).lanes.load(Ordering::Relaxed) / 2;
        self.set.give(thread, new, half);
        new
    }
}
