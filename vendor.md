# Datadog vendor changes

CPU profiler source: https://github.com/DataDog/dd-trace-py at
`9f39d0de3ab66fb125a94d265c636c8be51b15cb`.

The original import is under `cpp/vendor/dd-trace-py`. Its `SHA256SUMS` remains
the unchanged upstream baseline. Document every subsequent Datadog source patch
and local build/interface substitution here, with matching `PYROSCOPE_PATCH`
identifiers in the affected code. Each entry must describe affected files,
upstream behavior, the change, its rationale, and limitations/removal conditions.

The pre-existing memory-profiler adaptations predate this log; subsequent
changes to them must follow the same documentation policy.

## cpu-build: build the inactive sampler into Pyroscope

Affected files: `cpp/CMakeLists.txt`, `cpp/cpu/CMakeLists.txt`, `rust/build.rs`;
packaging and CI inputs also include the new target and its source files.

Upstream builds a separate `_stack` Python module linked to `dd_wrapper` and
libdatadog. Pyroscope instead builds a static CPU archive from all 18 stack/Echion
translation units and the unchanged native-call registry and profiler statistics.
It links this archive only into the `pyroscope._native` cdylib, with
`PyInit__stack` rooted so linker elimination cannot omit the sampler. Excluding
the archive from Rust test links also prevents shared C++ template symbols from
pulling Python-dependent CPU objects into test executables. The C++ runtime is
listed after the CPU archive to resolve its references with one-pass linkers.
The extension never calls or
registers that entry point. The existing py-spy backend remains active.

CPU compilation is unconditional during native builds; the existing `memory`
feature controls only the memory target. Miri skips native compilation because
it interprets Rust. The build uses the exact target Python headers and upstream
platform definitions, with native unwinding disabled. Runtime activation and
free-threaded compatibility are deferred. Replace the link-only entry-point root
when a real runtime integration keeps the sampler reachable.

## cpu-sample-stubs: temporary sample/export interface

Affected files: the three headers in `cpp/cpu/stubs/dd_wrapper/include/`.

Upstream `sample.hpp`, `sample_manager.hpp`, and `profiler_state.hpp` depend on
libdatadog profile/export types. A CPU-target-only include overlay replaces those
interfaces without editing their upstream files. Sample creation returns null;
interning returns empty results; sample writes and flushes discard data and
return failure. IDs remain opaque pointer types compatible with renderer caches.
Statistics borrows serialize access to real upstream counters, and state retains
the unchanged native-call registry. No exporter, dictionary, uploader, or
libdatadog library is built or initialized.

These stubs exist only to compile and link. They do not provide working CPU
profiling or runtime/fork guarantees. Remove them when wiring samples to
Pyroscope's profile builder in the runtime integration milestone.
