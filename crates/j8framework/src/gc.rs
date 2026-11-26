use core::cell::UnsafeCell;
use core::fmt;
use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};
use core::ptr::NonNull;

pub use j8macros::Collect;
pub use paragc::{Brand, Heap, Tlab, Trace, Tracer};

use crate::Value;

pub type ScopeLocal<'scope, T> = paragc::ScopeLocal<'scope, 'scope, T>;

pub type Slot<'scope, T> = paragc::Slot<'scope, 'scope, T>;

pub type WordSlot<'scope> = paragc::WordSlot<'scope, 'scope>;

pub type GcScope<'scope> = paragc::Scope<'scope, 'scope>;

pub type Handle<'scope, T> = paragc::Handle<'scope, 'scope, T>;

pub type HandleSlot<'scope, T> = paragc::HandleSlot<'scope, 'scope, T>;

pub type ScopeGlobal<'i, B> = paragc::ScopeGlobal<'i, 'i, B>;

pub type Weak<'i, B> = paragc::Weak<'i, B>;

pub unsafe fn rebrand<From, To>(v: From) -> To {
    const { assert!(size_of::<From>() == size_of::<To>()) };
    let v = ManuallyDrop::new(v);
    unsafe { core::ptr::read(core::ptr::from_ref::<From>(&v).cast::<To>()) }
}

pub unsafe fn rebrand_ref<From, To>(v: &From) -> &To {
    unsafe { &*core::ptr::from_ref(v).cast::<To>() }
}

#[inline]
pub unsafe fn gc_from_raw<'scope, T>(ptr: *const T) -> ScopeLocal<'scope, T> {
    unsafe { ScopeLocal::from_raw(ptr) }
}

pub struct Mutation<'scope> {
    m: NonNull<GcScope<'scope>>,
    generation: core::cell::Cell<u64>,
    pending: core::cell::Cell<Option<Value<'scope>>>,
    rethrown: core::cell::Cell<bool>,
}

impl<'scope> Mutation<'scope> {
    pub unsafe fn new(m: &GcScope<'scope>, generation: u64) -> Self {
        Self { m: NonNull::from(m), generation: core::cell::Cell::new(generation), pending: core::cell::Cell::new(None), rethrown: core::cell::Cell::new(false) }
    }

    pub fn raise(&self, v: Value<'scope>) -> crate::Error {
        self.pending.set(Some(v));
        self.rethrown.set(false);
        crate::Error::Exception
    }

    pub fn rethrow(&self, v: Value<'scope>) -> crate::Error {
        self.pending.set(Some(v));
        self.rethrown.set(true);
        crate::Error::Exception
    }

    pub fn rethrown(&self) -> bool {
        self.rethrown.get()
    }

    pub fn take_exception(&self) -> Option<Value<'scope>> {
        self.pending.take()
    }

    pub fn has_exception(&self) -> bool {
        self.pending.get().is_some()
    }

    #[inline]
    pub fn gc_scope(&self) -> &'scope GcScope<'scope> {
        unsafe { self.m.as_ref() }
    }

    #[inline]
    pub fn alloc<T: Trace + 'scope>(&self, v: T) -> ScopeLocal<'scope, T> {
        self.gc_scope().alloc(v)
    }

    #[inline]
    pub fn generation(&self) -> u64 {
        self.generation.get()
    }

    pub unsafe fn collect_full_here(&self) {
        unsafe { (*self.m.as_ptr()).collect() };
        self.generation.set(self.generation.get() + 1);
    }

    pub unsafe fn collect_here(&self) -> bool {
        let collected = unsafe { (*self.m.as_ptr()).collect_if_needed() };
        if collected {
            self.generation.set(self.generation.get() + 1);
        }
        collected
    }

    pub fn stash<'i, B: Brand>(&self, v: ScopeLocal<'scope, B::Of<'scope, 'scope>>) -> ScopeGlobal<'i, B> {
        let g: paragc::ScopeGlobal<'scope, 'scope, B> = self.gc_scope().scope_out(v);
        unsafe { rebrand(g) }
    }

    pub fn fetch<B: Brand>(&self, g: &ScopeGlobal<'_, B>) -> ScopeLocal<'scope, B::Of<'scope, 'scope>> {
        let g: &paragc::ScopeGlobal<'scope, 'scope, B> = unsafe { rebrand_ref(g) };
        self.gc_scope().scope_in(g)
    }

    pub fn weak<'i, B: Brand>(&self, v: ScopeLocal<'scope, B::Of<'scope, 'scope>>) -> Weak<'i, B> {
        let w: paragc::Weak<'scope, B> = self.gc_scope().weak(v);
        unsafe { rebrand(w) }
    }

    pub fn upgrade<B: Brand>(&self, w: &Weak<'_, B>) -> Option<ScopeLocal<'scope, B::Of<'scope, 'scope>>> {
        let w: &paragc::Weak<'scope, B> = unsafe { rebrand_ref(w) };
        self.gc_scope().upgrade(w)
    }
}

