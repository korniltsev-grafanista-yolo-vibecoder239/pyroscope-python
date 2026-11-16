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

The experimental Signal implementation supports CPython 3.10–3.15 on x86-64
and ARM64 Linux and macOS, with the GIL and the full C API. It samples Python
stacks while Python or native extension code uses the CPU, including C calls
which release the GIL. CPython 3.15 is tested with its optional JIT enabled and
disabled. Select it with:

```python
import pyroscope

pyroscope.configure(
    application_name="my-app",
    cpu_implementation=pyroscope.ProfilerImplementation.Signal,
    oncpu=True,
)
```

Linux defaults to `SignalTimer.CTimer`, with a CPU timer for the configure
caller and each registered `threading.Thread`. `SignalTimer.ITimer` uses a
process CPU timer and can also sample Python threads created through `_thread`
or native extensions; it is the only Signal timer available on macOS.
`oncpu=False`, free-threaded builds, subinterpreters, and indexed
`Py_STACKREF_DEBUG` builds are unsupported. Python line numbers are supported;
thread IDs, thread names, and thread tags are not yet attached to Signal samples.
Native frames are not collected.
Configure after forking; inheriting a running agent remains unsupported.

On CPython 3.15 the profiler finds the current thread's bound state in the main
interpreter's thread list using fault-recoverable direct reads. This avoids
compiler TLS access in a signal handler and preserves stacks during native
work without the GIL. The search visits at most 4096 thread states per sample;
unreadable or concurrently changing lists can lose a sample, and exceeding the
limit counts as truncation. It does not cache thread-state addresses across
samples. CPython 3.10–3.14 retain their pthread-key lookup. Frame layouts are
checked against the target interpreter's headers when building.

The 3.15 lookup checks both Python and native OS thread IDs to handle recycled
pthread IDs. On Linux this makes one `gettid` syscall per sample, followed by
direct memory reads; it does not introduce per-read syscalls. A Linux raw-fork
child inherits its caller's stale native ID; the process leader can use that
binding after an async-signal-safe `getpid` check.

### Memory recovery contract

The application and its dependencies must preserve kindasafe's `SIGSEGV` and
`SIGBUS` handlers during initialization and afterward, including across
`shutdown()` and profiler restarts. Install other handlers, such as
`faulthandler`, **before** profiler initialization. Previously installed
callbacks must support direct chaining and remain loaded. Initialization is
cached once; the profiler does not check handler ownership or reclaim replaced
handlers. Replacing them can turn an unreadable memory access into a process
crash.

Recovery initialization must succeed before unwinding, and both recovery
signals must remain unblocked on sampled threads throughout each walk,
including when SIGPROF interrupts another signal handler. These are caller
preconditions: the unwinder does not check initialization or signal masks.
Blocking recovery signals does not skip samples; a failed read can crash.
Successful memory loads use direct reads without per-read syscalls.

Deferred name and line resolution also uses kindasafe. Both recovery signals
must remain unblocked on the resolver thread. Source ranges, including the
3.15 thread-list search, must not overlap unrelated live Rust exclusive
borrows, even after allocation reuse. Copied bytes are untrusted: fault
recovery establishes neither object identity nor a coherent snapshot. Type
and bounds checks reject many stale reads; memory reuse or concurrent changes
can still drop or misattribute samples.

## License

Apache-2.0
