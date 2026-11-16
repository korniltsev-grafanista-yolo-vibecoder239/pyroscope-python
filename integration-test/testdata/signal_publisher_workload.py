"""CPU weights for native calls, preexisting threads, new threads and restarts."""

import _thread
import hashlib
import os
import threading
import time

import pyroscope

timer = os.environ["SIGNAL_TIMER"]
# Collect enough delivered process-timer ticks even on hosts where ITIMER_PROF
# systematically undercounts native CPU (also reproducible with a plain C handler).
cpu_seconds = 6 if timer == "ITimer" else 2


def native_cpu(seconds):
    deadline = time.thread_time() + seconds
    while time.thread_time() < deadline:
        # OpenSSL does the CPU work with the GIL released. The Python caller
        # must remain visible throughout, including after its evaluator returns.
        hashlib.pbkdf2_hmac("sha256", b"password", b"salt", 100_000)


def thread_work(seconds):
    native_cpu(seconds)


def preexisting_work():
    ready.set()
    go.wait()
    thread_work(cpu_seconds)


def raw_work():
    try:
        native_cpu(cpu_seconds)
    finally:
        done.set()


def configure():
    assert pyroscope.configure(
        application_name=os.environ["PYROSCOPE_APPLICATION_NAME"],
        server_address=os.environ.get("PYROSCOPE_SERVER_ADDRESS", "http://pyroscope:4040"),
        cpu_implementation=pyroscope.ProfilerImplementation.Signal,
        signal_timer=getattr(pyroscope.SignalTimer, timer),
        oncpu=True,
        sample_rate=100,
        upload_interval=1,
        line_no=pyroscope.LineNo.NoLine,
    )


scenario = os.environ["SCENARIO"]
if scenario == "preexisting":
    ready = threading.Event()
    go = threading.Event()
    worker = threading.Thread(target=preexisting_work)
    worker.start()
    assert ready.wait(10)

configure()
if scenario == "main":
    # No new Python evaluator entry around the expensive call: this specifically
    # tests the configure caller's persistent lease after configure() returns.
    deadline = time.thread_time() + cpu_seconds
    while time.thread_time() < deadline:
        hashlib.pbkdf2_hmac("sha256", b"password", b"salt", 100_000)
elif scenario == "preexisting":
    go.set()
    worker.join()
elif scenario == "new":
    worker = threading.Thread(target=thread_work, args=(cpu_seconds,))
    worker.start()
    worker.join()
elif scenario == "churn":
    for _ in range(20):
        worker = threading.Thread(target=thread_work, args=(cpu_seconds / 20,))
        worker.start()
        worker.join()
elif scenario == "raw":
    done = threading.Event()
    _thread.start_new_thread(raw_work, ())
    assert done.wait(30)
elif scenario == "restart":
    native_cpu(cpu_seconds / 2)
    assert pyroscope.shutdown()
    configure()
    native_cpu(cpu_seconds / 2)
else:
    raise AssertionError(scenario)

# Let the resolver finish its bounded drain before closing the last upload.
time.sleep(0.1)
assert pyroscope.shutdown()
