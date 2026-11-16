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

Select `cpu_implementation=pyroscope.ProfilerImplementation.Signal` with
`oncpu=True`. Signal supports CPython 3.10–3.15 default GIL builds on x86-64 and
ARM64 Linux/macOS, with matching build/runtime Python minor versions. Free-threaded
builds, subinterpreters and indexed `Py_STACKREF_DEBUG` builds are unsupported. Samples
contain complete Python stacks and the selected line-number mode; native frames
and thread names/IDs/tags are not included. Failed walks appear as weighted
`[dropped: reason]` samples instead of partial stacks.

`SignalTimer.CTimer` (Linux default) uses per-thread CPU clocks, including timer
overruns. It samples the configure caller and registered `threading.Thread`
workers, including preexisting workers. Raw `_thread` and foreign pthreads do
not automatically acquire CTimers. `SignalTimer.ITimer` (macOS default) uses a
process CPU timer: delivery can favor particular runnable threads. Native CPU
work which releases the GIL is attributed to its Python caller once that thread
is registered with the 3.15 lookup described below. Native-only threads have no
Python stack. Configure after forking, or shut down before forking and configure
again in the child; inheriting a running profiler across fork is unsupported.

On **CPython 3.15**, Signal installs a PEP 523 frame-evaluation callback. Normal
Python execution publishes the current thread state to an SDK-owned pthread
slot. The signal handler reads that slot and pins its lifetime with lock-free
atomics; it does not call CPython's compiler-TLS getter, inspect TLS layouts,
search interpreter lists or compare recyclable thread IDs. The frame walk still
uses kindasafe direct loads. A weak callback on CPython's thread-lifetime
sentinel invalidates the binding before state deletion, even when an extension
retains `PyThreadState_GetDict()`'s dictionary. Teardown waits for any current
reader; signal handlers never wait. Native thread exit releases its slot, so a
recycled pthread cannot inherit an abandoned state's binding.

The 3.15 mechanism has these additional limits:

- Configure registers its caller immediately. Other already-running threads
  register at their next Python frame evaluation. An existing native call or
  Python loop can remain uncovered until then, producing `no_thread_state`
  drops. Configure before starting CPU work on those threads.
- CPython disables call specialization and invalidates JIT executors when the
  hook is installed. The hook also runs on Python frame entries. This can cause
  substantial overhead for call-heavy applications, even at low sample rates.
  JIT-capable builds are accepted, but profiling changes their execution mode.
- Other PEP 523 evaluators are incompatible: configuration fails if one is
  installed. Do not replace Signal's evaluator while it runs. Shutdown restores
  the default evaluator, and restarts are supported.
- Embedders must keep the SDK loaded, preserve its private thread-dictionary
  entry, and obey CPython's sole-ownership rule for `threading_local_sentinel`.
  Retaining the public thread dictionary is supported. Moving/swapping thread
  states outside frame evaluation and interpreter reinitialization are not.

CPython 3.10–3.14 retain their original pthread-TSD lookup and have no evaluation
hook overhead. [Design and validation notes](docs/signal-python315-d.md) describe
the alternative and its lifecycle regressions.

Signal's unwinder and deferred name/line resolver use kindasafe. SDK users must
ensure their application and dependencies do not change `SIGSEGV`/`SIGBUS`
handlers during recovery initialization or afterward, including across profiler
stops/restarts. Install other handlers before profiler initialization; previous
callbacks must support direct chaining and remain loaded. Initialization is
cached once. The SDK does not check handler ownership or reclaim/reinstall
handlers; replacing them can turn a failed read into a process crash.

Successful recovery initialization before unwinding, and unblocked `SIGSEGV`
and `SIGBUS` throughout every walk, are caller preconditions, including when
sampling interrupts another signal handler. The unwinder does not check
initialization or signal masks. Blocked recovery signals do not skip samples:
a failed read can crash the process. The resolver thread must also keep recovery
signals unblocked. Read source ranges must not overlap unrelated live Rust
exclusive borrows, including after allocation reuse. Copied bytes remain
untrusted: fault recovery establishes neither object identity nor a coherent
snapshot. Successful memory loads use no syscalls; name resolution remains
deferred and can fail if code objects have already been freed/reused.

## License

Apache-2.0
