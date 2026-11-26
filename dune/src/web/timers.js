// Timers: setTimeout / setInterval / clearTimeout / clearInterval, queueMicrotask.
// Each armed timer waits on a native Timer: an entry in the event loop's own timer queue, on the loop's thread. The loop
// fires each due timer as a task of its own, earliest due first, and drains microtasks after it. An unreferenced timer
// does not keep the loop alive. The Node layer builds its Timeout objects on `native.timers`.
(function (native) {
  'use strict';
  const { clock, Timer } = native;
  const active = new Map();
  let lastId = 0;

  function wait(t, ms) {
    if (t.wait) t.wait.cancel();
    const w = (t.wait = new Timer());
    w.wait(Math.max(0, ms), t.ref).then(() => fire(t, w));
  }

  function arm(t) {
    t.due = clock.now() + t.delay;
    wait(t, t.delay);
  }

  /** The event loop fires each timer as a task of its own, earliest due first */
  function fire(t, w) {
    if (t.wait !== w || active.get(t.id) !== t) return;
    t.wait = null;
    if (t.repeat) arm(t);
    else active.delete(t.id);
    try {
      t.callback.apply(globalThis, t.args);
    } catch (e) {
      native.reportError(e);
    }
  }

  const timers = {
    add(callback, delay, args, repeat) {
      if (typeof callback !== 'function') throw new TypeError('The "callback" argument must be of type function');
      delay = Number(delay);
      if (!(delay >= 1 && delay <= 2147483647)) delay = repeat ? 1 : 0;
      const t = { id: ++lastId, callback, args, delay, repeat, ref: true, wait: null, due: 0 };
      active.set(t.id, t);
      arm(t);
      return t.id;
    },
    clear(id) {
      const t = active.get(id);
      if (!t) return;
      if (t.wait) t.wait.cancel();
      t.wait = null;
      active.delete(id);
    },
    has(id) { return active.has(id); },
    /** An unreferenced timer does not keep the process alive; it still fires if something else does */
    ref(id, on) {
      const t = active.get(id);
      if (!t || t.ref === on) return;
      t.ref = on;
      wait(t, t.due - clock.now());
    },
    refresh(id) { const t = active.get(id); if (t) arm(t); },
  };

  const define = (name, value) => Object.defineProperty(globalThis, name, { value, writable: true, configurable: true });
  define('setTimeout', (callback, delay, ...args) => timers.add(callback, delay, args, false));
  define('setInterval', (callback, delay, ...args) => timers.add(callback, delay, args, true));
  define('clearTimeout', (id) => timers.clear(+id));
  define('clearInterval', (id) => timers.clear(+id));
  define('queueMicrotask', (callback) => {
    if (typeof callback !== 'function') throw new TypeError('The "callback" argument must be of type function');
    Promise.resolve().then(() => { try { callback(); } catch (e) { native.reportError(e); } });
  });
  native.timers = timers;
})(globalThis.__j8_native);