pub struct Erased(());

unsafe impl Trace for Erased {
    fn trace(&self, _: &mut Tracer<'_>) {}

    fn relocate(&mut self, _: &mut Tracer<'_>) {}
}

#[inline]
pub fn erase<T>(g: ScopeLocal<'_, T>) -> ScopeLocal<'_, Erased> {
    unsafe { gc_from_raw(g.as_ptr().cast()) }
}

#[derive(Default)]
#[repr(transparent)]
pub struct IsoCell<T>(core::cell::Cell<T>);

unsafe impl<T: Copy> Sync for IsoCell<T> {}

unsafe impl<T: Copy> Send for IsoCell<T> {}

impl<T: Copy> IsoCell<T> {
    pub const fn new(v: T) -> Self {
        Self(core::cell::Cell::new(v))
    }

    #[inline]
    pub fn get(&self) -> T {
        self.0.get()
    }

    #[inline]
    pub fn set(&self, v: T) {
        self.0.set(v);
    }

    #[inline]
    pub fn get_mut(&mut self) -> &mut T {
        self.0.get_mut()
    }
}

unsafe impl<T: Copy + Send + 'static> Trace for IsoCell<T> {
    const NO_EDGES: bool = true;
    fn trace(&self, _: &mut Tracer<'_>) {}
}

impl<T: Copy + fmt::Debug> fmt::Debug for IsoCell<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.get(), f)
    }
}

struct Snap<T> {
    generation: u64,
    moved: IsoCell<bool>,
    data: UnsafeCell<ManuallyDrop<T>>,
}

unsafe impl<T: Send + Sync> Sync for Snap<T> {}

impl<T> Drop for Snap<T> {
    fn drop(&mut self) {
        if !*self.moved.get_mut() {
            unsafe { ManuallyDrop::drop(self.data.get_mut()) };
        }
    }
}

unsafe impl<T: Trace> Trace for Snap<T> {
    const NO_EDGES: bool = T::NO_EDGES;

    fn trace(&self, t: &mut Tracer<'_>) {
        if !self.moved.get() {
            unsafe { &*self.data.get() }.trace(t);
        }
    }

    fn relocate(&mut self, t: &mut Tracer<'_>) {
        if !*self.moved.get_mut() {
            self.data.get_mut().relocate(t);
        }
    }
}

const WRITING: usize = 1 << (usize::BITS - 1);

pub struct GcCell<'scope, T> {
    state: IsoCell<usize>,
    snap: Slot<'scope, Snap<T>>,
}

