use alloc::sync::Arc;
use core::num::NonZeroUsize;
use core::cell::{Cell, RefCell};

use j8lockfree::Fifo;

use crate::{AsyncJob, Heap, Scope, IsolateHandle, Mutation, Platform, Profiler, ScopeGlobal, Termination, ValueBrand, rebrand, rebrand_ref};
#[cfg(all(target_os = "linux", feature = "crossheap"))]
use crate::{Erased, ScopeLocal, erase};

#[cfg(all(target_os = "linux", feature = "crossheap"))]
paragc::brand!(pub ErasedBrand = for<'heap, 'scope> Erased);

#[cfg(all(target_os = "linux", feature = "crossheap"))]
#[derive(Default)]
pub struct Shares<'i> {
    cross: Option<paragc::CrossHeap<'i>>,
    next: u64,
    pending: Vec<(u64, ScopeGlobal<'i, ErasedBrand>)>,
    signed: Vec<(u64, Option<paragc::Signature>)>,
}

#[cfg(all(target_os = "linux", feature = "crossheap"))]
impl<'i> Shares<'i> {
    pub fn share<'scope, T>(&mut self, mc: &Mutation<'scope>, gc: ScopeLocal<'scope, T>) -> u64 {
        let ticket = self.next;
        self.next += 1;
        self.pending.push((ticket, mc.stash::<ErasedBrand>(erase(gc))));
        ticket
    }

    fn due(&self, heap: &Heap<'i>) -> bool {
        self.cross.is_some() && (heap.promotion_pending() || !self.pending.is_empty())
    }

    fn settle(&mut self, heap: &mut Heap<'i>) {
        let Some(cross) = &self.cross else { return };
        let pending = core::mem::take(&mut self.pending);
        let signed: Vec<_> = heap.scope(|m| cross.scope(|cm| pending.iter().map(|(ticket, root)| (*ticket, cm.sign_promoted(m.scope_in(root)))).collect()));
        self.signed.extend(signed);
    }
}

#[derive(Default)]
pub struct Parked<'i> {
    next: u64,
    held: Vec<(u64, ScopeGlobal<'i, ValueBrand>)>,
}

impl<'i> Parked<'i> {
    pub fn park(&mut self, root: ScopeGlobal<'i, ValueBrand>) -> u64 {
        let ticket = self.next;
        self.next += 1;
        self.held.push((ticket, root));
        ticket
    }

    pub fn unpark(&mut self, ticket: u64) -> Option<ScopeGlobal<'i, ValueBrand>> {
        let at = self.held.iter().position(|(t, _)| *t == ticket)?;
        Some(self.held.swap_remove(at).1)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CreateParams {
    pub profile: bool,
    pub event_loop: bool,
    pub collector_threads: Option<NonZeroUsize>,
    pub stack_size: Option<NonZeroUsize>,
    pub collect_threshold: Option<NonZeroUsize>,
}

impl CreateParams {
    pub const DEFAULT_STACK_SIZE: usize = 984 << 10;
}

const COLLECT_FRACTION: usize = 2;

pub type HeapLimitCallback<'p> = Box<dyn FnMut(usize, usize) -> usize + Send + 'p>;

pub(crate) struct HeapLimit<'p> {
    current: Cell<usize>,
    initial: usize,
    callback: RefCell<Option<HeapLimitCallback<'p>>>,
    collected_at: Cell<usize>,
    pending: Cell<bool>,
}

impl HeapLimit<'_> {
    pub(crate) fn exceeded(&self, used: usize) -> bool {
        let mut current = self.current.get();
        if used <= current {
            return false;
        }
        if let Some(callback) = self.callback.borrow_mut().as_mut() {
            current = callback(current, self.initial);
            self.current.set(current);
        }
        used > current
    }

    pub(crate) fn current(&self) -> usize {
        self.current.get()
    }

    pub(crate) fn pending(&self) -> &Cell<bool> {
        &self.pending
    }
}

pub struct Isolate<'p> {
    platform: &'p dyn Platform,
    heap: Heap<'p>,
    generation: u64,
    profiler: Profiler,
    spawned: Option<Fifo<AsyncJob<'p>>>,
    timers: Option<crate::Timers<'p>>,
    stop: Arc<Termination>,
    exception: Option<ScopeGlobal<'p, ValueBrand>>,
    parked: core::cell::RefCell<Parked<'p>>,
    heap_limit: Option<HeapLimit<'p>>,
    stack_size: usize,
    #[cfg(all(target_os = "linux", feature = "crossheap"))]
    shares: Shares<'p>,
}

impl<'p> Isolate<'p> {
    pub fn new(platform: &'p dyn Platform, params: CreateParams) -> Self {
        let mut profiler = Profiler::default();
        profiler.enable(params.profile);
        let mut heap = unsafe { Heap::new_unchecked(params.collector_threads.unwrap_or_else(|| NonZeroUsize::new(platform.threads()).unwrap_or(NonZeroUsize::MIN))) };
        heap.set_parallel_drop(false);
        let stop: Arc<Termination> = Arc::default();
        let due = Arc::clone(&stop);
        heap.on_collect_needed(move || due.request_collect());
        heap.set_collect_threshold(params.collect_threshold);
        Self {
            platform,
            heap,
            generation: 0,
            profiler,
            spawned: params.event_loop.then(Fifo::default),
            timers: params.event_loop.then(crate::Timers::default),
            stop,
            exception: None,
            parked: core::cell::RefCell::default(),
            heap_limit: None,
            stack_size: params.stack_size.map_or(CreateParams::DEFAULT_STACK_SIZE, NonZeroUsize::get),
            #[cfg(all(target_os = "linux", feature = "crossheap"))]
            shares: Shares::default(),
        }
    }

