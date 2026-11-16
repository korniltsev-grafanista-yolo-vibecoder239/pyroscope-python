"""Equal thread CPU budgets in Python and in native code that releases the GIL."""

import hashlib
import os
import threading
import time

import pyroscope

ready = threading.Event()
start = threading.Event()


def native_work():
    # This thread exists before configure(), exercising CTimer's registration
    # of existing threads as well as detached thread-state lookup.
    ready.set()
    if not start.wait(30):
        raise AssertionError("profiler did not start")
    end = time.thread_time() + 6
    while time.thread_time() < end:
        # CPython releases the GIL for the entire OpenSSL PBKDF2 computation.
        hashlib.pbkdf2_hmac("sha256", b"password", b"salt", 200_000)


def python_work():
    end = time.thread_time() + 6
    value = 1
    while time.thread_time() < end:
        for _ in range(1000):
            value = (value * 1664525 + 1013904223) & 0xFFFFFFFF


def main():
    native = threading.Thread(target=native_work)
    native.start()
    assert ready.wait(30)
    try:
        assert pyroscope.configure(
            application_name=os.environ["PYROSCOPE_APPLICATION_NAME"],
            server_address=os.environ["PYROSCOPE_SERVER_ADDRESS"],
            cpu_implementation=pyroscope.ProfilerImplementation.Signal,
            signal_timer=getattr(pyroscope.SignalTimer, os.environ["SIGNAL_TIMER"]),
            sample_rate=100,
            upload_interval=1,
            enable_logging=True,
        )
        start.set()
        if os.environ["SIGNAL_TIMER"] == "ITimer":
            # A process ITimer coalesces simultaneous expirations from two CPU
            # workers. Run its phases sequentially so total weight also checks
            # CPU accounting. CTimer exercises both workers concurrently.
            native.join(45)
            assert not native.is_alive()
        python = threading.Thread(target=python_work)
        python.start()
        python.join(45)
        native.join(45)
        assert not python.is_alive() and not native.is_alive()
        time.sleep(2)  # Drain the final delivery before shutdown.
        assert pyroscope.shutdown()
    finally:
        start.set()
        native.join(45)


if __name__ == "__main__":
    main()
