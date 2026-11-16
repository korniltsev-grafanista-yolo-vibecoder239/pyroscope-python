"""Equal CPU budgets across old/new Python threads and GIL-releasing native work."""

import _thread
import ctypes
import hashlib
import os
import threading
import time

import pyroscope


def native_leaf():
    # Each batch runs outside the GIL, without Python callbacks until it returns.
    end = time.thread_time() + 4
    while time.thread_time() < end:
        hashlib.pbkdf2_hmac("sha256", b"password", b"salt", 400_000)


def python_leaf():
    end = time.thread_time() + 4
    value = 0
    while time.thread_time() < end:
        for _ in range(1000):
            value = (value * 1664525 + 1013904223) & 0xFFFFFFFF
    return value


def preexisting():
    ready.set()
    release.wait()
    native_leaf()


def new_thread():
    python_leaf()


def raw_thread():
    native_leaf()
    complete.set()


@ctypes.CFUNCTYPE(ctypes.c_void_p, ctypes.c_void_p)
def foreign_thread(_):
    native_leaf()
    return None


def main():
    timer = os.environ["SIGNAL_TIMER"]
    scenario = os.environ["SCENARIO"]
    # ITimer uses coarse CPU accounting; keep the sequential cohorts on one CPU
    # so migration/CPU-specific tick phases do not bias their equal CPU budgets.
    # Choose among allowed CPUs to spread simultaneous integration processes.
    cpus = sorted(os.sched_getaffinity(0))
    cpu = cpus[time.monotonic_ns() % len(cpus)]
    os.sched_setaffinity(0, {cpu})
    print("sampling cohorts on CPU", cpu, flush=True)
    old = threading.Thread(target=preexisting)
    old.start()
    assert ready.wait(10)
    assert pyroscope.configure(
        application_name=os.environ["PYROSCOPE_APPLICATION_NAME"],
        server_address=os.environ["PYROSCOPE_SERVER_ADDRESS"],
        cpu_implementation=pyroscope.ProfilerImplementation.Signal,
        signal_timer=getattr(pyroscope.SignalTimer, timer),
        sample_rate=100,
        line_no=pyroscope.LineNo.NoLine,
        upload_interval=1,
        tags={"canary": os.environ["CANARY"]},
    )
    try:
        # Sequential budgets avoid process-timer bias between simultaneous workers.
        release.set()
        old.join()
        new = threading.Thread(target=new_thread)
        new.start()
        new.join()
        if scenario == "foreign":
            _thread.start_new_thread(raw_thread, ())
            assert complete.wait(20)
            libc = ctypes.CDLL(None)
            thread = ctypes.c_ulong()
            libc.pthread_create.argtypes = [
                ctypes.POINTER(ctypes.c_ulong),
                ctypes.c_void_p,
                type(foreign_thread),
                ctypes.c_void_p,
            ]
            libc.pthread_join.argtypes = [ctypes.c_ulong, ctypes.c_void_p]
            assert (
                libc.pthread_create(ctypes.byref(thread), None, foreign_thread, None)
                == 0
            )
            assert libc.pthread_join(thread, None) == 0
        # Keep the last upload window alive; shutdown doesn't promise a final drain.
        time.sleep(2)
    finally:
        release.set()
        old.join()
        assert pyroscope.shutdown()


ready = threading.Event()
release = threading.Event()
complete = threading.Event()
if __name__ == "__main__":
    main()
