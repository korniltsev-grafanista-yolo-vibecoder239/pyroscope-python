# Datadog vendor changes

CPU profiler source: https://github.com/DataDog/dd-trace-py at
`9f39d0de3ab66fb125a94d265c636c8be51b15cb`.

The original import is under `cpp/vendor/dd-trace-py`. Its `SHA256SUMS` remains
the unchanged upstream baseline. Document every subsequent Datadog source patch
and local build/interface substitution here, with matching `PYROSCOPE_PATCH`
identifiers in the affected code. Each entry must describe affected files,
upstream behavior, the change, its rationale, and limitations/removal conditions.

No CPU profiler source patches or build substitutions have been applied yet.
The pre-existing memory-profiler adaptations predate this log; subsequent
changes to them must follow the same documentation policy.
