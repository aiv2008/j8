use alloc::sync::Arc;
use core::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Default)]
#[repr(transparent)]
pub struct Termination(AtomicU8);

const REQUESTED: u8 = 1;
const UNWINDING: u8 = 2;
const TERMINATE: u8 = REQUESTED | UNWINDING;
const INTERRUPT: u8 = 4;
const COLLECT: u8 = 8;

impl Termination {
    pub fn request(&self) {
        let _ = self.0.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |s| (s & TERMINATE == 0).then_some(s | REQUESTED));
    }

    pub fn cancel(&self) {
        self.0.fetch_and(!TERMINATE, Ordering::SeqCst);
    }

    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::Relaxed) != 0
    }

    pub fn is_terminating(&self) -> bool {
        self.0.load(Ordering::Relaxed) & TERMINATE != 0
    }

    pub fn observe(&self) -> bool {
        if !self.is_set() {
            return false;
        }
        let _ = self.0.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |s| (s & TERMINATE == REQUESTED).then_some(s & !TERMINATE | UNWINDING));
        self.is_terminating()
    }

    pub fn finish(&self) {
        let _ = self.0.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |s| (s & TERMINATE == UNWINDING).then_some(s & !TERMINATE));
    }

    pub fn interrupt(&self) {
        self.0.fetch_or(INTERRUPT, Ordering::SeqCst);
    }

    pub fn has_interrupt(&self) -> bool {
        self.0.load(Ordering::Relaxed) & INTERRUPT != 0
    }

    pub fn take_interrupt(&self) -> bool {
        self.0.fetch_and(!INTERRUPT, Ordering::SeqCst) & INTERRUPT != 0
    }

    pub fn request_collect(&self) {
        self.0.fetch_or(COLLECT, Ordering::SeqCst);
    }

    pub fn take_collect(&self) -> bool {
        self.0.fetch_and(!COLLECT, Ordering::SeqCst) & COLLECT != 0
    }

    pub fn as_ptr(&self) -> *const u8 {
        self.0.as_ptr().cast_const()
    }
}

#[derive(Clone, Debug)]
pub struct IsolateHandle(pub Arc<Termination>);

impl IsolateHandle {
    pub fn terminate(&self) {
        self.0.request();
    }

    pub fn cancel_terminate(&self) {
        self.0.cancel();
    }

    pub fn is_terminating(&self) -> bool {
        self.0.is_terminating()
    }
}
