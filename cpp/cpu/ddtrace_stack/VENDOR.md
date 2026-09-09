# Vendored: dd-trace-py `stack` (echion sampler)

| | |
|---|---|
| Upstream | https://github.com/DataDog/dd-trace-py |
| Release | `v4.14.0` (commit `ef8651166784671cc6d5f82803c8ea38a0239557`) |
| Path | `ddtrace/internal/datadog/profiling/stack/` |
| License | Apache-2.0 OR BSD-3-Clause; `echion/` and `src/echion/` are MIT (see `LICENSE`) |

Upstream called this `stack_v2` before 4.14.0. Driven from Rust over the C ABI
in `src/pyroscope_entry.cpp`; see `rust/src/cpu/native.rs`.

## Two sampling modes

The tree provides two ways of measuring CPU, selected per session by
`CpuProfiler` in `rust/src/cpu/mod.rs`. They share all of echion -- the frame
cache, the string table, the fault-tolerant memory copy -- and differ only in
what decides when to take a sample.

**`CpuProfiler::Ddtrace`, the wall-clock walk.** A dedicated sampler thread
wakes on an interval and walks every Python thread, reading frames out of
process memory. Each stack is weighted by that thread's CPU delta, so threads
that did not run contribute nothing. Cost grows with the thread count whether or
not those threads are busy.

**`CpuProfiler::DdtraceCpuTimer`, the signal-driven timers.** Every Python
thread gets its own `timer_create` timer on its per-thread CPU clock, delivering
`SIGPROF` to that thread via `SIGEV_THREAD_ID`. The handler captures its own raw
frames into a preallocated per-thread ring and returns without allocating,
locking, or touching the GIL; the sampler thread drains the rings, validates the
copied CPython data and renders. A thread is only interrupted once it has
actually burned another interval of CPU, so no thread has to be walked to find
out whether it ran, and the samples land on the thread that was running rather
than being reconstructed from another thread's view of it.

`src/cpu_timer.cpp`, `include/cpu_timer.hpp`, `include/cpu_sample_ring.hpp` and
`include/cpu_timer_tid_table.hpp` are the CPU timer. They come from upstream
development after `v4.14.0` rather than from a tagged release, so they are newer
than the rest of this tree; the `danger.h`/`danger.cc` fault-recovery hook they
need came from the same place and applied unchanged, because those two files did
not move at all in between. Everything else in this tree is still `v4.14.0`.

**Linux only.** The wall walk needs CPython 3.11+ and the CPU timer 3.12+; both
limits are enforced by `CpuProfiler::check_supported` and explained below. The
tree compiles on macOS and on 3.10, it just cannot discover threads there, and
the CPU timer engine compiles to no-ops below 3.12.

## No libdatadog

Upstream renders samples through `StackRenderer` -> `ddup_*` ->
`libdd_wrapper.so` -> Rust **libdatadog**, which owns pprof encoding and
upload. **None of that chain is vendored.** Specifically dropped:

- the entire `dd_wrapper/` tree,
- `src/stack_renderer.cpp` and `include/stack_renderer.hpp`, replaced by
  `src/pyroscope_renderer.cpp` + our own `include/stack_renderer.hpp`, which
  render into `Pyroscope::CpuSample` (`../PyroscopeCpu.h`) and push over the C
  ABI in `rust/include/pyroscope_ffi.h`.

`libdd_wrapper.so` is a pre-built binary artifact and is not buildable from
source here; taking it would also mean two pprof encoders and two uploaders in
one process.

`shim/dd_wrapper/include/` holds small replacements for the few headers the
retained code still includes -- see the header comments in each. Only
`constants.hpp`, `defer.hpp` and `scope.hpp` are genuine upstream files (pure
utility code, no libdatadog dependency); `sample.hpp`, `profiler_stats.hpp`
and `profiler_state.hpp` are Pyroscope no-op shims. Keeping them as shims is
what lets `src/sampler.cpp` and `src/echion/stacks.cc` stay byte-identical to
upstream.

Check after building that no real dd_wrapper header leaked in:

```
nm --defined-only -g libpyroscope_cpu_ddtrace.a | grep -E 'ddog_|libdatadog'   # empty
nm -u _native.abi3.so | grep ddog_                                            # empty
```

## Other things dropped

- All `*.py` / `*.pyi`.
- `src/stack.cpp` -- the CPython extension-module glue (`PyMethodDef`
  `stack_start`/`stack_stop`/`register_thread`/`link_span`/...). Replaced by
  `src/pyroscope_entry.cpp`, a plain C ABI.
- Span linking, greenlet tracking, asyncio task attribution and
  `sys.monitoring` native-call tracking. All are Datadog product features
  driven from the Python layer, and none affects CPU sampling.
  `src/thread_span_links.cpp` and `src/origin_task_links.cpp` are still
  compiled because `src/sampler.cpp` references them; with nothing populating
  them they are inert.
