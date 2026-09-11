# Datadog CPU profiler source import

Upstream: https://github.com/DataDog/dd-trace-py

Revision: `9f39d0de3ab66fb125a94d265c636c8be51b15cb` (main, 2026-09-10).

All imported C++ sources and headers are byte-for-byte copies of that revision.
Paths below this directory match paths in the upstream repository.

The import contains the production `.cpp`, `.cc`, `.h`, and `.hpp` files from
these directories under `ddtrace/internal/datadog/profiling/`:

- `stack/src`, `stack/include`, and `stack/echion`: CPU/wall-time sampler,
  Python extension entry point, stack rendering, and Echion stack unwinding.
- `dd_wrapper/src` and `dd_wrapper/include`: the sampler's shared state,
  sample/profile management, and upstream upload interface dependencies.
- `profiling_helpers`: shared Python frame and version compatibility headers.

This is a source-only import. No build files, Python/Cython glue, tests, fuzzers,
or generated artifacts are imported, and the existing build is unchanged.
The upstream wrapper depends on the external libdatadog/native library and its
generated `datadog/profiling.h`; these are not vendored here. CPython development
headers and platform/standard-library headers are also external dependencies.
Compilation and adaptation to Pyroscope's profile backend are future work.

`LICENSE`, `LICENSE.Apache`, `LICENSE.BSD3`, and `NOTICE` are unchanged upstream
files. Echion copyright and MIT notices remain in the source headers.
`LICENSE.echion` is copied from https://github.com/P403n1x87/echion, Git blob
`dcc99ea99aebfa474237854d9fede6b576ae51fd`.

`SHA256SUMS` records every imported source/header and license/notice file. Verify
the copies from this directory with `sha256sum --check SHA256SUMS`; no compilation
is required.
