#pragma once

// PYROSCOPE_PATCH: cpu-sample-stubs — retain registry ownership without libdatadog state.
#include "native_call_tracker.hpp"

namespace Datadog {

class ProfilerState
{
  public:
    static ProfilerState& get()
    {
        static ProfilerState state;
        return state;
    }
    NativeCallRegistry native_call_registry;

  private:
    ProfilerState() = default;
};

} // namespace Datadog
