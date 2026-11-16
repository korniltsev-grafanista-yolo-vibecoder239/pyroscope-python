import ctypes
import hashlib
import os
import threading
import time

import pyroscope


def native_work(seconds=6):
    # Large updates release the GIL. Using the attached thread state instead of
    # the bound state would lose nearly all of this CPU time on Python 3.15.
    data = b"x" * (256 * 1024)
    deadline = time.thread_time() + seconds
    while time.thread_time() < deadline:
        hashlib.sha256(data).digest()


def native_callback(_):
    native_work(0.3)


def main():
    scenario = os.environ["SCENARIO"]
    timer = getattr(pyroscope.SignalTimer, os.environ["SIGNAL_TIMER"])
    ready = threading.Event()
    release = threading.Event()

    def preexisting():
        ready.set()
        release.wait()
        native_work()

    # Verify a state that was already live when the profiler initialized.
    if scenario == "preexisting":
        worker = threading.Thread(target=preexisting)
        worker.start()
        assert ready.wait(10)

    assert pyroscope.configure(
        application_name=os.environ["PYROSCOPE_APPLICATION_NAME"],
        server_address=os.environ["PYROSCOPE_SERVER_ADDRESS"],
        cpu_implementation=pyroscope.ProfilerImplementation.Signal,
        signal_timer=timer,
        sample_rate=100,
        upload_interval=1,
    )
    try:
        if scenario == "main":
            native_work()
        elif scenario == "preexisting":
            release.set()
            worker.join(30)
            assert not worker.is_alive()
        elif scenario == "foreign":
            # ctypes attaches a temporary Python state in this raw pthread.
            # No threading hooks run, so only the process timer covers it.
            assert timer == pyroscope.SignalTimer.ITimer
            libc = ctypes.CDLL(None)
            callback_type = ctypes.CFUNCTYPE(ctypes.c_void_p, ctypes.c_void_p)
            callback = callback_type(native_callback)
            libc.pthread_create.argtypes = [
                ctypes.POINTER(ctypes.c_void_p),
                ctypes.c_void_p,
                callback_type,
                ctypes.c_void_p,
            ]
            libc.pthread_join.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
            # Repeated creation/deletion tests temporary states and address/ID
            # reuse. The accumulated work is still 6 thread-CPU seconds.
            for _ in range(20):
                thread = ctypes.c_void_p()
                assert (
                    libc.pthread_create(ctypes.byref(thread), None, callback, None) == 0
                )
                assert libc.pthread_join(thread, None) == 0
        else:
            raise AssertionError(scenario)
        # Let the resolver drain the last records and give the 1-second upload
        # loop two windows to publish the finite workload before shutdown.
        time.sleep(2)
    finally:
        assert pyroscope.shutdown()


if __name__ == "__main__":
    main()
