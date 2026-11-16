// CPython 3.15: publish tstates from normal evaluation into SDK-owned pthread TSD.
// No CPython/compiler TLS, interpreter lists or thread IDs are read in SIGPROF.
#include <Python.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <unistd.h>

#ifdef Py_STACKREF_DEBUG
#error "Signal does not support indexed Py_STACKREF_DEBUG references"
#endif

_Static_assert(ATOMIC_POINTER_LOCK_FREE == 2 && ATOMIC_INT_LOCK_FREE == 2 &&
               ATOMIC_LONG_LOCK_FREE == 2,
               "Signal requires lock-free pointer and integer atomics");

typedef struct {
    _Atomic(PyThreadState *) state;
    atomic_uint readers;
    atomic_uint owners; // one capsule reference and one per installed TSD value
    pid_t process; // normal-thread lifecycle bookkeeping, never read in SIGPROF
} Mirror;

static const char capsule_name[] = "pyroscope.signal.eval_mirror";
static _Atomic(pthread_key_t) mirror_key;
static int key_created; // GIL protected; the key is never deleted/recycled
static PyObject *local_type; // GIL protected; retained while the hook is installed
static pid_t process_id; // refreshed by installation after a shutdown-before-fork

static void release_mirror(void *ptr)
{
    Mirror *mirror = ptr;
    if (mirror && atomic_fetch_sub(&mirror->owners, 1) == 1) {
        free(mirror);
    }
}

static void invalidate(Mirror *mirror)
{
    // SC ordering pairs the reader increment/load with this store/load: either
    // the reader sees NULL or teardown sees the reader. Only teardown may wait.
    atomic_store(&mirror->state, NULL);
    if (mirror->process != getpid()) {
        // After fork, other native threads and their reader pins/TSD owners do
        // not exist in the child. Clear stale owners, retaining the capsule and
        // (only for the surviving thread) its TSD reference. The parent is COW.
        atomic_store(&mirror->owners,
                     pthread_getspecific(mirror_key) == mirror ? 2 : 1);
        return;
    }
    while (atomic_load(&mirror->readers)) {
        sched_yield();
    }
}

static void destroy_capsule(PyObject *capsule)
{
    Mirror *mirror = PyCapsule_GetPointer(capsule, capsule_name);
    invalidate(mirror); // also handles premature removal from the dictionary
    release_mirror(mirror);
}

static PyObject *state_cleared(PyObject *capsule, PyObject *weakref)
{
    (void)weakref;
    invalidate(PyCapsule_GetPointer(capsule, capsule_name));
    Py_RETURN_NONE;
}

static PyMethodDef on_clear = {
    "_signal_state_cleared", state_cleared, METH_O, NULL
};

static int replace_mirror(Mirror *mirror)
{
    Mirror *old = pthread_getspecific(mirror_key);
    if (mirror == old) {
        return 0;
    }
    if (mirror) {
        atomic_fetch_add(&mirror->owners, 1);
    }
    if (pthread_setspecific(mirror_key, mirror) != 0) {
        release_mirror(mirror);
        return -1;
    }
    // A signal before the TSD store sees old, after it sees mirror. Both are
    // retained across the store. The pthread destructor runs after TSD is NULL.
    release_mirror(old);
    return 0;
}

static int remember_state(PyThreadState *state)
{
    Mirror *mirror = pthread_getspecific(mirror_key);
    if (state->_status.finalizing) {
        // Finalizers may execute Python during PyThreadState_Clear. Never put
        // another capsule in a dictionary which CPython has already cleared.
        return replace_mirror(NULL);
    }
    if (mirror && atomic_load(&mirror->state) == state) {
        if (mirror->process != process_id) {
            // Sampling was stopped before fork. Installation precedes every
            // new walk in the child; inherited counters cannot represent readers.
            atomic_store(&mirror->readers, 0);
            mirror->process = process_id;
        }
        return 0;
    }
    // Clear the old state's slot before any fallible work. A failed registration
    // must drop samples, never keep profiling a different/deleted state.
    if (replace_mirror(NULL) != 0) {
        Py_FatalError("cannot clear Signal's pthread slot");
    }
    PyObject *dict = PyThreadState_GetDict();
    if (!dict) {
        return -1;
    }
    PyObject *entry = PyDict_GetItemString(dict, capsule_name);
    if (entry) {
        if (!PyTuple_Check(entry) || PyTuple_GET_SIZE(entry) != 2) {
            return -1;
        }
        PyObject *capsule = PyTuple_GET_ITEM(entry, 0);
        mirror = PyCapsule_GetPointer(capsule, capsule_name);
        if (!mirror || atomic_load(&mirror->state) != state) {
            return -1;
        }
        return replace_mirror(mirror);
    }
    // CPython's *sentinel*, unlike PyThreadState_GetDict(), must have exactly
    // one strong owner: the thread state. PyThreadState_Clear destroys it before
    // clearing the dictionary. Observe it weakly so a native extension retaining
    // the entire thread dictionary cannot extend our binding's validity.
    // See CPython 3.15 Modules/_threadmodule.c: create_localdummies/Quick overview.
    // Constructing a plain built-in local initializes the two localdummies; its
    // destruction leaves the tstate-owned sentinel in place. No private writes.
    if (!state->threading_local_sentinel) {
        PyObject *local = PyObject_CallNoArgs(local_type);
        if (!local) {
            return -1;
        }
        Py_DECREF(local);
        if (!state->threading_local_sentinel) {
            return -1;
        }
    }
    mirror = malloc(sizeof(*mirror));
    if (!mirror) {
        return -1;
    }
    atomic_init(&mirror->state, state);
    atomic_init(&mirror->readers, 0);
    atomic_init(&mirror->owners, 1);
    mirror->process = process_id;
    PyObject *capsule = PyCapsule_New(mirror, capsule_name, destroy_capsule);
    if (!capsule) {
        release_mirror(mirror);
        return -1;
    }
    PyObject *callback = PyCFunction_NewEx(&on_clear, capsule, NULL);
    PyObject *watch = callback
        ? PyWeakref_NewRef(state->threading_local_sentinel, callback) : NULL;
    Py_XDECREF(callback);
    entry = watch ? PyTuple_Pack(2, capsule, watch) : NULL;
    Py_XDECREF(watch);
    int result = entry ? PyDict_SetItemString(dict, capsule_name, entry) : -1;
    Py_XDECREF(entry);
    if (result == 0) {
        result = replace_mirror(mirror);
    }
    Py_DECREF(capsule);
    return result;
}

