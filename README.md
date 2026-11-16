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
    server_address="http://localhost:4040",
    cpu_implementation=pyroscope.ProfilerImplementation.Signal,
)
```

Signal supports **64-bit CPython 3.10–3.15 with the GIL**, on Linux and macOS
(x86_64 and arm64). Build the extension for the interpreter's exact minor
version. Free-threaded builds, PyPy, GraalPy, and the limited C API are unsupported.
Signal samples CPU work, including native work that releases the GIL; it requires
`oncpu=True` and does not implement GIL-only filtering or native stack frames.

Linux defaults to `SignalTimer.CTimer`, which uses one CPU timer per registered
thread. It covers the configure caller and existing/new `threading.Thread`
threads; raw `_thread` or foreign native threads are not automatically registered.
`SignalTimer.ITimer` is available on Linux and is the macOS default. It uses a
process CPU timer whose signal may land on any thread; thread attribution can
therefore be less representative than with CTimer. Signal owns `SIGPROF` and
`ITIMER_PROF` when using ITimer.

On CPython 3.15, Signal discovers the bound thread-state TLS slot during normal
initialization, then reads the current thread's slot directly. The signal handler
never invokes the allocating compiler TLS resolver or searches by recyclable
thread IDs. Fresh native threads cannot inherit a departed thread's state, even
if an old bound state remains linked in CPython. Detached states remain available
for sampling native CPU work. Both JIT and non-JIT builds are supported. Linux
uses the x86_64/arm64 glibc or musl TLS ABI; macOS uses Mach-O TLS descriptors and
the module's existing pthread key. An unrecognized or ambiguous TLS layout makes
configuration fail before the timer is armed.
Frame walks are limited to 256 entries, including interpreter sentinels; failed
walks discard the whole stack and report lost sampling periods in the profile.

Signal uses kindasafe to read memory directly, without syscalls on a successful
memory load. **Initialize other `SIGSEGV`/`SIGBUS` handlers before configuring the
profiler.** Application code and dependencies must not change either recovery
handler during initialization or afterward, including across profiler stops and
restarts. Previously installed callbacks must support direct chaining and remain
loaded. Recovery initialization is cached once; Signal neither checks ownership nor
reclaims/reinstalls replaced handlers. Replacing them can crash the process.

Recovery initialization must succeed before unwinding, and `SIGSEGV` and `SIGBUS`
must remain unblocked throughout every walk, including when `SIGPROF` interrupts
another signal handler. These are caller preconditions, with no runtime
initialization or signal-mask checks: blocked recovery signals do not skip a
sample, and a failed read can crash the process. Recovery signals must also stay
unblocked on the deferred name/line resolver thread. Source memory ranges for
TLS reads and deferred resolution must not overlap unrelated live Rust
exclusive borrows, including after allocation reuse. Copied bytes remain
untrusted: fault recovery guarantees neither object identity nor a coherent
snapshot; changing or reused objects can cause dropped or misattributed samples.

## License

Apache-2.0
