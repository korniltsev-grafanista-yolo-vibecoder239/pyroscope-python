# Current project: vendor Datadog's CPU profiler

The goal is to make CPU profiling more efficient and reduce its overhead in
pyroscope-python by vendoring Datadog's Python CPU profiler implementation from
https://github.com/DataDog/dd-trace-py.

The existing CPU profiler uses py-spy through the Rust backend. The vendored
implementation is preparation for a future integration; it does not change the
active profiler yet.

## Initial scope

1. Record the project and goal in this file in a dedicated commit.
2. Copy the C++ sources and headers needed by Datadog's CPU profiler, preserving
   their upstream bytes, directory structure, and license notices. Record the
   upstream revision so the import can be verified, and commit it separately.
3. Push the commits without creating a pull request.

Do not compile, modify the imported sources, wire them into the build, or replace
the existing profiler during this initial import. Integration, compilation, and
overhead measurements are future work.
