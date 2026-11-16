"""Equal CPU budgets: preexisting/new threads, Python/native leaves, two sessions."""

import hashlib
import os
import threading
import time

import pyroscope

CPU_SECONDS = int(os.environ["CPU_SECONDS"])
# Process-wide ITIMER_PROF can favor one of several simultaneously running
# threads. Use one allowed CPU for its equal-budget shape assertion. CTimer
# retains concurrent native execution across CPUs and measures each separately.
if os.environ["TIMER"] == "ITimer":
    os.sched_setaffinity(0, {min(os.sched_getaffinity(0))})


def python_leaf():
    end = time.thread_time() + CPU_SECONDS
    value = 0
    while time.thread_time() < end:
        value = (value * 1664525 + 1013904223) & 0xFFFFFFFF


def native_leaf():
    end = time.thread_time() + CPU_SECONDS
    while time.thread_time() < end:
        # OpenSSL's CPU loop releases the GIL; Python remains in native_leaf.
        hashlib.pbkdf2_hmac("sha256", b"password", b"salt", 100_000)


def preexisting_worker(ready, start):
    ready.set()
    assert start.wait(30)
    python_leaf()
    native_leaf()


def new_worker():
    python_leaf()
    native_leaf()


for session in range(2):
    ready, start = threading.Event(), threading.Event()
    old = threading.Thread(target=preexisting_worker, args=(ready, start))
    old.start()
    assert ready.wait(30)
    assert pyroscope.configure(
        application_name=os.environ["PYROSCOPE_APPLICATION_NAME"],
        server_address=os.environ["PYROSCOPE_SERVER_ADDRESS"],
        cpu_implementation=pyroscope.ProfilerImplementation.Signal,
        signal_timer=getattr(pyroscope.SignalTimer, os.environ["TIMER"]),
        upload_interval=1,
        sample_rate=100,
        tags={"session": str(session)},
    )
    new = threading.Thread(target=new_worker)
    new.start()
    start.set()
    old.join(60)
    new.join(60)
    assert not old.is_alive() and not new.is_alive()
    time.sleep(2)
    assert pyroscope.shutdown()