impl<'scope, T: Trace + 'scope> GcCell<'scope, T> {
    pub fn new(mc: &Mutation<'scope>, t: T) -> Self {
        let snap = mc.alloc(Snap { generation: mc.generation(), moved: IsoCell::new(false), data: UnsafeCell::new(ManuallyDrop::new(t)) });
        Self { state: IsoCell::new(0), snap: Slot::from(snap) }
    }

    pub fn empty() -> Self {
        Self { state: IsoCell::new(0), snap: Slot::default() }
    }

    #[inline]
    pub fn has_value(&self) -> bool {
        self.snap.get().is_some()
    }

    fn current(&self) -> ScopeLocal<'scope, Snap<T>> {
        self.snap.get().expect("borrowed an empty GcCell (check has_value first)")
    }

    pub fn try_borrow(&self) -> Option<CellRef<'_, T>> {
        let s = self.state.get();
        if s & WRITING != 0 {
            return None;
        }
        self.state.set(s + 1);
        Some(CellRef { state: &self.state, data: unsafe { &*self.current().get().data.get() } })
    }

    pub fn borrow(&self) -> CellRef<'_, T> {
        self.try_borrow().expect("GcCell already mutably borrowed")
    }

    pub fn try_borrow_mut<'c>(&'c self, mc: &Mutation<'scope>) -> Option<CellMut<'c, T>> {
        self.try_borrow_mut_or(mc, || unreachable!("borrowed an empty GcCell mutably without an initial value"))
    }

    pub fn try_borrow_mut_or<'c>(&'c self, mc: &Mutation<'scope>, init: impl FnOnce() -> T) -> Option<CellMut<'c, T>> {
        if self.state.get() != 0 {
            return None;
        }
        self.state.set(WRITING);
        let Some(mut snap) = self.snap.get() else {
            let snap = mc.alloc(Snap { generation: mc.generation(), moved: IsoCell::new(false), data: UnsafeCell::new(ManuallyDrop::new(init())) });
            self.snap.set(mc.gc_scope(), Some(snap));
            return Some(CellMut { state: &self.state, data: unsafe { &mut *snap.get().data.get() } });
        };
        if snap.generation != mc.generation() {
            let data = unsafe { ManuallyDrop::take(&mut *snap.data.get()) };
            snap.moved.set(true);
            snap = mc.alloc(Snap { generation: mc.generation(), moved: IsoCell::new(false), data: UnsafeCell::new(ManuallyDrop::new(data)) });
            self.snap.set(mc.gc_scope(), Some(snap));
        }
        Some(CellMut { state: &self.state, data: unsafe { &mut *snap.get().data.get() } })
    }

    pub fn borrow_mut<'c>(&'c self, mc: &Mutation<'scope>) -> CellMut<'c, T> {
        self.try_borrow_mut(mc).expect("GcCell already borrowed")
    }

    pub fn get_mut(&mut self) -> &mut T {
        let snap = self.current();
        unsafe { &mut *snap.get().data.get() }
    }
}

unsafe impl<'scope, T: Trace + 'scope> Trace for GcCell<'scope, T> {
    fn trace(&self, t: &mut Tracer<'_>) {
        self.snap.trace(t);
    }

    fn relocate(&mut self, t: &mut Tracer<'_>) {
        self.snap.relocate(t);
    }
}

impl<'scope, T: Trace + fmt::Debug + 'scope> fmt::Debug for GcCell<'scope, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.has_value() {
            return f.write_str("GcCell(<empty>)");
        }
        match self.try_borrow() {
            Some(r) => fmt::Debug::fmt(&*r, f),
            None => f.write_str("GcCell(<borrowed>)"),
        }
    }
}

pub struct CellRef<'c, T> {
    state: &'c IsoCell<usize>,
    data: &'c T,
}

impl<T> Deref for CellRef<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.data
    }
}

impl<T> Drop for CellRef<'_, T> {
    fn drop(&mut self) {
        self.state.set(self.state.get() - 1);
    }
}

pub struct CellMut<'c, T> {
    state: &'c IsoCell<usize>,
    data: &'c mut T,
}

impl<T> Deref for CellMut<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.data
    }
}

impl<T> DerefMut for CellMut<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.data
    }
}

impl<T> Drop for CellMut<'_, T> {
    fn drop(&mut self) {
        self.state.set(0);
    }
}

pub struct ValueCell<'scope>(WordSlot<'scope>);

impl<'scope> ValueCell<'scope> {
    pub fn new(v: Value<'scope>) -> Self {
        Self(WordSlot::new(v.bits()))
    }

    #[inline]
    pub fn get(&self) -> Value<'scope> {
        unsafe { Value::from_bits(self.0.get()) }
    }

    #[inline]
    pub fn set(&self, mc: &Mutation<'scope>, v: Value<'scope>) {
        self.0.set(mc.gc_scope(), v.bits());
    }

    pub fn set_mut(&mut self, v: Value<'scope>) {
        self.0.set_mut(v.bits());
    }
}

unsafe impl Trace for ValueCell<'_> {
    fn trace(&self, t: &mut Tracer<'_>) {
        self.0.trace(t);
    }

    fn relocate(&mut self, t: &mut Tracer<'_>) {
        self.0.relocate(t);
    }
}

impl fmt::Debug for ValueCell<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.get(), f)
    }
}

