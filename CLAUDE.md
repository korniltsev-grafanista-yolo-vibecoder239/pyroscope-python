# Current project: vendor Datadog's CPU profiler

The goal is to make CPU profiling more efficient and reduce its overhead in
pyroscope-python by vendoring Datadog's Python CPU profiler implementation from
https://github.com/DataDog/dd-trace-py.

The existing CPU profiler uses py-spy through the Rust backend. The vendored
implementation is preparation for a future integration; it does not change the
active profiler yet.

## Current milestone: compile and link

The unchanged source import is complete. Compile and link the vendored CPU
profiler into the native extension without adding a Rust feature. Activation is
a runtime concern; keep the profiler inactive and retain the py-spy backend.
Local no-op sample/export interfaces are allowed for this milestone. Minimize
changes to upstream code. Runtime integration and overhead measurements are
future work. Commit and push changes without creating a pull request.

## Document every Datadog patch

Every subsequent change to Datadog-derived source must have an adjacent
`// PYROSCOPE_PATCH: <id> — <reason>` comment and a matching entry in root
`vendor.md`. Use begin/end comments around larger modified blocks. Use the
language's comment syntax for non-C++ files.

Record affected files, upstream behavior, the change and rationale, and any
temporary limitation or removal condition. Update the source markers and
`vendor.md` in the same commit as the patch. Document and mark local stubs and
build substitutions too, even when the vendored files remain unchanged.

Preserve the pinned upstream revision, license notices, and original
`SHA256SUMS` import baseline. Document deviations instead of replacing upstream
checksums with checksums of patched files. Do not reformat unrelated vendor code.
