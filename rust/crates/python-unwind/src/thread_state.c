// CPython 3.15: publish thread states during ordinary execution, never by
// inspecting CPython's compiler TLS or searching its thread list in SIGPROF.
#include <Python.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>

#if defined(Py_GIL_DISABLED) || defined(Py_STACKREF_DEBUG)
#error "Signal requires the GIL and pointer-valued stack references"
#endif
_Static_assert(ATOMIC_POINTER_LOCK_FREE == 2, "signal-safe pointer atomics");
_Static_assert(ATOMIC_INT_LOCK_FREE == 2, "signal-safe counter atomics");

typedef struct {
    _Atomic(PyThreadState *) state;
    _Atomic unsigned readers;
    _Atomic unsigned owners;
} Lease;

// The key and evaluator survive stop/restart. No key recycling, pthread ID
// matching, architecture-specific TLS offsets, or lazy TLS access in handlers.
static pthread_key_t key;
static int key_created;  // Only accessed with the GIL, before arming a timer.
static PyObject *locals;
static Lease registering;  // Recursion guard while registration can run GC.
static const char capsule_name[] = "pyroscope.signal.thread_state";

static void release_owner(void *value)
{
    Lease *lease = value;
    if (lease != NULL && lease != &registering &&
        atomic_fetch_sub(&lease->owners, 1) == 1) {
        free(lease);
    }
}

static void invalidate_lease(Lease *lease)
{
    // Sequential consistency orders reader admission against invalidation:
    // a reader either sees NULL, or teardown waits for its bounded walk.
    // Only ordinary-thread teardown waits; SIGPROF never waits.
    atomic_store(&lease->state, NULL);
    while (atomic_load(&lease->readers) != 0) {
        atomic_signal_fence(memory_order_seq_cst);
    }
}

static void invalidate(PyObject *capsule)
{
    Lease *lease = PyCapsule_GetPointer(capsule, capsule_name);
    invalidate_lease(lease);
    release_owner(lease);
}

static PyObject *state_cleared(PyObject *capsule, PyObject *weakref)
{
    (void)weakref;
    // Invalidate on sentinel death even if a debugger retains our dictionary,
    // weakref, callback or capsule. Capsule destruction alone is insufficient.
    invalidate_lease(PyCapsule_GetPointer(capsule, capsule_name));
    Py_RETURN_NONE;
}

static PyMethodDef state_cleared_def = {"state_cleared", state_cleared, METH_O, NULL};

// Requires the GIL and the current, attached state. The private _thread._local
// initializes CPython's thread-local sentinel and owns its weakref callback.
// Sentinel death invalidates the lease BEFORE tstate storage is freed, without
// depending on the lifetime of either the public or private dictionaries.
static int publish(PyThreadState *state)
{
    Lease *old = pthread_getspecific(key);
    if (old == &registering ||
        (old != NULL && atomic_load(&old->state) == state)) {
        return 0;
    }
    if (pthread_setspecific(key, &registering) != 0) {
        PyErr_NoMemory();
        return -1;
    }
    Lease *lease = calloc(1, sizeof(*lease));
    if (lease == NULL) {
        PyErr_NoMemory();
        goto fail;
    }
    atomic_init(&lease->state, state);
    atomic_init(&lease->readers, 0);
    atomic_init(&lease->owners, 1);  // capsule ownership
    PyObject *capsule = PyCapsule_New(lease, capsule_name, invalidate);
    if (capsule == NULL) {
        free(lease);
        goto fail;
    }
    // Force creation of this state's local sentinel using the original builtin
    // type. This can run GC; the registering key prevents recursive publication.
    PyObject *dict = PyObject_GetAttrString(locals, "__dict__");
    if (dict == NULL) {
        Py_DECREF(capsule);
        goto fail;
    }
    PyObject *callback = PyCFunction_New(&state_cleared_def, capsule);
    PyObject *weakref = callback == NULL ? NULL :
        PyWeakref_NewRef(state->threading_local_sentinel, callback);
    Py_XDECREF(callback);
    int result = weakref == NULL ? -1 : PyDict_SetItemString(dict, "lease", weakref);
    Py_XDECREF(weakref);
    Py_DECREF(dict);
    if (result < 0) {
        Py_DECREF(capsule);
        goto fail;
    }
    atomic_fetch_add(&lease->owners, 1);  // pthread-key ownership
    // The key's storage was already allocated by the recursion guard above.
    if (pthread_setspecific(key, lease) != 0) {
        release_owner(lease);
        Py_DECREF(capsule);
        PyErr_NoMemory();
        goto fail;
    }
    // Retain the capsule until the key has acquired its independent ownership.
    Py_DECREF(capsule);
    release_owner(old);
    return 0;
fail:
    pthread_setspecific(key, NULL);
    release_owner(old);
    return -1;
}

