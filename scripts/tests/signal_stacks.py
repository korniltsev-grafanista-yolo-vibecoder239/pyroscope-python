"""Finite CPU budgets for the Rust tests of real SIGPROF sampling."""

import hashlib
import time


def python_inner():  # first: python_inner
    value = 0
    for _ in range(10000):
        value = (value * 1664525 + 1013904223) & 0xFFFFFFFF
    return value


def python_outer():  # first: python_outer
    return python_inner()


def python_leaf():  # first: python_leaf
    end = time.thread_time() + 2
    while time.thread_time() < end:
        python_outer()


def native_leaf():  # first: native_leaf
    end = time.thread_time() + 2
    while time.thread_time() < end:
        hashlib.pbkdf2_hmac("sha256", b"password", b"salt", 50000)


def entry():  # first: entry
    python_leaf()
    native_leaf()
