"""Fork behaviour for the vendored CPU profilers.

Covers both modes of cpp/cpu/ddtrace_stack/: CpuProfiler.Ddtrace and
CpuProfiler.DdtraceCpuTimer.

Intentionally not run in CI alongside the other suites; fork tests can deadlock
on unrelated library state. Run manually:

    python scripts/tests/test_ddtrace_cpu_fork.py                  # both modes
    python scripts/tests/test_ddtrace_cpu_fork.py ddtrace-cpu-timer

The thing being checked is that a forked child does NOT keep sampling. Upstream
dd-trace-py restarts the sampler from its pthread_atfork child handler; that is
patched out (see cpp/cpu/ddtrace_stack/VENDOR.md, "do not restart the sampler in
the fork child") because Pyroscope leaks the agent in the child, so a restarted
sampler would push into a sink nothing ever drains.

The child proves this by burning CPU and then configuring a fresh agent of its
own, which only succeeds if the inherited state was properly torn down. For the
CPU timer that also covers the fork half of Engine::postfork_child(): the
parent's per-thread timers do not survive a fork (POSIX timers are not
inherited) and its CaptureStates are stale, so the child has to drop the lot and
rebuild the signal-handler control plane before it can arm anything.
"""

import hashlib
import os
import subprocess
import sys
import threading
import time

import pyroscope

APP_NAME = "pyroscope.ddtrace-cpu-fork-test"

# name -> (CpuProfiler value, minimum CPython)
PROFILERS = {
    "ddtrace": (pyroscope.CpuProfiler.Ddtrace, (3, 11)),
    "ddtrace-cpu-timer": (pyroscope.CpuProfiler.DdtraceCpuTimer, (3, 12)),
}


def burn(seconds):
    value = "seed"
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        value = hashlib.sha256(value.encode()).hexdigest()
    return value


def child_body(profiler):
    """Runs in the forked child. Returns an exit code."""
    # If the sampler had been restarted here it would be walking threads and
    # pushing samples while we do this.
    burn(1.0)

    # A child that inherited a half-torn-down agent cannot start a new one.
    if not pyroscope.configure(
        application_name=APP_NAME + ".child",
        server_address="http://127.0.0.1:4040",
        cpu_profiler=profiler,
        mem_enabled=False,
    ):
        return 2
    burn(1.0)
    if not pyroscope.shutdown():
        return 3
    return 0


def fork_and_check(profiler):
    pid = os.fork()
    if pid == 0:
        code = 1
        try:
            code = child_body(profiler)
        except BaseException:
            code = 4
        finally:
            os._exit(code)

    _, status = os.waitpid(pid, 0)
    code = os.waitstatus_to_exitcode(status)
    if code != 0:
        raise AssertionError(
            "forked child exited with %d; the ddtrace sampler probably survived "
            "the fork or left state the child could not recover from" % code
        )


def run_one(name):
    profiler, minimum = PROFILERS[name]
    if sys.platform != "linux" or sys.version_info < minimum:
        print("skipped: cpu_profiler=%s requires Linux and CPython %d.%d+" % (name, *minimum))
        return

    if not pyroscope.configure(
        application_name=APP_NAME,
        server_address="http://127.0.0.1:4040",
        cpu_profiler=profiler,
        upload_interval=1,
        mem_enabled=False,
    ):
        raise AssertionError("failed to start the agent with cpu_profiler=%s" % name)

    try:
        # Fork while several threads are busy, so the child inherits a
        # thread_info_map full of threads that do not exist in it.
        stop = threading.Event()
        threads = [
            threading.Thread(target=lambda: [burn(0.1) for _ in iter(lambda: not stop.is_set(), False)])
            for _ in range(3)
        ]
        for t in threads:
            t.start()
        time.sleep(1.0)

        fork_and_check(profiler)

        stop.set()
        for t in threads:
            t.join()

        # The parent must still be profiling: its sampling thread survives the
        # fork untouched (Sampler::prefork does not stop it).
        burn(1.0)
    finally:
        pyroscope.shutdown()

    print("ok: %s" % name)


def main():
    names = sys.argv[1:] or list(PROFILERS)
    for name in names:
        if name not in PROFILERS:
            raise SystemExit("unknown profiler %r; expected one of %s" % (name, list(PROFILERS)))

    if len(names) == 1:
        run_one(names[0])
        return

    # One process per mode: the CPU accounting mode is frozen for the life of
    # the process, so the second one here could not start.
    failures = []
    for name in names:
        print("################ cpu_profiler=%s ################" % name, flush=True)
        if subprocess.call([sys.executable, os.path.abspath(__file__), name]) != 0:
            failures.append(name)
    if failures:
        raise SystemExit("FAILED: %s" % ", ".join(failures))


if __name__ == "__main__":
    main()
