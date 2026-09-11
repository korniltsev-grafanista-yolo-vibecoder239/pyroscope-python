# PYROSCOPE_PATCH: cpu-build — compile the sampler with local sample/export stubs.
set(PROFILING_ROOT ${CMAKE_CURRENT_SOURCE_DIR}/../vendor/dd-trace-py/ddtrace/internal/datadog/profiling)
set(STACK_ROOT ${PROFILING_ROOT}/stack)

find_package(Threads REQUIRED)
add_library(datadog_cpu_profiler STATIC
    ${STACK_ROOT}/src/echion/danger.cc
    ${STACK_ROOT}/src/echion/frame.cc
    ${STACK_ROOT}/src/echion/greenlets.cc
    ${STACK_ROOT}/src/echion/interp.cc
    ${STACK_ROOT}/src/echion/long.cc
    ${STACK_ROOT}/src/echion/mirrors.cc
    ${STACK_ROOT}/src/echion/stack_chunk.cc
    ${STACK_ROOT}/src/echion/stacks.cc
    ${STACK_ROOT}/src/echion/strings.cc
    ${STACK_ROOT}/src/echion/tasks.cc
    ${STACK_ROOT}/src/echion/threads.cc
    ${STACK_ROOT}/src/echion/vm.cc
    ${STACK_ROOT}/src/gc_frame_tracker.cpp
    ${STACK_ROOT}/src/origin_task_links.cpp
    ${STACK_ROOT}/src/sampler.cpp
    ${STACK_ROOT}/src/stack.cpp
    ${STACK_ROOT}/src/stack_renderer.cpp
    ${STACK_ROOT}/src/thread_span_links.cpp
    ${PROFILING_ROOT}/dd_wrapper/src/native_call_tracker.cpp
    ${PROFILING_ROOT}/dd_wrapper/src/profiler_stats.cpp
)

# The overlay must precede PROFILING_ROOT. Only the three wrapper interfaces are
# replaced; all sampler, Echion, statistics, and registry sources stay upstream.
target_include_directories(datadog_cpu_profiler PRIVATE
    ${CMAKE_CURRENT_SOURCE_DIR}/stubs
    ${STACK_ROOT}/include
    ${STACK_ROOT}/include/util
    ${STACK_ROOT}/echion
    ${PROFILING_ROOT}
    ${PROFILING_ROOT}/dd_wrapper/include
    ${Python3_INCLUDE_DIRS}
)
target_compile_definitions(datadog_cpu_profiler PRIVATE UNWIND_NATIVE_DISABLE)
if(APPLE)
    target_compile_definitions(datadog_cpu_profiler PRIVATE PL_DARWIN)
else()
    target_compile_definitions(datadog_cpu_profiler PRIVATE PL_LINUX)
endif()
target_link_libraries(datadog_cpu_profiler PRIVATE Threads::Threads)
install(TARGETS datadog_cpu_profiler ARCHIVE DESTINATION .)