static PyObject *evaluate(PyThreadState *state, struct _PyInterpreterFrame *frame,
                          int throwflag)
{
    // Clear may execute Python finalizers. Never recreate a local sentinel or
    // republish a state whose destruction has begun.
    if (!state->_status.finalizing) {
        Lease *lease = pthread_getspecific(key);
        if (lease != &registering &&
            (lease == NULL || atomic_load(&lease->state) != state)) {
            PyObject *raised = PyErr_GetRaisedException();
            if (publish(state) < 0) {
                // Registration failure loses samples, not application execution.
                // The next evaluator entry retries. Preserve generator.throw().
                PyErr_Clear();
            }
            PyErr_SetRaisedException(raised);
        }
    }
    return _PyEval_EvalFrameDefault(state, frame, throwflag);
}

// Called with the GIL on the configuring thread before installing SIGPROF.
// Failure sets a Python exception. Refuse other PEP 523 evaluators, rather than
// assume their frame/lifetime rules match the default evaluator's.
int pyroscope_signal_prepare(void)
{
    PyThreadState *state = PyThreadState_Get();
    PyInterpreterState *interp = PyThreadState_GetInterpreter(state);
    if (interp != PyInterpreterState_Main() || state->_status.finalizing) {
        PyErr_SetString(PyExc_RuntimeError, "Signal requires the live main interpreter");
        return -1;
    }
    _PyFrameEvalFunction current = _PyInterpreterState_GetEvalFrameFunc(interp);
    if (current != _PyEval_EvalFrameDefault && current != evaluate) {
        PyErr_SetString(PyExc_RuntimeError, "Signal on 3.15 requires the default frame evaluator");
        return -1;
    }
    if (!key_created) {
        if (pthread_key_create(&key, release_owner) != 0) {
            PyErr_SetString(PyExc_RuntimeError, "cannot allocate Signal pthread key");
            return -1;
        }
        key_created = 1;
        // Resolve pthread_getspecific outside any signal handler (also on dyld).
        (void)pthread_getspecific(key);
    }
    if (locals == NULL) {
        PyObject *module = PyImport_ImportModule("_thread");
        if (module == NULL) {
            return -1;
        }
        PyObject *type = PyObject_GetAttrString(module, "_local");
        if (type == NULL) {
            Py_DECREF(module);
            return -1;
        }
        // A monkeypatched/subclassed local can have different cleanup semantics
        // or discard attributes. Only CPython's immutable, module-owned type
        // establishes the sentinel lifetime used by this implementation.
        int builtin = PyType_Check(type) &&
            (((PyTypeObject *)type)->tp_flags & Py_TPFLAGS_IMMUTABLETYPE) &&
            strcmp(((PyTypeObject *)type)->tp_name, "_thread._local") == 0 &&
            PyType_GetModule((PyTypeObject *)type) == module;
        Py_DECREF(module);
        if (!builtin) {
            Py_DECREF(type);
            PyErr_SetString(PyExc_RuntimeError,
                            "Signal requires CPython's unmodified _thread._local type");
            return -1;
        }
        locals = PyObject_CallNoArgs(type);
        Py_DECREF(type);
        if (locals == NULL) {
            return -1;
        }
    }
    if (publish(state) < 0) {
        return -1;
    }
    _PyInterpreterState_SetEvalFrameFunc(interp, evaluate);
    return 0;
}

// Signal path: fixed work, no allocation, Python APIs, locks, loader or syscalls.
// The key owns a reference until this OS thread exits or replaces its own key.
// Neither can happen while this thread is interrupted in the handler.
PyThreadState *pyroscope_signal_acquire(void **cookie)
{
    Lease *lease = pthread_getspecific(key);
    *cookie = NULL;
    if (lease == NULL || lease == &registering) {
        return NULL;
    }
    atomic_fetch_add(&lease->readers, 1);
    PyThreadState *state = atomic_load(&lease->state);
    if (state == NULL) {
        atomic_fetch_sub(&lease->readers, 1);
    } else {
        *cookie = lease;
    }
    return state;
}

void pyroscope_signal_release(void *cookie)
{
    if (cookie != NULL) {
        Lease *lease = cookie;
        atomic_fetch_sub(&lease->readers, 1);
    }
}
