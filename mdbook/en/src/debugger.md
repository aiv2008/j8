# Debugger

`j8executor` has a debugger for embedders: breakpoints by script and line, pausing on exceptions and on `debugger`
statements, stepping into, over and out, the call frames with their scopes and variables, and evaluating an expression
in a paused frame. It is enough to build the Chrome DevTools Protocol's Debugger and Runtime domains on.

## Attaching

```rust,ignore
struct Inspector;

impl j8executor::Debugger for Inspector {
    fn paused(&self, p: &mut j8executor::Paused<'_, '_, '_, '_>) -> j8executor::Action {
        let frames = p.frames();                       // CallSites, innermost first: function, script, line, column
        let scopes = p.scopes(0);                      // Local, then the contexts outward, then Global
        let v = p.evaluate(0, "x + 1");                // in the top frame, with its locals in scope
        j8executor::Action::StepOver                   // or Continue, StepInto, StepOut
    }
}

let inspector = Inspector;
let executor = Executor::default().with_debugger(&inspector);
isolate.enter(&cx, &executor, |scope| {
    let bp = executor.set_breakpoint(scope, "main.js", 7, 0);    // Some(Breakpoint { id, location })
    executor.pause_on_exceptions(PauseOnExceptions::Uncaught);
});
```

- The executor borrows the debugger (`&'p (dyn Debugger + Sync)`), so it outlives the executor.
- `paused` runs on the JS thread, inside the execution, every time it stops: `Paused::reason` says why
  (`Breakpoint(ids)`, `Step`, `Pause`, `Statement` for `debugger;`, `Exception { value, uncaught }`). While it runs,
  JS is stopped; an embedder that talks to a front end (DevTools) runs its message loop inside `paused` and returns the
  action the front end asked for. Values it reads are `Value<'scope>`s of the paused entry, and `Paused::scope` is that
  entry's `Scope`, so they can be read with the usual operations. `frames` are the same `CallSite`s as
  `Scope::stack_trace`.
- `set_breakpoint(scope, script, line, column)` resolves to the first statement at or after (line, column) in the
  functions of the script with that name (lines and columns from 1, as in stack traces) and returns where it landed. A
  script that isn't loaded yet keeps the request: it is resolved when code of that name next runs, and `location` is
  `None` until then. `clear_breakpoint(id)` removes it.
- `evaluate(frame, source)` compiles the source as direct eval code chained onto the frame's context, with the frame's
  register locals in front of it, and runs it with the frame's `this`. Assignments to variables that live in contexts
  stay; assignments to register locals only change the copy the expression sees. Pausing doesn't nest: breakpoints in
  code that `evaluate` runs don't stop.
- Uninitialized `let` / `const` bindings (in their temporal dead zone) read as `undefined`.
- `pause()` stops at the next statement that runs (`Reason::Pause`), as DevTools' pause button does. An executor that
  lives in an event loop is reached from another thread with an `Interrupter` ([Framework](framework.md)):
  `interrupter.interrupt(|executor, _| executor.pause())` stops a busy loop at its next check, and `set_breakpoint`,
  `clear_breakpoint` and `pause_on_exceptions` sent the same way take effect whether the loop is idle or running.

## How it works, and what it costs

- **Breakpoints swap a handler.** Every decoded instruction carries its handler ([Executor](executor.md)); a breakpoint
  replaces the handler of the statement's first instruction with `brk`, which reports to the debugger and then runs the
  instruction's own handler. Clearing the breakpoint puts the own handler back. The handler is a `Cell<Handler>`, read
  with one load as before, so code without breakpoints runs exactly as it did; nothing is checked per instruction.
- **Debug information is compiled only for a debugger.** An executor with a debugger compiles with `DebugInfo::Record`:
  the compiler keeps the statement positions (the places a breakpoint or a step can stop) and which register holds which
  variable over which bytecode range, in `BytecodeArray::debug`. Without a debugger it compiles with `DebugInfo::Omit`
  and the table is `None`, one word per function; the expression positions that stack traces read are the same either
  way, and the code cache keeps the two apart. On a 9 MB script (TypeScript's `typescript.js`, 21,693 functions)
  recording takes about 1% more compile time and about 4 MB: 98,597 statement positions, 49,174 locals with 0.45 MB of
  names, one table per function that has any.
- **Stepping arms only what it can reach.** `StepOver` and `StepOut` arm the statements of the functions on the stack;
  `StepInto` and `pause()` also arm the first statement of every function (functions loaded meanwhile included), so
  the next function entered stops there. Everything armed is put back at the next pause. `brk` stops when the frame
  depth says so: any depth for into, the same or shallower for over, shallower for out.
- **Exceptions** are reported once, where the executor first unwinds them: rethrowing (`ReThrow`, or an exception
  leaving a nested run on its way out through a built-in) marks the pending exception, so it is not reported again.
  Whether it will be caught follows the handlers' predictions, as V8 does: a `catch` counts; a `finally` or an
  iterator's close rethrows, so the search goes on outward; in an async function the exception rejects its promise,
  which counts as caught when the promise already has a handler (an `await` or a `then`) and otherwise goes on to the
  caller.
- **No JIT while debugging.** `with_debugger` drops the [tier](jit.md), so every function is interpreted and every
  breakpoint is reachable. Without a debugger nothing changes: the tier and the hot paths are untouched.

## Not yet

- Breakpoints by URL pattern, conditional breakpoints and logpoints (an embedder can evaluate a condition in `paused`
  and return `Continue`).
- Setting variables (`setVariableValue`), restarting a frame, and the frames of async functions that have suspended.
- Stepping into a generator or an async function as it resumes, and stepping out of the last frame into the next task.
- Pausing while the JIT is on: a debugger is attached when the executor is built.
