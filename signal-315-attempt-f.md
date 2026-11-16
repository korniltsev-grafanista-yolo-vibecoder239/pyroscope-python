# Signal on CPython 3.15: attempt F

This branch starts at `6efc4719952f2153de38d2fd817a0fac25a05a93`.
That baseline already describes and verifies the 3.15 frame layout. Its Signal
startup rejects 3.15 because the bound-state getter moved from pthread TSS to
compiler TLS, whose resolver can allocate or enter the loader in SIGPROF.

## Different mechanism

An evaluator wrapper publishes the actual `PyThreadState *` on a normal Python
thread into a new, SDK-owned pthread key. The configuring thread is seeded
immediately; other threads publish on evaluator entry. The signal handler admits
a reader, loads that lease, walks the existing frame chain, and releases the
reader. All successful frame reads remain direct kindasafe memory loads.

The lifetime bridge is a weakref callback on CPython's thread-local sentinel.
Sentinel death invalidates the state before deallocation, regardless of retained
dictionaries or capsules. Ordinary teardown waits for already admitted readers;
the signal path never waits. Native thread exit clears the SDK key, including
when Python state is abandoned. The SDK never compares recyclable thread IDs.

The first-wave commits were inspected as reference evidence:

| Reference | Its lookup | Difference in F |
| --- | --- | --- |
| A, `59e917545e2ed40cc32204d760ef4ec5b202f167` | Bounded interpreter thread-list walk, bound flags, Python and native IDs | Lifecycle publication; no signal-time list traversal or identity matching |
| B, `537b2eac3025f17339544a353607afef4c56f8fe` | Discover CPython's TLS module/offset; read ELF DTV or Mach-O TLV storage | Own a new pthread key and populate it from evaluator events; no CPython TLS discovery or reads |
| C, `f77a0e1df217fa9bf4838039bc2f7a4b4281a493` | Discover startup ELF thread-pointer displacement / Mach-O key and offset | No thread-pointer displacement, executable dependency validation or TLV parsing |

The reason to explore this design is independence from CPython's compiler TLS
ABI. Its costs are significant: evaluator overhead, loss of tier-2/JIT execution,
and an initial gap for preexisting threads until they enter the evaluator again.
The configure caller has no such gap. The hook stays installed across stops;
custom evaluators and replacement `_thread._local` types are rejected at startup.
See README.md for the complete support and recovery contracts.

## Verification

Local runtime validation used Linux x86-64, CPython 3.10.22, 3.14.10 and 3.15.2.
ARM64 and macOS jobs were added to CI but were not run locally.

- Rust workspace tests passed on all three versions. On this shared host,
  existing millisecond timer-delivery tests needed a serial rerun; assertions
  were unchanged. The relocatable 3.10 interpreter required `PYTHONHOME` for
  embedded tests.
- 3.15 resolver/lifecycle tests also passed with `PYTHON_JIT=1`. This verifies
  operation when JIT is requested, not preservation of JIT execution.
- The benchmark preflight validated all 82 root-to-leaf Python frames and lines.
- Packaged Docker/Pyroscope Signal suites passed on 3.15 and 3.10: both timer
  modes, overruns, raw forks, arming failure, restarts, native CPU, preexisting/new
  threads, churn, raw threads, and recovery-handler lifecycle scenarios.
- Final sentinel/constructor hardening was additionally checked with the 3.15
  raw-fork, lifecycle and published-state integration tests.
- The kindasafe workspace, Python thread-hook tests on 3.10/3.15, Go profile-shape
  tests, formatting and 3.15 workspace/all-target clippy checks passed. Cargo
  emitted existing manifest warnings; clippy reported no code warnings.

The native lifecycle fixture uses actual SIGPROFs and asserts exactly this whole
profile at a 10 ms period; there is no sampling tolerance for deliberate signals:

| Complete root-to-leaf stack | Periods | CPU weight |
| --- | ---: | ---: |
| `<module>;existing;entered` | 23 | 230,000,000 ns |
| `<module>;reentered` | 46 | 460,000,000 ns |
| `[dropped: no_thread_state]` | 50 | 500,000,000 ns |

The 50 drops are 19 inside an already-running native call at installation,
11 after state deletion on a live native thread, 7 before Python entry on an
actually reused pthread with an abandoned bound state still linked, and 13
after deletion on that reused thread. Seventeen of `reentered`'s samples
interrupt a native SIGUSR1 handler. Both public and private dictionaries are
retained across deletion. Competing evaluator installation is also rejected.

Integration assertions match whole observed paths and require at least 90% of
all weight, including drops, on the expected stack. CTimer's two-second native
workloads require at least 1.6 seconds of recorded weight (20% timer tolerance).
Observed main, preexisting-thread and new-thread CTimer profiles each recorded
2,000,000,000 ns on their complete expected path. Churn and restart are checked
the same way; the original overrun test remains unchanged.

ITimer can systematically undercount CPU on this host. A separate minimal C
handler, without the SDK, delivered 154 ticks at 100 Hz during 2.00 thread CPU
seconds; at 25 Hz it delivered 62 ticks during 4.01 CPU seconds. The new ITimer
native workloads therefore run for six CPU seconds and require at least 300
delivered periods (3 seconds of weight), in addition to the unchanged 90% complete
stack coverage requirement. This establishes substantial native coverage without
claiming that delivered ITimer weight converges to actual CPU time. The original
100 Hz timer and integration assertions were not relaxed.

A call-heavy microbenchmark (one million calls to a trivial Python leaf, best of
three thread-CPU measurements) measured 27.8 ms before configuration versus
41.7 ms active and 41.4 ms stopped with JIT off. With JIT requested it measured
24.5 ms, 46.5 ms and 47.4 ms respectively. These are workload-specific observations
of the evaluator tradeoff, not general overhead guarantees.
