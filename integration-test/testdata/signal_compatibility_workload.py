"""Equal CPU budgets in Python and in C with a detached Python thread state."""

import _thread
import hashlib
import os
import sys
import threading
import time

import pyroscope

ready = threading.Event()
start = threading.Event()
errors = []


def python_leaf():
    end = time.thread_time() + 0.1
    value = 0
    while time.thread_time() < end:
        for _ in range(10000):
            value = (value * 1664525 + 1013904223) & 0xFFFFFFFF
    return value


def native_leaf():
    end = time.thread_time() + 0.1
    while time.thread_time() < end:
        # _hashlib releases the GIL for the entire expensive call. A lookup
        # of only the attached tstate would lose nearly all of this weight.
        hashlib.pbkdf2_hmac("sha256", b"password", b"salt", 10000)


def before():
    ready.set()
    start.wait()
    for _ in range(30):
        python_leaf()
        native_leaf()


def after():
    for _ in range(30):
        python_leaf()
        native_leaf()


def raw_worker(done):
    try:
        assert threading.get_ident() not in threading._active
        for _ in range(30):
            python_leaf()
            native_leaf()
    except BaseException as error:
        errors.append(error)
    finally:
        done.release()


def main():
    mode = os.environ.get("MODE", "threading")
    print(sys.version, "JIT:", getattr(sys, "_jit", None), flush=True)
    if hasattr(sys, "_jit"):
        print("JIT enabled:", sys._jit.is_enabled(), flush=True)
        if os.environ.get("PYTHON_JIT") == "1":
            assert sys._jit.is_enabled(), "requested JIT test needs a JIT build"
    if mode == "threading":
        # Register one thread which already exists, and one created after
        # configure. Use separate roots so a missing worker cannot pass.
        old = threading.Thread(target=before)
        old.start()
        assert ready.wait(10)
    assert pyroscope.configure(
        application_name=os.environ["PYROSCOPE_APPLICATION_NAME"],
        server_address=os.environ["PYROSCOPE_SERVER_ADDRESS"],
        cpu_implementation=pyroscope.ProfilerImplementation.Signal,
        signal_timer=getattr(pyroscope.SignalTimer, os.environ["SIGNAL_TIMER"]),
        sample_rate=100,
        upload_interval=1,
        enable_logging=True,
    )
    if mode == "threading":
        new = threading.Thread(target=after)
        start.set()
        old.join()
        # Keep one CPU-bound worker at a time: ITimer notifications coalesce
        # when several cores expire the process timer concurrently. The
        # existing configuration tests exercise simultaneous workers.
        # Exiting and recreating a thread also exercises OS thread-ID reuse.
        new.start()
        new.join()
    else:
        assert mode == "raw"
        done = _thread.allocate_lock()
        done.acquire()
        _thread.start_new_thread(raw_worker, (done,))
        assert done.acquire(timeout=30)
        assert not errors, errors
    # Drain the last samples and upload the complete finite workload.
    time.sleep(3)
    assert pyroscope.shutdown()


if __name__ == "__main__":
    main()