static PyObject *evaluate(PyThreadState *state, struct _PyInterpreterFrame *frame,
                          int throwflag)
{
    // Registration is ordinary GIL-held Python execution, never signal work.
    // Preserve the exception being injected into a generator/coroutine. On OOM,
    // keep executing Python with an empty slot (and hence dropped samples).
    PyObject *exception = PyErr_GetRaisedException();
    if (remember_state(state) != 0) {
        PyErr_Clear();
    }
    PyErr_SetRaisedException(exception);
    return _PyEval_EvalFrameDefault(state, frame, throwflag);
}

// Called with the GIL on a normal thread before installing/arming SIGPROF.
const char *pyroscope_mirror_install(void)
{
    PyThreadState *state = PyThreadState_Get();
    PyInterpreterState *interp = PyThreadState_GetInterpreter(state);
    if (interp != PyInterpreterState_Main()) {
        return "Signal's evaluation mirror supports only the main interpreter";
    }
    _PyFrameEvalFunction previous = _PyInterpreterState_GetEvalFrameFunc(interp);
    if (previous != _PyEval_EvalFrameDefault) {
        return "Signal on Python 3.15 requires the default frame evaluator (PEP 523 conflict)";
    }
    process_id = getpid();
    if (!key_created) {
        pthread_key_t key;
        if (pthread_key_create(&key, release_mirror) != 0) {
            return "cannot allocate Signal's pthread key";
        }
        atomic_store(&mirror_key, key);
        key_created = 1;
    }
    PyObject *thread = PyImport_ImportModule("_thread");
    if (!thread) {
        PyErr_Clear();
        return "cannot import _thread for Signal state lifetime tracking";
    }
    local_type = PyObject_GetAttrString(thread, "_local");
    Py_DECREF(thread);
    if (!local_type || !PyType_Check(local_type)) {
        Py_CLEAR(local_type);
        PyErr_Clear();
        return "cannot find the built-in _thread._local type";
    }
    // Also binds pthread_getspecific on platforms with lazy native symbol binding.
    // Seed the configure caller even when its current evaluation predates us.
    if (remember_state(state) != 0) {
        Py_CLEAR(local_type);
        PyErr_Clear();
        return "cannot register Signal's configuring thread";
    }
    _PyInterpreterState_SetEvalFrameFunc(interp, evaluate);
    return NULL;
}

void pyroscope_mirror_uninstall(void)
{
    PyInterpreterState *interp = PyInterpreterState_Main();
    // Do not overwrite a later evaluator installed by another tool. Replacing
    // ours during sampling is unsupported; a subsequent install rejects it.
    if (_PyInterpreterState_GetEvalFrameFunc(interp) == evaluate) {
        _PyInterpreterState_SetEvalFrameFunc(interp, _PyEval_EvalFrameDefault);
    }
    Py_CLEAR(local_type);
    // Keep the key and live capsules: in-flight evaluations and restarted
    // sessions may still use them. Their owners release them at state/thread exit.
}

// Signal path: the TSD owner keeps the Mirror alive until this thread can resume.
// Successful installation is a caller precondition, not checked here.
PyThreadState *pyroscope_mirror_acquire(void **guard)
{
    Mirror *mirror = pthread_getspecific(mirror_key);
    *guard = mirror;
    if (!mirror) {
        return NULL;
    }
    atomic_fetch_add(&mirror->readers, 1);
    return atomic_load(&mirror->state);
}

void pyroscope_mirror_release(void *guard)
{
    Mirror *mirror = guard;
    if (mirror) {
        atomic_fetch_sub(&mirror->readers, 1);
    }
}
