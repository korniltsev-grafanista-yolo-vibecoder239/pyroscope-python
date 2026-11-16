# CPython 3.15 Signal: execution-scoped pthread bindings

This alternative starts at `6efc4719952f2153de38d2fd817a0fac25a05a93`.
CPython 3.15 changed its bound-state lookup to compiler TLS. Calling its getter
from SIGPROF can enter a TLS resolver, so the baseline rejects Signal on 3.15.
The existing 3.15 frame offsets were already usable.

The implementation maintains its own pthread slot using ordinary Python
execution callbacks. SIGPROF reads this slot, then uses the existing kindasafe
frame walker. There are no CPython calls, thread-list searches, TLS discovery,
thread-identity queries, allocations, or locks in the 3.15 lookup. Frame and
resolver reads remain direct loads with the existing recovery preconditions.

| Reference examined | First-wave mechanism | Difference in this implementation |
| --- | --- | --- |
| [A: 59e9175](https://github.com/korniltsev-grafanista-yolo-vibecoder239/pyroscope-python/commit/59e917545e2ed40cc32204d760ef4ec5b202f167) | Bounded interpreter thread-list search in the handler, matching bound status and native identities | Execution events publish a binding on the actual thread; the handler does not search lists or match identities. |
| [B: 537b2ea](https://github.com/korniltsev-grafanista-yolo-vibecoder239/pyroscope-python/commit/537b2eac3025f17339544a353607afef4c56f8fe) | Locate CPython's bound TLS variable and read it through ELF DTV / Mach-O storage | An SDK-owned pthread key contains a separate execution binding; no CPython TLS variable is discovered or read. |
| [C: f77a0e1](https://github.com/korniltsev-grafanista-yolo-vibecoder239/pyroscope-python/commit/f77a0e1df217fa9bf4838039bc2f7a4b4281a493) | Discover a startup TLS offset / Darwin descriptor and read CPython's native slot | The binding comes from callbacks and has an explicit execution lifetime; no loader metadata, ABI offsets, or startup-TLS requirement. |

## Lifetime invariant

Only a currently executing Python scope may publish a state address. Entry,
resume, throw, and line events register it. Return, yield, and exception-unwind
events clear it **before the last Python frame leaves**. Nested exits retain the
binding for native work continuing in an outer Python caller. CPython 3.15 uses
interpreter-owned sentinel frames at C evaluator entries as well as at the base;
the exit callback follows these sentinels to find any remaining Python caller.
This was verified against [CPython's evaluator](https://github.com/python/cpython/blob/v3.15.2/Python/ceval.c)
and [monitoring event semantics](https://docs.python.org/3.15/library/sys.monitoring.html#events).

Native work called within that scope remains sampleable without the GIL. Outside
Python, even a retained bound state is ineligible. Each session uses a fresh epoch,
and pthread destruction clears the slot before freeing its small allocation.
Neither a stopped session nor a recycled pthread can revive an old binding.

A dictionary-owned capsule is insufficient for this invariant. An extension may
retain `PyThreadState_GetDict()` after CPython clears/deletes the state. The
regression reproduced a nonzero freed address in that earlier candidate even
though its freed memory still contained a finalizing flag and the profile appeared
to drop samples correctly. The final implementation does not use dictionaries or
capsule destruction. Its regression checks the binding is zero **before any
state-memory read**, while retaining the dictionary, and verifies all 13 native
signal deliveries become `[dropped: no_thread_state]`. The seven deliveries during
Python execution have the complete stack `outer;leaf`.

## Validation

Executed on Linux x86-64, CPython 3.15.2, 3.14.10, and 3.10.22:

- `cargo test --locked --workspace -- --test-threads=1` for each interpreter.
  Interpreter paths, library paths, and build targets are selected separately;
  relocatable 3.10 also needs `PYTHONHOME` when the Rust test embeds it.
- The 3.15 Signal Rust tests also pass with `PYTHON_JIT=1` (JIT available/enabled).
- Real explicit SIGPROF deliveries verify exact complete stacks and counts for
  retained dictionaries, actual pthread reuse with an abandoned bound state,
  stop/restart during detached native work, generator yield/throw, exceptions,
  and native continuation after a nested Python callback.
- A preexisting loop with no further function calls registers through LINE events.
  Its real CTimer test requires at least 160 periods and at least 95% on the complete
  stack `spin`, including drops in the denominator. The JIT-enabled run observed
  exactly `spin 161`.
- Packaged integration tests cover ITimer and CTimer, native PBKDF2 work without
  the GIL, old/new `threading.Thread` workers, raw `_thread` and foreign pthread
  callbacks under ITimer, overruns, raw fork, timer-arm failure, hook ownership,
  recovery-handler preservation, and shutdown/restart behavior.
- The 3.15 benchmark preflight verifies all 82 frames and line numbers. It measures
  synchronous unwinding/resolution, not execution-event overhead.
- Rust formatting, Clippy with warnings denied, Python formatting/import order,
  and Go formatting checks.

Integration assertions match whole root-to-leaf paths, require substantial total
weight, and retain full profiles in diagnostics. Equal CPU budgets require 35–65%
per worker (two workers), or 15–35% each (four workers), and at least 90% combined
coverage. Four CPU seconds per worker retain at least 300/600 measured periods
despite ITimer undercounting. The sequential cohorts inherit affinity to one
allowed CPU so migrations and CPU-specific tick phases do not bias the comparison.
The existing concurrent-worker tests remain unpinned. No assertion was relaxed
to accept a missing stack.

An observed 3.15 CTimer profile (nanoseconds) was:

```text
Thread._bootstrap;Thread._bootstrap_inner;Thread.run;new_thread;python_leaf 3990000000
Thread._bootstrap;Thread._bootstrap_inner;Thread.run;preexisting;native_leaf 4010000000
```

The four-cohort ITimer profile from the same test run was:

```text
foreign_thread;native_leaf 2440000000
raw_thread;native_leaf 2380000000
Thread._bootstrap;Thread._bootstrap_inner;Thread.run;new_thread;python_leaf 2370000000
Thread._bootstrap;Thread._bootstrap_inner;Thread.run;preexisting;native_leaf 2360000000
```

The exact boundary-test profile, in sampling periods, is:

```text
[dropped: no_thread_state] 59
generator 18
parent 13
parent;callback 3
raises 5
```

## Costs and limits

This design trades sampling-only overhead for execution callbacks. A local
arithmetic loop of one million iterations, using median thread CPU time across
five repetitions at 100 Hz, took 0.026347 seconds without profiling and 0.091457
seconds with Signal (3.47×). This is a specific microbenchmark, not an application
performance guarantee. Monitoring can change JIT optimization behavior.

One monitoring tool ID must be available, and entry/exit events must remain
deliverable. Other instrumentation must not suppress or prevent those callbacks.
Already-running native work has no binding until its next enabled Python event,
including after restart. Manually swapping/migrating live states, deleting an
executing state, subinterpreters, free-threaded builds, and indexed stack references
are unsupported. Raw/foreign threads do not acquire CTimer timers automatically.
Linux ARM64, musl, and macOS runtime execution was not available locally; CI covers
the repository's platform matrix and adds a macOS 3.15 Signal test job.
The README retains the complete recovery-handler, signal-mask, and deferred-read
contracts; this design adds no runtime recovery-handler ownership checks.
