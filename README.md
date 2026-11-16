# pyroscope-python

Pyroscope continuous profiling agent for Python applications.

Uses [py-spy](https://github.com/benfred/py-spy) for stack sampling and the [pyroscope](https://crates.io/crates/pyroscope) Rust crate to send profiles to a Pyroscope server.

## Installation

```bash
pip install pyroscope-io
```

For source builds, initialize the native dependency submodules with
`git submodule update --init --recursive`. Cargo uses the `py-spy` submodule
as a local path dependency; its version is pinned by the parent repository.

## Signal CPU profiler

```python
import pyroscope

pyroscope.configure(
    application_name="my-app",
    cpu_implementation=pyroscope.ProfilerImplementation.Signal,
    oncpu=True,
    signal_timer=pyroscope.SignalTimer.CTimer,  # Linux; use ITimer on macOS
)
```

Signal supports CPython 3.10–3.15 GIL builds on 64-bit x86-64/ARM64 Linux and
macOS, in the main interpreter. Free-threaded builds, indexed
`Py_STACKREF_DEBUG` references, subinterpreters, and applications that manually
swap or migrate live thread states are unsupported. Frame layouts are checked
against the build interpreter's headers. Profiles include Python frames and
line numbers (including Python callers of native work), but no native frames,
thread IDs, names, or thread tags. JIT execution is subject to CPython's
instrumentation: enabling monitoring may deoptimize or inhibit optimized code.

On **3.15**, Python execution events (`PY_START`, `PY_RESUME`, `PY_THROW`, and `LINE`)
populate a **profiler-owned pthread slot** on each executing thread. SIGPROF
reads that slot and walks the interrupted thread's frame chain. It does not
call CPython's compiler-TLS getters, inspect TLS layouts, or search thread lists.
`PY_RETURN`, `PY_YIELD`, and `PY_UNWIND` callbacks clear the binding before the
last Python frame leaves, while nested exits preserve the outer Python caller.
No thread-state address survives outside that execution scope, even when native
code retains the dictionary returned by `PyThreadState_GetDict()` after deleting
the state. Native thread exit clears its slot even if an embedder retains the
state; a recycled pthread cannot inherit the old binding. A new profiling session
requires a new event on each thread.
CPython 3.10–3.14 retain their existing pthread-backed GIL-state lookup.

This approach adds an execution callback on each monitored line/entry, with
application-dependent overhead beyond sampling costs. It requires one free
`sys.monitoring` tool ID; startup fails if all six are occupied. Other monitoring
tools are preserved, and shutdown releases Pyroscope's tool and callbacks.
Applications must not modify that tool or suppress its entry/exit events (for
example, native code must not keep `PyThreadState_EnterTracing()` active across
the last Python frame's exit). Other instrumentation must allow these callbacks
to run, including during exceptions; they are the binding lifetime protocol.
An already-running Python loop can register on its next line event, but
**native work already in progress at startup or
restart is unprofiled until the next enabled Python execution event**. Such
deliveries contribute `[dropped: no_thread_state]`. Once registered, a thread
remains sampleable while native code releases the GIL. Stop profiling before
interpreter finalization; loading/unloading/reinitializing Python during a
session is unsupported.

`CTimer` uses a Linux CPU timer per registered `threading.Thread`, including
preexisting threads and the configure caller. Raw `_thread` workers and foreign
native threads do not get CTimer timers automatically. `ITimer` is process-wide
and can sample those threads when Python events have established their binding;
native-only threads contribute dropped samples. ITimer can undercount CPU time
through tick granularity and signal coalescing. CTimer carries timer overruns
into sample weights. Shutdown before forking, then configure in the child.

The unwinder and deferred resolver use kindasafe direct memory loads, without
syscalls on successful memory-read paths. Initialize recovery successfully
before unwinding. The application and its dependencies must not change
`SIGSEGV`/`SIGBUS` handlers during initialization or afterward, including across
profiler stops/restarts. Install other handlers first; previous callbacks must
support direct chaining and remain loaded. There are no runtime ownership
checks or automatic handler reinstallation.

Keep both recovery signals unblocked throughout each walk, including when
SIGPROF interrupts another signal handler, and on the resolver thread. The
unwinder does not check initialization or signal masks: violating these
preconditions can crash the process rather than skip samples. Native reads
assume the interrupted thread owns its active frame chain and no other thread
mutates or destroys it. Deferred source ranges must not overlap unrelated live
Rust exclusive borrows, including after allocation reuse. Copied bytes remain
untrusted: fault recovery establishes neither object identity nor a coherent
snapshot. These contracts also apply to the 3.15 binding approach.

See [the design and validation notes](docs/signal-python315-design.md) for the
comparison with the first-wave approaches, lifecycle regressions, and measured
instrumentation overhead.

## License

Apache-2.0
