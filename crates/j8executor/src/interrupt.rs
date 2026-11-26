use alloc::boxed::Box;
use alloc::sync::Arc;

use j8framework::Scope;
use j8lockfree::Fifo;

use crate::event_loop::Bell;
use crate::Executor;

pub(crate) type Interrupt<'p> = Box<dyn for<'e, 'scope, 'x> FnOnce(&Executor<'e>, &mut Scope<'scope, 'x>) + Send + 'p>;

pub(crate) type Interrupts<'p> = Arc<Fifo<Interrupt<'p>>>;

#[derive(Clone)]
pub struct Interrupter<'p> {
    bell: Arc<Bell>,
    queue: Interrupts<'p>,
}

impl<'p> Interrupter<'p> {
    pub(crate) fn new(bell: Arc<Bell>, queue: Interrupts<'p>) -> Self {
        Self { bell, queue }
    }

    pub fn interrupt(&self, f: impl for<'e, 'scope, 'x> FnOnce(&Executor<'e>, &mut Scope<'scope, 'x>) + Send + 'p) {
        self.queue.push(Box::new(f));
        self.bell.interrupt();
    }
}

impl Executor<'_> {
    pub(crate) fn serve_interrupts(&self, a: &mut Scope<'_, '_>) {
        if !a.stop.take_interrupt() {
            return;
        }
        while let Some(f) = self.interrupts.pop() {
            f(self, a);
        }
    }
}
