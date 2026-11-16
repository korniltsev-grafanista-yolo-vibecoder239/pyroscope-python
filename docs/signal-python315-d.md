# CPython 3.15 Signal: evaluation-fed state mirrors

This alternative starts at `6efc4719952f2153de38d2fd817a0fac25a05a93`.
CPython 3.15 moved its bound GIL state from pthread TSD to compiler TLS.
Calling its getter in an arbitrary native thread's signal handler can enter a
TLS allocator/loader. A successful memory read must still be a direct load;
neither fault-handler ownership checks nor signal-mask checks belong in the
unwinder.

The implementation obtains the thread-state pointer as an argument to a PEP 523
evaluation callback during normal, GIL-held execution. Configuration registers
its caller immediately. The callback registers other threads, and then invokes
CPython's default evaluator. Each thread has an SDK-owned pthread slot pointing
to a small native mirror. The signal handler pins that mirror, reads its state
pointer, walks the existing frame chain using kindasafe, and releases the pin.
It allocates nothing, never waits, and makes no syscalls for state lookup or
successful memory reads. `pthread_getspecific` is the same platform primitive
used by the older Python lookup, with native bindings warmed before sampling.

## Lifetime, including retained dictionaries

A thread-dictionary capsule's destructor alone cannot invalidate a binding:
extensions may retain a strong reference to `PyThreadState_GetDict()`'s result
after the state is deleted. Instead, a weak callback watches
`threading_local_sentinel`. CPython requires the thread state to own the only
strong reference to this sentinel, and clears it before the dictionary and
data stack. A temporary built-in `_thread._local` initializes the sentinel when
necessary; the SDK does not write CPython's private fields. See CPython's
[localdummy ownership explanation](https://github.com/python/cpython/blob/v3.15.2/Modules/_threadmodule.c#L1360)
and [state teardown](https://github.com/python/cpython/blob/v3.15.2/Python/pystate.c#L1770).

The dictionary retains a `(capsule, weakref)` tuple, but retaining that tuple
cannot keep the sentinel alive. Its callback clears the mirror's atomic state
pointer before deletion. Sequentially consistent reader increment/load and
invalidation store/load ensure that either the reader sees null or teardown
waits for the reader. Only normal-thread teardown waits. The capsule and pthread
slot separately own the mirror; native thread exit releases the slot, and a
recycled pthread starts with an empty slot even if CPython retains an abandoned
state. Finalization callbacks cannot register a new mirror for a finalizing state.

After shutdown-before-fork, the child discards reader/owner counts belonging to
vanished native threads instead of waiting for them. Process identity is read
only during normal registration/teardown, never in the signal walk. The parent
retains its own counters. Inheriting an actively running profiler across fork
remains unsupported.

The retained-dictionary regression reproduced the original lifetime error as an
unexpected empty record with weight 13. With sentinel invalidation it holds 64
complete dictionaries past state deletion, repeatedly reuses allocation
addresses, and requires every subsequent native signal to yield exactly
`[dropped: no_thread_state]` at the specified weight, with no stack record.
Releasing the retained dictionaries later is also checked. Under ASAN, address
quarantine can prevent address reuse; the same test still verifies deletion and
invalidation. A separate regression requires actual pthread identity reuse
while the old bound state remains linked.

## Difference from the first wave

| Reference | Mechanism examined | Material difference here |
| --- | --- | --- |
| [A: 59e9175](https://github.com/korniltsev-grafanista-yolo-vibecoder239/pyroscope-python/commit/59e917545e2ed40cc32204d760ef4ec5b202f167) | Bounded interpreter thread-list walk, bound flags and native/pthread identities | Explicit registration and lifetime invalidation; no list walk or identity-based selection in a sample |
| [B: 537b2ea](https://github.com/korniltsev-grafanista-yolo-vibecoder239/pyroscope-python/commit/537b2eac3025f17339544a353607afef4c56f8fe) | Discover CPython's compiler-TLS slot and read ELF DTV/Mach-O storage | Own the storage and populate it from evaluator arguments; no discovery or reads of CPython's TLS |
| [C: f77a0e1](https://github.com/korniltsev-grafanista-yolo-vibecoder239/pyroscope-python/commit/f77a0e1df217fa9bf4838039bc2f7a4b4281a493) | Locate bound TLS using fixed Linux thread-pointer offsets or Mach-O descriptors | No architectural thread-pointer arithmetic, startup-image restriction, or descriptor parsing; normal callbacks maintain a separate binding |

The cost is different too: the callback inhibits call specialization/JIT, has
per-frame overhead, and registers other preexisting threads only at their next
Python evaluation. These are explicit limits, not transparent execution
preservation. A local nine-trial median of 500,000 trivial calls measured
13.17 ms before profiling, 20.81 ms during it (1.58x), and 13.60 ms after shutdown.
With `PYTHON_JIT=1`, the corresponding times were 12.11, 24.22 and 11.55 ms
(2.00x). These are illustrative host measurements, not an overhead bound.
The [README](../README.md#signal-cpu-profiler) documents the supported contract.

## Validation

Native tests use actual `SIGPROF` delivery with explicit period counts. They
compare the complete `foreign_root;foreign_leaf` stack, filename and line mode,
and its 11-period pprof value. Native-only and deleted-state probes must have
empty stack queues and exact dropped-profile weights (including 13 and 17
periods in the retained-dictionary test). There is no statistical tolerance in
these tests. They also verify the preexisting-native-call registration gap,
competing evaluator rejection, shutdown/restart restoration, and a fork child
clearing a state with an inherited in-flight reader.

The existing resolver tests now run on 3.15, including direct, nested, recursive,
Unicode, comprehension, generator, coroutine, exception and C-reentry stacks;
additional detached and generator-throw cases check GIL release and exception
preservation. All line-number modes and exact weighted pprof aggregation remain
checked. The benchmark preflight compares all 82 frames against CPython itself.

The packaged workload uses preexisting/new workers, each with equal Python and
GIL-releasing PBKDF2 CPU budgets, then repeats after shutdown. Each of four full
paths must contribute 15–35% (expected 25%), together at least 90%, with at least
600 periods of total weight. Unexpected stacks and drops remain in the
denominator, and diagnostics retain the full profile. Python 3.10's first-session
preexisting CTimer worker has one bootstrap frame; subsequent/new workers also
include the installed bootstrap wrapper. Python 3.11+ uses qualified names.

ITimer has tick-accounting/coalescing losses without an overrun count. A separate
native C probe, without the profiler, delivered only 150 ticks for two thread
CPU seconds at 100 Hz on this host. Its fixture therefore observes longer CPU
budgets to meet the same 600-period floor; no shape threshold is relaxed.
It pins its workers to one allowed CPU to control process-wide delivery bias;
CTimer leaves them free to run native code concurrently on multiple CPUs.
CTimer retains the shorter eight-CPU-second workload and six-second minimum,
and the existing two-second overrun test remains unchanged.

One observed CPython 3.15 CTimer session, in nanoseconds, was:

```text
[dropped: contentions] 20000000
Thread._bootstrap;Thread._bootstrap_inner;Thread.run;new_worker;native_leaf 2010000000
Thread._bootstrap;Thread._bootstrap_inner;Thread.run;new_worker;python_leaf 1990000000
Thread._bootstrap;Thread._bootstrap_inner;Thread.run;preexisting_worker;native_leaf 2000000000
Thread._bootstrap;Thread._bootstrap_inner;Thread.run;preexisting_worker;python_leaf 1990000000
```

The expected paths account for 99.75% of its 8.01 seconds. One ITimer session
contained these complete paths and no other weight:

```text
Thread._bootstrap;Thread._bootstrap_inner;Thread.run;new_worker;native_leaf 3880000000
Thread._bootstrap;Thread._bootstrap_inner;Thread.run;new_worker;python_leaf 3310000000
Thread._bootstrap;Thread._bootstrap_inner;Thread.run;preexisting_worker;native_leaf 3300000000
Thread._bootstrap;Thread._bootstrap_inner;Thread.run;preexisting_worker;python_leaf 3480000000
```

Local validation uses Linux x86-64. macOS, ARM64 and musl were not run locally;
the CI matrix includes Linux x64/ARM64 Python 3.10, 3.14, 3.15 and macOS 3.15.
ASAN validation instruments the SDK's Rust and native C/C++ code, using an
uninstrumented CPython 3.15.2 and `detect_leaks=0` (the abandoned-state fixture
intentionally retains one Python state until process exit).

Commands run (with `PYO3_PYTHON`, `Python3_EXECUTABLE`, `Python3_ROOT_DIR`,
`PYTHONHOME`, and `LD_LIBRARY_PATH` selecting the indicated local interpreter):

- `cargo test --locked --workspace`: CPython 3.10.22, 3.11.18, 3.12.16,
  3.13.18, 3.14.10 and 3.15.2.
- `PYTHON_JIT=1 cargo test --locked -p pyroscope_python_extension signal_sampler -- --nocapture`.
- `cargo clippy --locked --workspace --all-targets -- --deny warnings` on 3.15.
- `cargo bench --locked -p pyroscope-python-benchmarks --bench signal_stack -- --test`
  on 3.15 (82-frame preflight and all three benchmark paths).
- `cargo +nightly test --locked --target x86_64-unknown-linux-gnu -p
  pyroscope_python_extension signal_sampler -- --nocapture`, with
  `RUSTFLAGS=-Zsanitizer=address`, C/C++ `-fsanitize=address`, and
  `ASAN_OPTIONS=detect_leaks=0`: all six signal/lookup tests passed.
- Packaged `go test -v -count=1 -timeout 20m . -profiler=signal` on 3.15,
  including callbacks, both timers, overruns, raw forks, lifecycle/fault handlers,
  and native/Python worker profiles across restarts. The same run on 3.10 plus
  targeted reruns covered all Signal cases. One 3.10 burst-workload run timed out;
  its unchanged test passed on rerun. The new 3.10 fixture was corrected to assert
  the observed preexisting-worker bootstrap and to control ITimer delivery bias.
- Packaged `go test -v -count=1 -timeout 10m -run
  'TestPythonSignalNativeAndPythonThreadLifetimes|TestPythonProfilerCallbacks'
  . -profiler=signal` with a CPython 3.15.2 JIT-enabled runtime (`PYTHON_JIT=1`):
  both timers, callback profiles and both worker-lifecycle sessions passed.
- `python3.15 -m unittest scripts.tests.test_thread_hooks`, `cargo fmt --all --check`,
  Ruff format/import checks for the changed Python files, and `git diff --check`.
