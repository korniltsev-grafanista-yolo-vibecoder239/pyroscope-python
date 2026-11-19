# TODOs

- [ ] Guard the shift by the untrusted frame owner in `python-unwind/src/lib.rs:223`.
  - `owner` is an untrusted byte read from frame memory. In `1 << owner`, values of 8 or more panic with overflow checks enabled (debug and test builds), aborting inside the signal handler.
  - In release builds, the shift amount wraps, allowing garbage owners to alias valid owners.
  - Use an `owner < 8 && ...` guard or `checked_shl` to address both cases.

- [ ] Validate the thread identity or static TLS range when accepting `Location::Fixed` in `python-thread-state/src/tls.rs:233`.
  - Equal deltas alone can classify two dynamic TLS blocks as static if they happen to sit at the same distance from their respective thread pointers.
  - Fixed mode has no `bound_to_this_thread` check, so this theoretical coincidence is not rejected.
  - Apply the same thread-id validation in Fixed mode, or require the delta to fall within a plausible static TLS range.
  - Proposed stronger Fixed-versus-DTV detection for glibc/x86-64, performed outside the signal handler:
    1. Obtain the owning library's actual `link_map`.
    2. Read `l_tls_offset`; reject `NO_TLS_OFFSET` and `FORCED_DYNAMIC_TLS_OFFSET`.
    3. Verify `dlpi_tls_data == thread_pointer - l_tls_offset`.
    4. Keep that library loaded for the reader's lifetime.
  - Rationale: these are the fields glibc uses to place static TLS in each thread. Reference: [glibc TLS allocation code](https://codebrowser.dev/glibc/glibc/elf/dl-tls.c.html#_dl_allocate_tls_init).
  - `l_tls_offset` is private glibc ABI. The reported diagnostic located it through the running libc's `_thread_db_link_map_l_tls_offset` metadata, avoiding a hardcoded structure offset. This approach needs compatibility validation and must reject initialization when the metadata is unavailable.
  - Alternative proposal: detect whether the TLS block lies inside the probe thread's own stack mapping. For a pthread-created probe thread, static TLS lies inside its stack block, while dynamic TLS cannot. Equal block-to-thread-pointer deltas are a consequence of static TLS rather than the property to detect.
  - Reported rationale, checked in glibc 2.43:
    - Static versus dynamic is a per-module decision stored in private `l_tls_offset`. The first `__tls_get_addr` on an undecided module marks it forced-dynamic permanently (`elf/dl-tls.c:987`). By initialization, libpython's variable has already been accessed, so the decision is final and one thread is enough to observe it.
    - For pthread-created threads, `allocate_stack` places the thread descriptor at the top of the stack block and lays out static TLS relative to it (`nptl/allocatestack.c:414-454`). This also holds for user-supplied stacks and for both x86-64 and aarch64.
    - Dynamic TLS blocks come from `malloc`, which never returns memory inside a thread's stack mapping.
  - Apply the stack-mapping proposal in `locate`:
    1. Allocate the probe thread's stack with `mmap` and supply it through `pthread_attr_setstack`, giving exact bounds without relying on `pthread_getattr_np`.
    2. On the probe thread, obtain the block through `__tls_get_addr` as today and test membership in `[stack, stack + size)`.
    3. If inside, classify the module as static and return `Fixed`, still requiring equal deltas on both threads as a consistency check.
    4. If outside, classify the module as dynamic and return `Dtv`, still requiring `in_dtv` on both threads.
    5. Fail startup with `UnknownTlsLayout` for any other outcome, as now.
  - Only test the probe thread this way. The main thread's static TLS is allocated by the loader at startup outside its stack, and the calling thread may be the main thread.
  - Alternative tradeoffs: reading `l_tls_offset` directly is authoritative but adds a third private-layout dependency because `link_map` has no public accessor for it. Asking for the static TLS size through `_dl_get_tls_static_info` requires a `GLIBC_PRIVATE` symbol, considered worse than the stack check in this proposal.
  - Cheap backstop: apply `bound_to_this_thread` in Fixed mode too. The proposed cost is one extra load per sample, with misclassification becoming visible `no_thread_state` drops. Verify the CPython assumption that the thread state in the slot always carries the calling thread's id because both are set when the state is bound; this has not been verified.

- [ ] Address unchecked safety preconditions in safe public functions (Rust soundness).
  - These functions are callable from safe code but perform raw reads that can crash the process unless kindasafe is initialized and its recovery signals are unblocked:
    - `unwind::handler` and `unwind::unwind` in `python-unwind/src/lib.rs:107` and `python-unwind/src/lib.rs:180`.
    - `thread_state::current` on Python 3.15 in `python-thread-state/src/lib.rs:154`.
    - `Resolver::resolve_stack` in `python-resolve/src/lib.rs:95`.
  - `Resolver::resolve_stack` also reads whatever address a caller supplies in `Frame { code }`, including memory under a live `&mut`, which the crate's own documentation forbids.

- [ ] Fix CTimer bookkeeping for threads that never call `Thread._delete`.
  - Create a reproducer test for the dummy-thread cleanup leak and the sampling loss when a tid is reused.
  - `_DummyThread` (a C-created thread that touches `threading`) registers through `_set_native_id` but is removed from `_active` without `_delete` in Python 3.12 and 3.15, so `unregister_thread` never runs.
  - The kernel timer and map entry remain until the ident is reused or the session ends. `Timer::thread_created` in `signal-timer/src/lib.rs:165` does nothing when the tid already has an entry, so a new thread with a recycled tid is silently never sampled.
  - Proposed fixes below have not been implemented or tested.
  - Required fix: make registration replace a stale entry. In the `Backend::CTimer { interval, threads }` branch of `Timer::thread_created`, always create a timer with `ctimer::CTimer::start(tid, Self::SIGNAL, *interval)?`, then call `lock(threads).insert(tid, timer)` to drop any existing timer for that tid, and return `Ok(())`. Update the method's doc comment accordingly.
  - Replacement rationale: a thread registers from its own `_set_native_id`, so an existing entry for its tid can only belong to a dead thread. A timer bound to a dead thread's CPU clock never fires for the new thread. `attach` operates on a new session's empty map, and each thread registers itself only once, so both callers are safe with replacement.
  - Leak cleanup proposal: tie cleanup to OS thread exit using a Rust `thread_local!` guard, such as `RefCell<Option<ExitGuard>>`, armed by `register_thread` on the thread being registered, including dummy threads. Store `id: u64` and `tid: libc::pid_t` in the guard. Its `Drop` should remove `LIVE[id]` only when it still equals `tid`, release the `LIVE` lock, then call `signal_sampler::thread_removed(timer, tid)` if `TIMER` is present. The destructor runs at OS thread exit regardless of whether Python calls `_delete`.
  - Arm the exit guard only when `native_id == gettid()`. Install-time and attach paths register other threads from the configuring thread and must not arm that caller's guard for them.
  - Keep the `_delete` hook for threads that existed before hooks were installed and therefore never call `register_thread` themselves. A pre-existing dummy thread remains uncovered by this cleanup; replacing stale entries still prevents recycled-tid sampling loss.
  - The exit destructor must take only Rust mutexes, in the same order as `register_thread`, and never acquire the GIL, to avoid deadlock.
  - Less-preferred alternative: a Python sentinel in `threading.local()` whose `__del__` calls unregister, as used for dummy threads in CPython 3.13. This depends on thread-state cleanup and `__del__` running, which is not guaranteed at interpreter shutdown.

- [ ] Investigate using `py.detach(...)` for the TLS probe in `python-thread-state` instead of manually saving and restoring the Python thread state.
  - Check whether it can replace the `PyEval_SaveThread` / `PyEval_RestoreThread` sequence while preserving the probe's required detached state and restoring the original state afterward.

- [ ] Analyze the compiled `python-unwind` rlib for async-signal-unsafe function invocations.
  - Inspect the compiled code, not just the source, for anything reachable from the signal handler path that is not async-signal-safe: `Drop` glue, mutex or lock acquisition, heap allocation and deallocation, `abort`, `assert!`/`debug_assert!` and other panic paths (including overflow and bounds checks), and formatting.
  - Check both debug and release builds, since overflow checks and debug assertions only show up in some profiles.
