# pyroscope-python

Pyroscope continuous profiling agent for Python applications.

Uses [py-spy](https://github.com/benfred/py-spy) for stack sampling by default. An experimental Signal CPU profiler is also available.

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
    application_name="my-service",
    cpu_implementation=pyroscope.ProfilerImplementation.Signal,
    oncpu=True,
)
```

Signal supports CPython 3.10–3.15 with the GIL, in the main interpreter, on
64-bit Linux and macOS (x86-64 and ARM64). Free-threaded builds, subinterpreters,
and `Py_STACKREF_DEBUG` stack-reference representations are unsupported.
Build the extension against the target interpreter's full C API; the build
checks its internal layouts against that interpreter's headers. Signal supports
all `LineNo` modes, but does not attach thread names, thread IDs, or thread tags.

Linux defaults to `signal_timer=pyroscope.SignalTimer.CTimer`, which samples
the configure caller and Python threads registered by the threading hooks,
including threads already running when configured. Native threads that bypass
those hooks have no CTimer. `SignalTimer.ITimer` uses a process CPU timer and can
also sample Python callbacks on native threads; macOS supports only ITimer.
CPU work in C extensions that release the GIL is attributed to its Python caller.
Signal requires `oncpu=True`; `gil_only` does not filter its samples.

On CPython 3.15, Signal avoids Python's native TLS getters inside the signal
handler: those getters can allocate or lock. Startup identifies the bound-state
TLS slot, and sampling reads that thread's actual slot, including while the GIL
is released. Thread identities and thread-state addresses are never cached or
matched against the interpreter's thread list: a fresh non-Python thread has no
Python state even if its pthread identity matches an abandoned bound state.
On Linux (glibc or musl), 3.15 requires Python in the executable or a direct
startup shared-library dependency; embedding with `dlopen`-loaded libpython is
unsupported. On macOS, the original and v2 Darwin TLV formats are supported.
Unsupported or ambiguous TLS layouts fail startup. libpython and the main
interpreter must stay alive while profiling. Frame walks are limited to 256
frames. Lost sampling periods appear as `[dropped: ...]` stacks in the profile.

Signal reads memory directly using kindasafe. Applications and dependencies
must leave kindasafe's `SIGSEGV` and `SIGBUS` handlers installed during
initialization and afterward, including across profiler stops and restarts.
Install other handlers (including `faulthandler`) before profiler initialization;
previous callbacks must support direct chaining and remain loaded. Signal does
not check handler ownership or reclaim/reinstall replaced handlers. Recovery is
initialized once; replacing its handlers can crash the process on a failed read.

`SIGSEGV` and `SIGBUS` must remain unblocked on sampled threads throughout every
walk, including when `SIGPROF` interrupts another signal handler, and on the
resolver thread during deferred name and line resolution. Recovery initialization
and signal masks are caller preconditions, with no checks in the unwinder.
Blocked recovery signals do not skip a sample: a failed read can crash the
process. Successful kindasafe loads use direct memory reads without syscalls.

Recorded addresses and copied bytes are untrusted: recovery does not establish
object identity or a coherent snapshot. Native reads assume current-thread
ownership of its frame chain; deferred name/line reads can observe changing or
reused storage. Source ranges must not overlap unrelated live Rust exclusive
borrows, including after allocation reuse.

## License

Apache-2.0