pub struct Cow<'scope, T>(Slot<'scope, T>);

impl<'scope, T: Trace + 'scope> Cow<'scope, T> {
    pub fn new(mc: &Mutation<'scope>, t: T) -> Self {
        Self(Slot::from(mc.alloc(t)))
    }

    pub fn get(&self) -> &'scope T {
        self.0.get().expect("a Cow always holds a value").get()
    }

    pub fn update_with<R>(&self, mc: &Mutation<'scope>, mut f: impl FnMut(&T) -> (T, R)) -> R {
        let mut cur = self.0.get();
        loop {
            let (next, r) = f(cur.expect("a Cow always holds a value").get());
            match self.0.compare_exchange(mc.gc_scope(), cur, Some(mc.alloc(next))) {
                Ok(_) => return r,
                Err(now) => cur = now,
            }
        }
    }

    pub fn update(&self, mc: &Mutation<'scope>, mut f: impl FnMut(&T) -> T) {
        self.update_with(mc, |t| (f(t), ()));
    }

    pub fn set(&self, mc: &Mutation<'scope>, t: T) {
        self.0.set(mc.gc_scope(), Some(mc.alloc(t)));
    }
}

unsafe impl<'scope, T: Trace + 'scope> Trace for Cow<'scope, T> {
    fn trace(&self, t: &mut Tracer<'_>) {
        self.0.trace(t);
    }

    fn relocate(&mut self, t: &mut Tracer<'_>) {
        self.0.relocate(t);
    }
}

struct QNode<'scope, T> {
    value: Option<T>,
    next: Slot<'scope, QNode<'scope, T>>,
}

unsafe impl<'scope, T: Trace + 'scope> Trace for QNode<'scope, T> {
    fn trace(&self, t: &mut Tracer<'_>) {
        self.value.trace(t);
        self.next.trace(t);
    }

    fn relocate(&mut self, t: &mut Tracer<'_>) {
        self.value.relocate(t);
        self.next.relocate(t);
    }
}

pub struct GcQueue<'scope, T> {
    head: Slot<'scope, QNode<'scope, T>>,
    tail: Slot<'scope, QNode<'scope, T>>,
}