- `ThreadInfo::sample_cpu_timer()` and the task-address visitor it is built on,
  which is the CPU timer's half of that same asyncio/greenlet attribution: at
  drain time it re-walks the thread's tasks, matches one against the identity the
  signal handler captured, and stitches the task's logical ancestry onto the
  physical frames. Roughly 500 lines in `src/echion/threads.cc` that have nothing
  to attribute to here. See patch 4 below for what replaces it.
- `fuzz/` and `test/`.
- `profiling_helpers/`, which `src/echion/frame.cc` includes, is **not**
  vendored again: `cpp/profiling_helpers/` (used by the memalloc profiler) is
  byte-identical to this release's copy, and `CMakeLists.txt` puts `cpp/` on
  the include path. One definition on disk means the two archives cannot drift
  into an ODR violation on those `namespace DataDog` inline functions.

## Local modifications

Marked in-file with `Pyroscope patch:` comments.

1. **`src/echion/threads.cc`, `for_each_thread()`** -- auto-register threads on
   discovery, and refresh entries whose thread has been replaced.

   Upstream depends on the Python `threading` patch calling
   `stack_thread_register()` for every thread. That call does two things:
   creates the `ThreadInfo`, and *overwrites* any existing entry under the same
   key. Both halves matter, and without the Python layer both have to happen
   here.

   - Without the insert, `thread_info_map` stays empty and the loop's
     `continue` silently skips **every** thread -- the profiler yields nothing.
   - Without the overwrite, recycled `pthread_t` values are mis-attributed. The
     map is keyed by `pthread_t`, which the C library reuses after a thread
     exits; a new thread inheriting a dead one's key would keep the dead
     thread's `cpu_clock_id` (derived from the old kernel TID), so
     `update_cpu_time()` reads a clock that no longer exists and the CPU delta
     stays zero for that thread's whole life. A thread-churning workload
     reported **5% of the CPU it actually used** before this was handled. The
     kernel TID (`PyThreadState::native_thread_id`) is the discriminator.

   Cost: no Python-level thread name (not reachable off-GIL from here), so this
   profiler reports no `thread_name` tag. Entries for dead threads are never
   evicted, which is fine because `pthread_t` reuse bounds the map at roughly
   the peak concurrent thread count rather than the total ever created.

   The same block also arms each discovered thread's CPU timer. Upstream
   reconciles timers here too, but only as a safety net behind the `threading`
   patch, which arms a timer from inside each new thread the moment it starts.
   With no Python layer this walk is the only discovery mechanism, so it is also
   the only place a timer can be armed -- which is where two of the CPU timer's
   limitations come from; see "CPU timer mode" below.

   The patched path is compiled only for `PL_LINUX && PY_VERSION_HEX >= 3.11`.
   `PyThreadState::native_thread_id` does not exist before CPython 3.11, and
   only Linux can derive a usable per-thread CPU clock from it. Everywhere else
   the tree still compiles (the lookup miss falls through to upstream's
   `continue`) but yields nothing, so `CpuProfiler::check_supported` in
   `rust/src/cpu/mod.rs` refuses to start it rather than report a blank profile:
   non-Linux is rejected because `rust/build.rs` does not build the archive
   there, CPython 3.10 by an explicit version check.

   **TODO(macos):** `ThreadInfo::create()` on Darwin calls
   `pthread_mach_thread_np()` on a `pthread_t` copied out of a concurrently
   changing interpreter list, which is undefined for a stale value. The likely
   fix is to use `native_id` directly (CPython sets `native_thread_id` from
   `pthread_mach_thread_np(pthread_self())` on macOS, so it is already the mach
   port) and let `update_cpu_time()`'s `thread_info()` call fail cleanly for a
   dead port -- needs verifying against CPython 3.11-3.14 first.

2. **`src/echion/vm.cc`, `echion/echion/vm.h`** -- dropped
   `__attribute__((constructor))` from `init_safe_copy()` and renamed it to
   `pyroscope_init_safe_copy()`.

   Upstream runs it at `dlopen`, i.e. on `import pyroscope`, installing
   process-wide SIGSEGV/SIGBUS handlers and a 1 MiB alt stack whether or not
   this profiler is ever selected. `src/pyroscope_entry.cpp` calls it
   explicitly (under a `std::once_flag`) when the sampler starts instead.

   Deliberately never uninstalled on stop: the handler chains to the previous
   disposition for anything it does not own, so leaving it installed is benign,
   whereas tearing it down while `fast_copy_active` stays true would leave
   `safe_memcpy` without its recovery path on a subsequent restart
   (`Sampler::sampling_thread` only reinstalls under its own `std::once_flag`).

