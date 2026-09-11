#pragma once

// PYROSCOPE_PATCH: cpu-sample-stubs — compile-only interface; no profile export.
// Remove this overlay when samples are connected to Pyroscope's profile builder.
#include "profiler_stats.hpp"

#include <cstdint>
#include <mutex>
#include <optional>
#include <string_view>
#include <unordered_map>

namespace Datadog {

struct StubStringId;
struct StubFunctionId;
using string_id = StubStringId*;
using function_id = StubFunctionId*;

inline std::optional<string_id> intern_string(std::string_view) { return std::nullopt; }
inline std::optional<function_id> intern_function(string_id, string_id) { return std::nullopt; }

// Preserve the borrow lifetime and serialization expected by upstream stats users.
class ProfileBorrow
{
    inline static std::mutex mutex;
    inline static ProfilerStats counters;
    std::unique_lock<std::mutex> lock{ mutex };

  public:
    ProfilerStats& stats() { return counters; }
};

class Sample
{
  public:
    static ProfileBorrow profile_borrow() { return {}; }
    bool push_monotonic_ns(int64_t) { return false; }
    bool push_threadinfo(int64_t, int64_t, std::string_view) { return false; }
    bool push_walltime(int64_t, int64_t) { return false; }
    bool push_cputime(int64_t, int64_t) { return false; }
    bool push_span_id(uint64_t) { return false; }
    bool push_local_root_span_id(uint64_t) { return false; }
    bool push_trace_type(std::string_view) { return false; }
    bool push_origin_task_id(uint64_t) { return false; }
    bool push_origin_task_name(std::string_view) { return false; }
    bool push_task_id(uint64_t) { return false; }
    bool push_task_name(std::string_view) { return false; }
    void push_frame(function_id, uint64_t, int64_t) {}
    void push_frame(std::string_view, std::string_view, uint64_t, int64_t) {}
    bool flush_sample() { return false; }
};

} // namespace Datadog
