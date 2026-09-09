// Pyroscope C ABI for the vendored dd-trace-py stack (echion) sampler.
//
// Replaces upstream's src/stack.cpp, which was a CPython extension module
// (PyMethodDef stack_start/stack_stop/register_thread/link_span/
// track_greenlet/...) driven from ddtrace/profiling/collector/stack.py. We
// vendor no Python, so this file drives the sampler directly instead.
//
// Dropped along with the Python layer, because all of them are Datadog product
// features with no Pyroscope equivalent and no bearing on CPU sampling cost:
// span linking, greenlet tracking, asyncio task attribution, and sys.monitoring
// native call tracking.

#include <Python.h>

#include <cstdint>
#include <mutex>

#include "cpu_timer.hpp"
#include "echion/config.h"
#include "echion/vm.h"
#include "sampler.hpp"

namespace {
std::mutex g_mutex;
bool g_running = false;
std::once_flag g_safe_copy_once;

// Statuses returned to rust/src/cpu/native.rs, which turns each into the
// message the user sees. Keep the two in step.
enum Status : int
{
    kOk = 0,
    kAlreadyRunning = 1,
    kInvalidSampleRate = 2,
    kSamplingThreadFailed = 3,
    kCpuTimerDidNotArm = 4,
    kModeAlreadyCommitted = 5,
    kSampleRateTooHighForCpuTimer = 6,
};

// Everything that can make a start fail before it has changed any state.
//
// Split out so rust/src/cpu/mod.rs can ask the question from
// CpuProfiler::check_supported() and raise out of configure(), which is this
// repo's contract for a profiler that cannot run here: reporting nothing under a
// profiler's name is worse than refusing to start it. Deliberately free of side
// effects -- in particular it must not call Engine::configure(), which is what
// freezes the process's CPU accounting mode.
//
// Must be called with g_mutex held.
int
start_preflight(uint32_t sample_rate_hz, bool want_cpu_timer)
{
    if (g_running) {
        return kAlreadyRunning;
    }
    if (sample_rate_hz == 0) {
        return kInvalidSampleRate;
    }
    if (!want_cpu_timer) {
        // The wall walk stops reporting CPU for the whole life of a process that
        // configured the timer engine, so asking for it here would profile
        // nothing.
        return Datadog::CpuTimer::Engine::get().configured_enabled() ? kModeAlreadyCommitted : kOk;
    }

    const auto stats = Datadog::CpuTimer::Engine::get().debug_stats();
    if (!stats.supported) {
        // Linux + CPython 3.12+ + GIL-enabled. rust/src/cpu/mod.rs refuses
        // before we get here, so reaching this means the two disagree.
        return kCpuTimerDidNotArm;
    }
    if (stats.configuration_frozen && !stats.configured) {
        // A wall-mode session already ran here and froze the mode.
        return kModeAlreadyCommitted;
    }
    if (stats.permanently_disabled || (stats.stopped_once && !stats.active)) {
        // The engine will not re-arm after a shutdown, nor after it disabled
        // itself. Both mean a session started now would sample nothing.
        return kCpuTimerDidNotArm;
    }
    // 1000/rate is the period Engine::configure() would be given; below its
    // floor the engine would silently clamp it and profile at a rate the user
    // did not ask for.
    if (1000ULL / static_cast<uint64_t>(sample_rate_hz) < stats.min_interval_ms) {
        return kSampleRateTooHighForCpuTimer;
    }
    return kOk;
}
} // namespace