3. **`src/sampler.cpp`, `stack_atfork_child()`** -- do not restart the sampler
   in the fork child.

   Upstream calls `Sampler::restart_after_fork()` there. Pyroscope's fork
   policy is the opposite: `rust/src/lib.rs::at_fork_after_in_child` stops
   profiling and deliberately leaks the agent, because the agent's threads did
   not survive the fork. An auto-restarted sampler would keep pushing samples
   into a Rust sink that no uploader is draining.

   The cleanup itself is kept, and kept on `pthread_atfork` rather than moved
   to Rust: it re-inits echion's mutexes and maps with placement new, and
   `pthread_atfork` also covers a raw `fork(2)` from C, which
   `os.register_at_fork` would miss. Because that handler already ran the
   cleanup, `pyroscope_cpu_ddtrace_postfork_child()` must not call
   `Sampler::postfork_child()` again.

4. **`src/cpu_timer.cpp`, `render_raw_sample()`** -- render the captured
   physical stack directly instead of going through
   `ThreadInfo::sample_cpu_timer()`.

   That function is the drain-time asyncio/greenlet attribution listed under
   "Other things dropped". With no Python layer populating them, `asyncio_loop`
   is always 0 and `greenlet_info_map` is always empty, so it would fall straight
   through to exactly the render we now do unconditionally -- upstream's own
   fallback branch, taken always. Output is identical; what it buys is keeping
   ~500 lines of task-identity matching out of `src/echion/threads.cc`, and
   keeping the `thread_info_map` lock off the drain path.

   `RawSample::asyncio_task`, `greenlet_id` and `coroutine_fingerprints` are
   still captured and now go unread, in the same way
   `src/thread_span_links.cpp` is still compiled but inert. That is deliberate:
   the signal handler is the most safety-critical code in this tree and is kept
   byte-identical to upstream rather than hand-trimmed to save a few stores.

5. **`src/sampler.cpp`, `capture_samples()` and `sampling_thread()`** -- in
   CPU-timer mode the wall walk becomes discovery-only, on its own cadence.

   Upstream keeps unwinding in CPU-timer mode and only stops *weighting* those
   samples with CPU time (`include_cpu_time=false`), because it still publishes
   a wall-time profile from them. Pyroscope publishes only `process_cpu`, and
   `Pyroscope::CpuSample::export_sample()` drops a sample with no CPU weight, so
   unwinding every Python thread would be pure overhead -- the opposite of the
   point of running the timer-driven sampler.

   The walk still has to happen, because it is what discovers threads and arms
   their timers (patch 1), so the callback is simply skipped. It runs at the
   configured sample rate, capped by `g_cpu_timer_discovery_max_interval_us`
   (100 ms) for slow rates. Running it at the sample rate is still strictly
   cheaper than wall-clock mode, which does the same walk *plus* a full unwind at
   that rate.

6. **`include/cpu_timer.hpp`, `DebugStats`** -- three added fields
   (`configuration_frozen`, `stopped_once`, `min_interval_ms`).

   `src/pyroscope_entry.cpp` has to answer "could a start succeed right now?"
   without mutating anything, so that `CpuProfiler::check_supported` can raise
   out of `pyroscope.configure()`. Upstream never asks: the CPU timer is an
   opt-in env var that degrades quietly. All three are already known to
   `EngineState`; publishing `min_interval_ms` in particular beats copying the
   engine's floor into the entry point and letting the two drift.

   `shim/dd_wrapper/include/profiler_stats.hpp` likewise gains the two no-op
   setters the reworked sampling loop calls.

## CPU timer mode

Three consequences of `CpuProfiler::DdtraceCpuTimer` that are worth knowing
before choosing it over the wall walk. All three are reported as errors out of
`pyroscope.configure()` where they can be detected up front; see the `Status`
enum in `src/pyroscope_entry.cpp` and `ddtrace_start_error` in
`rust/src/cpu/native.rs`.

- **Once per process.** `CpuTimer::Engine` refuses to re-arm after it has been
  shut down: a `SIGPROF` left over from a deleted timer must not find live state
  again. So `configure()` -> `shutdown()` -> `configure()` fails, and so does
  switching modes within one process (the accounting mode is frozen at the first
  start so a profile can never mix two ways of measuring CPU). A fork child of a
  CPU-timer session *can* start fresh, because its timers did not survive the
  fork and `Engine::postfork_child()` rebuilds the control plane -- but it
  inherits the parent's sample rate.

- **Arming is remote.** It happens on the sampler thread, not on the thread
  being armed, so `Engine::register_thread()` cannot install an alternate signal
  stack for the target thread and cannot inspect its signal mask. A thread that
  blocks `SIGPROF` therefore contributes no samples, rather than being detected
  and disabling the engine: missing CPU, not a crash. Guarded reads in the
  handler still recover, they just run on the thread's ordinary stack, which is
  fine because the faults are bad-pointer reads and not stack overflow.

