#pragma once

// PYROSCOPE_PATCH: cpu-sample-stubs — sample storage is unavailable in this milestone.
#include "sample.hpp"

namespace Datadog {

class SampleManager
{
  public:
    static Sample* start_sample() { return nullptr; }
    static void drop_sample(Sample* sample) { delete sample; }
};

} // namespace Datadog