impl<'scope, T: Trace + Clone + 'scope> GcQueue<'scope, T> {
    pub fn new(mc: &Mutation<'scope>) -> Self {
        let dummy = mc.alloc(QNode { value: None, next: Slot::default() });
        Self { head: Slot::from(dummy), tail: Slot::from(dummy) }
    }

    fn node(s: &Slot<'scope, QNode<'scope, T>>) -> ScopeLocal<'scope, QNode<'scope, T>> {
        s.get().expect("the queue always has a dummy node")
    }

    pub fn push(&self, mc: &Mutation<'scope>, value: T) {
        let m = mc.gc_scope();
        let node = mc.alloc(QNode { value: Some(value), next: Slot::default() });
        loop {
            let last = Self::node(&self.tail);
            match last.get().next.get() {
                None => {
                    if last.get().next.compare_exchange(m, None, Some(node)).is_ok() {
                        let _ = self.tail.compare_exchange(m, Some(last), Some(node));
                        return;
                    }
                }
                Some(n) => {
                    let _ = self.tail.compare_exchange(m, Some(last), Some(n));
                }
            }
        }
    }

    pub fn pop(&self, mc: &Mutation<'scope>) -> Option<T> {
        let m = mc.gc_scope();
        loop {
            let first = Self::node(&self.head);
            let last = Self::node(&self.tail);
            let next = first.get().next.get()?;
            if ScopeLocal::ptr_eq(first, last) {
                let _ = self.tail.compare_exchange(m, Some(last), Some(next));
                continue;
            }
            let value = next.get().value.clone();
            if self.head.compare_exchange(m, Some(first), Some(next)).is_ok() {
                return value;
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        Self::node(&self.head).get().next.get().is_none()
    }
}

unsafe impl<'scope, T: Trace + 'scope> Trace for GcQueue<'scope, T> {
    fn trace(&self, t: &mut Tracer<'_>) {
        self.head.trace(t);
        self.tail.trace(t);
    }

    fn relocate(&mut self, t: &mut Tracer<'_>) {
        self.head.relocate(t);
        self.tail.relocate(t);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Static<T: ?Sized>(pub T);

unsafe impl<T: Send + Sync + 'static> Trace for Static<T> {
    const NO_EDGES: bool = true;

    fn trace(&self, _: &mut Tracer<'_>) {}
}

impl<T> From<T> for Static<T> {
    fn from(t: T) -> Self {
        Self(t)
    }
}

impl<T: ?Sized> Deref for Static<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: ?Sized> DerefMut for Static<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

#[macro_export]
macro_rules! leaf {
    ($($ty:ty),* $(,)?) => {$(
        unsafe impl $crate::Trace for $ty {
            const NO_EDGES: bool = true;
            fn trace(&self, _: &mut $crate::Tracer<'_>) {}
        }
    )*};
}

pub trait CellExt<'scope, T> {
    fn borrow(self) -> CellRef<'scope, T>;
    fn try_borrow(self) -> Option<CellRef<'scope, T>>;
    fn borrow_mut(self, mc: &Mutation<'scope>) -> CellMut<'scope, T>;
    fn try_borrow_mut(self, mc: &Mutation<'scope>) -> Option<CellMut<'scope, T>>;
}

impl<'scope, T: Trace + 'scope> CellExt<'scope, T> for ScopeLocal<'scope, GcCell<'scope, T>> {
    #[inline]
    fn borrow(self) -> CellRef<'scope, T> {
        self.get().borrow()
    }

    #[inline]
    fn try_borrow(self) -> Option<CellRef<'scope, T>> {
        self.get().try_borrow()
    }

    #[inline]
    fn borrow_mut(self, mc: &Mutation<'scope>) -> CellMut<'scope, T> {
        self.get().borrow_mut(mc)
    }

    #[inline]
    fn try_borrow_mut(self, mc: &Mutation<'scope>) -> Option<CellMut<'scope, T>> {
        self.get().try_borrow_mut(mc)
    }
}

pub trait Mutable<'scope>: Sized {
    type Cell: Trace + 'scope;
    fn cell(v: Self) -> Self::Cell;
    fn load(cell: &Self::Cell) -> Self;
    fn store(cell: &Self::Cell, mc: &Mutation<'scope>, v: Self);
}

impl<'scope> Mutable<'scope> for Value<'scope> {
    type Cell = ValueCell<'scope>;

    fn cell(v: Self) -> Self::Cell {
        ValueCell::new(v)
    }

    #[inline]
    fn load(cell: &Self::Cell) -> Self {
        cell.get()
    }

    #[inline]
    fn store(cell: &Self::Cell, mc: &Mutation<'scope>, v: Self) {
        cell.set(mc, v);
    }
}

impl<'scope, T: Trace + 'scope> Mutable<'scope> for Option<ScopeLocal<'scope, T>> {
    type Cell = Slot<'scope, T>;

    fn cell(v: Self) -> Self::Cell {
        Slot::from(v)
    }

    #[inline]
    fn load(cell: &Self::Cell) -> Self {
        cell.get()
    }

    #[inline]
    fn store(cell: &Self::Cell, mc: &Mutation<'scope>, v: Self) {
        cell.set(mc.gc_scope(), v);
    }
}

impl<'scope, T: Trace + 'scope> Mutable<'scope> for ScopeLocal<'scope, T> {
    type Cell = Slot<'scope, T>;

    fn cell(v: Self) -> Self::Cell {
        Slot::from(v)
    }

    #[inline]
    fn load(cell: &Self::Cell) -> Self {
        cell.get().expect("a non-optional ScopeLocal field is always set")
    }

    #[inline]
    fn store(cell: &Self::Cell, mc: &Mutation<'scope>, v: Self) {
        cell.set(mc.gc_scope(), Some(v));
    }
}

macro_rules! mutable_scalar {
    ($($t:ty),* $(,)?) => {$(
        impl<'scope> Mutable<'scope> for $t {
            type Cell = IsoCell<$t>;

            fn cell(v: Self) -> Self::Cell {
                IsoCell::new(v)
            }

            #[inline]
            fn load(cell: &Self::Cell) -> Self {
                cell.get()
            }

            #[inline]
            fn store(cell: &Self::Cell, _: &Mutation<'scope>, v: Self) {
                cell.set(v);
            }
        }
    )*};
}

mutable_scalar!(bool, u8, u16, u32, u64, usize, i32, i64, f64);