- **Threads are unarmed until the next discovery walk**, so a short-lived
  thread's early CPU is never sampled. Measured on a workload of threads living
  300 ms: a 100 ms discovery period accounted for 79% of the process's CPU, and
  matching the 10 ms sample period brought that to 95%. This is why discovery
  tracks the sample rate (patch 5).

Upstream's private diagnostics (`Engine::debug_stats()`: dropped samples, timer
overruns, capture failures, disable reasons) have no Pyroscope channel to go to
and are not surfaced anywhere yet. They are the first thing to reach for when a
CPU-timer profile looks wrong.

## Sample weighting

Both modes report **real CPU nanoseconds**, never a count of ticks.
`rust/src/cpu/mod.rs` tags the batch `ReportData::ReportsCpuNanos` so
`encode::pprof` writes those nanoseconds verbatim instead of multiplying by the
sampling period.

The wall walk wakes on an interval and walks every Python thread, producing one
stack per thread per tick whether or not that thread ran.
`StackRenderer::render_thread_begin` therefore weights each sample by the
per-thread CPU delta echion supplies via `render_cpu_time()`, and
`export_sample()` drops samples from threads that consumed no CPU.

Weighting by the sampling period instead (one tick = one period of CPU, which
is correct for py-spy with `gil_only`) inflates the profile by roughly the
thread count and counts blocked threads as busy: 13.6s of "CPU" was measured
for a process that used 3.7s.

The CPU timer's weight is not a per-tick delta at all: the handler reads the
thread's own CPU clock at capture time, so `RawSample::cpu_delta_ns` is exactly
the CPU that elapsed between the previous expiration on that thread and this
one. `StackRenderer::render_cpu_sample_begin` takes it as given and
`render_cpu_time()` is never called on that path. This is also why
`si_overrun` must **not** be folded into the weight: coalesced expirations are
already covered by the clock delta, and adding them would double-count. In this
mode the wall walk stops reporting CPU entirely (patch 5), which is what keeps
the two paths from both billing the same CPU -- a bug the end-to-end test's 2x
ceiling exists to catch.

## Configuration notes

- `Sampler::set_interval()` takes **fractional seconds**, not microseconds.
- The stack depth cap lives in echion's `max_frames` global (`echion/config.h`),
  not on `Sampler`.
- Adaptive sampling is deliberately **disabled** in `pyroscope_entry.cpp`. It
  varies the interval to hit a CPU-overhead target, which would make the sample
  rate the user asked for a suggestion rather than a setting and quietly trade
  samples for overhead under load.
- `UNWIND_NATIVE_DISABLE` and `PL_LINUX`/`PL_DARWIN` compile definitions are
  required, mirroring upstream's `stack/CMakeLists.txt`. Without `PL_*`,
  `echion/danger.h` compiles out its platform memory-copy primitives and the
  whole tree fails to resolve.
- `_POSIX_C_SOURCE` is deliberately **not** defined (unlike
  `cpp/CMakeLists.txt`): on macOS it selects the strict POSIX namespace, which
  hides BSD extensions the vendored code uses -- `echion/danger.cc` calls
  `getpagesize()`.
- The CPU timer period is `1000 / sample_rate` ms, floored by the engine's
  `kMinIntervalMs` (2 ms). Rather than let it clamp silently, the pre-flight
  refuses a `sample_rate` above 500 Hz, because a rate the user set is a setting
  and not a suggestion. `src/cpu_timer.cpp` explains the floor: 1 ms `SIGPROF`
  delivery was found to livelock some condition/event-loop handoff paths even
  with `SA_RESTART`.
- The CPU timer caps stack depth at its own `kMaxCpuTimerFrames` (512),
  independent of `max_frames`, because those frames are captured into
  preallocated per-thread ring slots and a high cap multiplies into per-thread
  resident memory.
- `src/cpu_timer.cpp` allocates `EngineState` from a dynamic initializer, so
  `cpu_timer.cpp.o` has an `.init_array` entry (as `sampler.cpp.o` already
  does for `stack_init`). It installs no signal handlers and arms no timers, so
  `import pyroscope` still has no effect on signal dispositions -- which is the
  property patch 2 exists to preserve.

## Re-vendoring

`scripts/check-cpu-profilers-multiversion.sh` compiles this tree against
CPython 3.10-3.14 in about a minute; run it after any update, because that is
where version-specific CPython internals break.

Compiling is not enough for the CPU timer: its signal handler walks
`_PyInterpreterFrame` through layouts that differ per version (`cframe` on 3.12,
`current_frame` on 3.13, a tagged `f_executable` on 3.14), and every one of those
fields exists on every version it compiles for. Run
`scripts/tests/test_ddtrace_cpu.py` against 3.12, 3.13 and 3.14 as well -- it
fails if the reported CPU stops tracking `os.times()`.
