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

The experimental Signal profiler targets CPython 3.10–3.15 with the GIL, on
64-bit x86 and ARM Linux/macOS. This 3.15 alternative was runtime-tested on Linux
x86-64; ARM64 and macOS validation is left to the added CI jobs.
It records Python line numbers
and CPU spent in native calls, including calls that release the GIL. Native
frames, thread names, thread IDs and per-thread tags are not collected.

```python
import pyroscope

pyroscope.configure(
    application_name="my-app",
    cpu_implementation=pyroscope.ProfilerImplementation.Signal,
    oncpu=True,
)
```

Linux defaults to `SignalTimer.CTimer`, which arms a CPU timer for the configure
caller and registered `threading.Thread` instances. `SignalTimer.ITimer` samples
threads selected by a process CPU timer, including raw `_thread` threads and
native threads calling Python. ITimer is the only mode on macOS. CTimer does not
automatically register raw/native threads. Configure after forking, or shut down
before forking and configure again in the child. Inheriting a running profiler
is unsupported.

### CPython 3.15 publication mechanism and limitations

3.15's bound-state getter uses compiler TLS, which may enter the dynamic loader
when called from SIGPROF. Instead, this implementation installs a
[PEP 523 frame](https://docs.python.org/3.15/c-api/subinterpreters.html#c._PyInterpreterState_SetEvalFrameFunc)
evaluator wrapper on a normal Python thread. The wrapper publishes a state lease
in an SDK-owned pthread key before evaluating Python. The configure caller is
published immediately, so it can be sampled even when its next action is a
GIL-releasing native call. Leases persist across evaluator returns and detachments.
The handler reads the SDK key and takes a nonblocking reader count, then walks
the actual frame chain with fault-recoverable direct loads.

A private `_thread._local` holds a weakref callback to the thread-local sentinel.
CPython clears the sentinel before freeing the state; the callback invalidates
the lease and waits for any admitted walks to finish. Only ordinary teardown
waits. Retaining the public `PyThreadState_GetDict()` dictionary, or even the
SDK's dictionary through heap introspection, does not prolong the lease.
Native thread exit clears the pthread key, including when an embedder
abandons a bound state and the OS recycles that thread's identity. There is no
thread-list search, thread-ID matching, CPython TLS discovery or per-read syscall.
3.10–3.14 retain their existing CPython pthread-key lookup.

This alternative deliberately trades execution overhead for independence from
CPython's compiler TLS ABI:

- Other preexisting threads become visible on their next frame-evaluator entry.
  CPU spent in an already-running native call or a Python frame that makes no
  further Python calls is reported as `[dropped: no_thread_state]` until then.
  Configure before starting work to avoid this initial gap.
- The evaluator wrapper prevents CPython's evaluator-dependent call optimization
  and tier-2/JIT execution. A local CPython 3.15.2 Linux x86-64 call-heavy loop
  measured about 1.5× runtime with JIT off and 1.9× with JIT requested; these are
  workload-specific observations, not general overhead bounds.
  It remains installed after shutdown, so its overhead
  also persists. Enabling `PYTHON_JIT` does not preserve JIT execution under this
  profiler. This is unsuitable when preserving JIT performance is required.
- A different PEP 523 evaluator at configuration time is rejected. Applications
  and dependencies must not replace the installed evaluator or mutate the SDK's
  private thread-local storage. Replacing `_thread._local` before initial setup
  is also rejected: its original lifetime semantics are required. The native
  extension must remain loaded.
- Subinterpreters, embedding cycles that finalize and reinitialize CPython, free-threaded
  builds, indexed `Py_STACKREF_DEBUG` references and `oncpu=False` are unsupported.
  Frame offsets are checked against the build interpreter's headers.

### Fault recovery contract

Applications and dependencies must preserve kindasafe's SIGSEGV/SIGBUS handlers
during initialization and afterward, including across profiler stops and
restarts. Install other handlers (including `faulthandler`) before initialization.
Previously installed callbacks must support direct chaining and remain loaded.
Recovery initialization is cached once. The SDK neither checks handler ownership
nor reclaims replaced handlers; replacing them can cause a process crash.

Recovery must initialize successfully before unwinding. SIGSEGV and SIGBUS must
remain unblocked throughout each walk on the sampled thread, including when
SIGPROF interrupts another signal handler. The unwinder does not check
initialization or signal masks. Blocked recovery signals do not skip a sample:
an unreadable memory access can instead crash the process. Successful reads use
direct loads without syscalls.

Deferred name and line resolution also uses kindasafe. Both recovery signals
must remain unblocked on the resolver thread. Source ranges must not overlap
unrelated live Rust exclusive borrows, including after allocation reuse. Treat
copied bytes as untrusted: fault recovery establishes neither object identity nor
a coherent snapshot. Active frame chains must remain owned by the interrupted
thread; foreign mutation is unsupported. Deferred object reuse can still cause
samples to be dropped or misattributed.

## License

Apache-2.0