    #[cfg(all(target_os = "linux", feature = "crossheap"))]
    pub fn attach_cross_heap(&mut self, cross: paragc::CrossHeap<'p>) -> std::io::Result<()> {
        self.heap.attach_cross_heap(&cross)?;
        self.shares.cross = Some(cross);
        Ok(())
    }

    #[cfg(all(target_os = "linux", feature = "crossheap"))]
    pub fn cross_heap(&self) -> Option<&paragc::CrossHeap<'p>> {
        self.shares.cross.as_ref()
    }

    #[cfg(all(target_os = "linux", feature = "crossheap"))]
    pub fn take_signatures(&mut self) -> Vec<(u64, Option<paragc::Signature>)> {
        core::mem::take(&mut self.shares.signed)
    }

    pub fn handle(&self) -> IsolateHandle {
        IsolateHandle(Arc::clone(&self.stop))
    }

    pub fn gc_cycles(&self) -> u64 {
        self.generation
    }

    pub fn profiler(&self) -> &Profiler {
        &self.profiler
    }

    pub fn take_timers(&self) -> Vec<crate::TimerRequest<'p>> {
        self.timers.as_ref().map(crate::Timers::take).unwrap_or_default()
    }

    pub fn take_spawned(&self) -> Vec<AsyncJob<'p>> {
        self.spawned.as_ref().map(|q| core::iter::from_fn(|| q.pop()).collect()).unwrap_or_default()
    }

    pub fn create_context(&mut self, intrinsics: &dyn crate::Intrinsics) -> ScopeGlobal<'p, crate::ContextBrand> {
        let generation = self.generation;
        let cx = self.heap.scope(|s| {
            let s: &paragc::Scope<'_, '_> = s;
            let mc = unsafe { Mutation::new(rebrand_ref(s), generation) };
            mc.stash::<crate::ContextBrand>(crate::Context::create(&mc, intrinsics))
        });
        self.after_enter();
        cx
    }

    pub fn enter<R>(&mut self, cx: &ScopeGlobal<'p, crate::ContextBrand>, invoke: &dyn for<'s> crate::Invoke<'s>, f: impl for<'scope> FnOnce(&mut Scope<'scope, '_>) -> R) -> R {
        let (platform, profiler, generation, stop, stack_size) = (self.platform, &mut self.profiler, self.generation, &*self.stop, self.stack_size);
        let spawned = self.spawned.as_ref();
        let timers = self.timers.as_ref();
        let parked = &self.parked;
        let heap_limit = self.heap_limit.as_ref();
        #[cfg(all(target_os = "linux", feature = "crossheap"))]
        let shares = &mut self.shares;
        let carried = self.exception.take();
        let mut left = None;
        let mut collected = generation;
        let r = self.heap.scope(|s| {
            let s: &paragc::Scope<'_, '_> = s;
            let mc = unsafe { Mutation::new(rebrand_ref(s), generation) };
            if let Some(e) = &carried {
                let v = *mc.fetch(e);
                let _ = mc.raise(v);
            }
            let r = f(&mut Scope {
                mc: &mc,
                platform,
                profiler,
                context: mc.fetch(cx),
                new_target: crate::Value::Undefined,
                invoke,
                spawned: spawned.map(|q| unsafe { rebrand_ref(q) }),
                timers: timers.map(|t| unsafe { rebrand_ref(t) }),
                stop,
                parked: unsafe { rebrand_ref(parked) },
                spare_regs: Vec::new(),
                heap_limit: heap_limit.map(|l| unsafe { rebrand_ref(l) }),
                stack_limit: Scope::stack_limit_here(stack_size),
                joining: crate::Visiting::default(),
                gc_safe: core::cell::Cell::new(false),
                handles: Vec::new(),
                #[cfg(all(target_os = "linux", feature = "crossheap"))]
                shares: unsafe { &mut *core::ptr::from_mut(shares).cast() },
            });
            if let Some(v) = mc.take_exception() {
                left = Some(mc.stash::<ValueBrand>(mc.alloc(v)));
            }
            collected = mc.generation();
            r
        });
        self.generation = collected;
        self.exception = left.map(|g| unsafe { rebrand(g) });
        self.after_enter();
        r
    }

    fn after_enter(&mut self) {
        self.stop.finish();
        #[cfg(all(target_os = "linux", feature = "crossheap"))]
        if self.shares.due(&self.heap) {
            self.heap.collect_young();
            self.generation += 1;
            self.shares.settle(&mut self.heap);
        }
        if self.heap.collect_if_needed() {
            self.generation += 1;
        }
        self.rearm_heap_limit();
    }

    pub fn heap_used(&self) -> usize {
        self.heap.used()
    }

    pub fn set_heap_limit(&mut self, limit: Option<NonZeroUsize>, callback: Option<HeapLimitCallback<'p>>, interrupt: impl Fn() + Send + Sync + 'p) {
        let used = self.heap_used();
        self.heap_limit = limit.map(|l| HeapLimit { current: Cell::new(l.get()), initial: l.get(), callback: RefCell::new(callback), collected_at: Cell::new(used), pending: Cell::new(false) });
        self.heap.on_limit(interrupt);
        self.heap.set_limit(limit.map(NonZeroUsize::get));
    }

    fn rearm_heap_limit(&mut self) {
        let Some((current, collected_at)) = self.heap_limit.as_ref().map(|l| (l.current(), l.collected_at.get())) else { return };
        if self.heap_used().saturating_sub(collected_at) >= current / COLLECT_FRACTION {
            self.collect();
            let used = self.heap_used();
            if let Some(limit) = &self.heap_limit {
                limit.collected_at.set(used);
            }
        }
        self.heap.set_limit(Some(current));
    }

    pub fn collect(&mut self) {
        self.heap.collect();
        self.generation += 1;
    }
}