extern "C" {

// Reports why pyroscope_cpu_ddtrace_start() would fail right now, or 0 if it
// would not. Changes nothing.
int
pyroscope_cpu_ddtrace_can_start(uint32_t sample_rate_hz, int cpu_timer_enabled)
{
    std::lock_guard<std::mutex> lock(g_mutex);
    return start_preflight(sample_rate_hz, cpu_timer_enabled != 0);
}

// The shortest CPU timer period the engine will arm, in milliseconds, or 0 if
// this build has no CPU timer at all. Exposed so the error message for a too-high
// sample_rate can quote the real limit instead of a copy of it.
uint64_t
pyroscope_cpu_ddtrace_min_interval_ms(void)
{
    return Datadog::CpuTimer::Engine::get().debug_stats().min_interval_ms;
}

// Starts the echion sampling thread.
//
// `sample_rate_hz` is samples per second and `max_nframes` caps stack depth.
//
// With `cpu_timer_enabled`, CPU time is measured by per-thread POSIX CPU timers
// delivering SIGPROF (see src/cpu_timer.cpp) at `sample_rate_hz`, and the
// echion wall walk is demoted to thread discovery. Without it, the wall walk
// itself produces the samples, weighted by each thread's CPU delta.
//
// Returns 0 on success, one of the Status values above on failure.
int
pyroscope_cpu_ddtrace_start(uint32_t sample_rate_hz, uint32_t max_nframes, int cpu_timer_enabled)
{
    std::lock_guard<std::mutex> lock(g_mutex);

    // Re-run the pre-flight rather than trusting the caller's earlier answer:
    // it is cheap, and the two calls are not atomic with respect to each other.
    if (const int status = start_preflight(sample_rate_hz, cpu_timer_enabled != 0); status != kOk) {
        return status;
    }

    // Install the SIGSEGV/SIGBUS recovery handlers and pick a memory-copy
    // strategy. Upstream does this from a library constructor; we patched that
    // out so importing pyroscope does not install signal handlers for users who
    // never select this profiler. See the Pyroscope patch in src/echion/vm.cc.
    //
    // Deliberately never torn down on stop: the handler chains to the previous
    // disposition for anything it does not own, so leaving it installed is
    // benign, whereas uninstalling it while fast_copy_active stays true would
    // leave safe_memcpy without its recovery path on a subsequent restart
    // (Sampler::sampling_thread only reinstalls under its own std::once_flag).
    std::call_once(g_safe_copy_once, []() { pyroscope_init_safe_copy(); });

    auto& engine = Datadog::CpuTimer::Engine::get();
    const bool want_cpu_timer = cpu_timer_enabled != 0;

    // The interval the kernel arms each per-thread timer with. Truncating
    // division is deliberate: it rounds the period down, i.e. the rate up, so
    // the user never silently gets fewer samples than requested. The engine
    // enforces its own floor on top of this, which is what
    // kSampleRateTooHighForCpuTimer detects below.
    const uint64_t requested_interval_ms = 1000ULL / static_cast<uint64_t>(sample_rate_hz);
    engine.configure(want_cpu_timer, requested_interval_ms);

    // configure() is a no-op once the mode is frozen, which a fork child of a
    // CPU-timer session inherits. Its timers did not survive the fork, but its
    // interval did, so a child asking for a different sample rate would get the
    // parent's. Refuse rather than profile at a rate nobody asked for.
    if (want_cpu_timer && engine.debug_stats().interval_ms != requested_interval_ms) {
        return kModeAlreadyCommitted;
    }

    auto& sampler = Datadog::Sampler::get();

    // Sampler::set_interval takes FRACTIONAL SECONDS, not microseconds -- see
    // upstream stack.cpp's stack_set_interval ("Assumes the interval is given
    // in fractional seconds"). Passing microseconds here makes the sampling
    // thread sleep for hours and the stop path time out waiting for it.
    //
    // In CPU-timer mode this only sets a value the sampling loop does not read:
    // the wall walk runs on the discovery cadence instead. Set it anyway so the
    // sampler is not left holding a stale interval from a previous session.
    const double interval_s = 1.0 / static_cast<double>(sample_rate_hz);
    sampler.set_interval(interval_s);

    // echion reads the depth cap from this global rather than from Sampler.
    // src/cpu_timer.cpp has its own, lower cap (kMaxCpuTimerFrames), because
    // its frames are captured into preallocated per-thread ring slots.
    if (max_nframes > 0) {
        max_frames = max_nframes;
    }

    // Adaptive sampling varies the interval to hit a CPU-overhead target. That
    // is a sensible default for Datadog, but it makes the sample rate the user
    // asked for a suggestion rather than a setting, and it quietly trades
    // samples for overhead under load. Pin the interval instead.
    sampler.set_adaptive_sampling(false);

    if (!sampler.start()) {
        PyErr_SetString(PyExc_RuntimeError, "ddtrace stack profiler: failed to start sampling thread");
        return kSamplingThreadFailed;
    }

    // Sampler::start() is what arms the timer engine. If it declined -- because
    // this process already stopped it once, or because a timer syscall or the
    // TID table failed -- every sample would come back empty, so stop again and
    // report it rather than upload nothing under this profiler's name.
    if (want_cpu_timer && !engine.debug_stats().active) {
        sampler.stop();
        return kCpuTimerDidNotArm;
    }

    g_running = true;
    return kOk;
}

void
pyroscope_cpu_ddtrace_stop(void)
{
    std::lock_guard<std::mutex> lock(g_mutex);
    if (!g_running) {
        return;
    }
    Datadog::Sampler::get().stop();
    g_running = false;
}

// Disarm in a freshly forked child.
//
// Deliberately does NOT touch the Sampler: the pthread_atfork child handler
// registered by Sampler::one_time_setup() has already run
// stack_postfork_cleanup(), which resets echion's mutexes and maps and (through
// Sampler::postfork_child) drops the parent's CPU timer state. Calling
// Sampler::postfork_child() a second time here would redo that work on state
// the first pass already rebuilt.
//
// g_mutex is not taken: it may have been held by a thread that no longer
// exists, and only this (single) thread runs in the child.
void
pyroscope_cpu_ddtrace_postfork_child(void)
{
    g_running = false;
}

} // extern "C"
