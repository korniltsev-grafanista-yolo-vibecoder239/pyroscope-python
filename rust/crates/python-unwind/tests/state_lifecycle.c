// Real CPython states and pthread reuse, exercised by signal_sampler's tests.
#include <Python.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>

extern int pyroscope_signal_prepare(void);
static _Atomic int ready, proceed, failed, reused;
static pthread_t abandoned_id;
static PyThreadState *abandoned_state;
static PyObject *kept_globals;

static PyObject *other_evaluator(PyThreadState *state, struct _PyInterpreterFrame *frame,
                                 int throwflag)
{
    return _PyEval_EvalFrameDefault(state, frame, throwflag);
}

int pyroscope_signal_test_evaluator_conflict(void)
{
    PyInterpreterState *interp = PyInterpreterState_Main();
    _PyFrameEvalFunction saved = _PyInterpreterState_GetEvalFrameFunc(interp);
    _PyInterpreterState_SetEvalFrameFunc(interp, other_evaluator);
    int rejected = pyroscope_signal_prepare() == -1 && PyErr_Occurred() != NULL &&
        _PyInterpreterState_GetEvalFrameFunc(interp) == other_evaluator;
    PyErr_Clear();
    _PyInterpreterState_SetEvalFrameFunc(interp, saved);
    return rejected && !_PyInterpreterState_IsSpecializationEnabled(interp);
}

static void emit_n(int count)
{
    for (int i = 0; i < count; ++i) {
        if (raise(SIGPROF) != 0) {
            atomic_store(&failed, 1);
        }
    }
}

static PyObject *emit(PyObject *self, PyObject *arg)
{
    (void)self;
    long count = PyLong_AsLong(arg);
    if (count == -1 && PyErr_Occurred()) {
        return NULL;
    }
    emit_n((int)count);
    Py_RETURN_NONE;
}

static void nested_handler(int signum)
{
    (void)signum;
    // SIGPROF interrupts another native handler. SIGSEGV/SIGBUS stay unblocked.
    emit_n(17);
}

static PyObject *nested_signal(PyObject *self, PyObject *arg)
{
    (void)self;
    (void)arg;
    struct sigaction saved, action = {0};
    action.sa_handler = nested_handler;
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGUSR1, &action, &saved) != 0) {
        return PyErr_SetFromErrno(PyExc_OSError);
    }
    int result = raise(SIGUSR1);
    sigaction(SIGUSR1, &saved, NULL);
    if (result != 0) {
        return PyErr_SetFromErrno(PyExc_OSError);
    }
    Py_RETURN_NONE;
}

static PyObject *cold(PyObject *self, PyObject *arg)
{
    (void)self;
    (void)arg;
    PyThreadState *state = PyEval_SaveThread();
    atomic_store(&ready, 1);
    while (!atomic_load(&proceed)) {
        sched_yield();
    }
    // Already inside native code when the evaluator publisher was installed.
    emit_n(19);
    PyEval_RestoreThread(state);
    Py_RETURN_NONE;
}

static PyObject *abandon(PyObject *self, PyObject *arg)
{
    (void)self;
    (void)arg;
    abandoned_state = PyEval_SaveThread();
    // Real abandonment: keep the bound state and its entire live frame chain,
    // while terminating the OS thread without returning from Python evaluation.
    pthread_exit(NULL);
    return NULL;
}

static PyMethodDef methods[] = {
    {"emit", emit, METH_O, NULL},
    {"nested_signal", nested_signal, METH_NOARGS, NULL},
    {"cold", cold, METH_NOARGS, NULL},
    {"abandon", abandon, METH_NOARGS, NULL},
    {NULL, NULL, 0, NULL}
};

static void run(const char *source)
{
    PyObject *globals = PyDict_New();
    if (globals == NULL) {
        atomic_store(&failed, 1);
        return;
    }
    if (PyList_Append(kept_globals, globals) < 0) {
        atomic_store(&failed, 1);
    }
    for (PyMethodDef *method = methods; method->ml_name != NULL; ++method) {
        PyObject *function = PyCFunction_New(method, NULL);
        if (function == NULL || PyDict_SetItemString(globals, method->ml_name, function) < 0) {
            atomic_store(&failed, 1);
        }
        Py_XDECREF(function);
    }
    PyObject *code = Py_CompileString(source, "state_lifecycle.py", Py_file_input);
    if (code != NULL && PyList_Append(kept_globals, code) < 0) {
        atomic_store(&failed, 1);
    }
    PyObject *result = code == NULL ? NULL : PyEval_EvalCode(code, globals, globals);
    if (result == NULL) {
        PyErr_Print();
        atomic_store(&failed, 1);
    }
    Py_XDECREF(result);
    Py_XDECREF(code);
    Py_DECREF(globals);
}

static void *preexisting(void *unused)
{
    (void)unused;
    PyGILState_STATE gil = PyGILState_Ensure();
    // Retaining the public thread-state dict must not keep a lease valid after
    // PyGILState_Release deletes the state on this still-running native thread.
    PyObject *dict = Py_XNewRef(PyThreadState_GetDict());
    run("def entered():\n emit(23)\ndef existing():\n cold()\n entered()\nexisting()\n");
    PyGILState_Release(gil);
    emit_n(11);
    gil = PyGILState_Ensure();
    Py_XDECREF(dict);
    PyGILState_Release(gil);
    return NULL;
}

static void *abandoning(void *unused)
{
    (void)unused;
    PyGILState_Ensure();
    run("def abandoned():\n abandon()\nabandoned()\n");
    atomic_store(&failed, 1);  // Must have exited inside abandon().
    return NULL;
}

static void *replacement(void *unused)
{
    (void)unused;
    if (!pthread_equal(pthread_self(), abandoned_id)) {
        return NULL;
    }
    atomic_store(&reused, 1);
    emit_n(7);  // Same pthread identity, but never entered Python on this thread.
    PyGILState_STATE gil = PyGILState_Ensure();
    PyObject *dict = Py_XNewRef(PyThreadState_GetDict());
    run("def reentered():\n emit(29)\n nested_signal()\nreentered()\n"
        "import gc, _thread\n"
        // Keep the SDK's per-thread dictionary too, as a heap inspector might.
        // Invalidation must follow sentinel death, not dictionary/capsule death.
        "retained_private = [d for o in gc.get_objects() if type(o) is _thread._local\n"
        " for r in gc.get_referents(o) if type(r) is dict\n"
        " for d in r.values() if type(d) is dict and 'lease' in d]\n"
        "assert retained_private\n");
    PyGILState_Release(gil);
    emit_n(13);  // State deleted while this OS thread and dict remain alive.
    gil = PyGILState_Ensure();
    Py_XDECREF(dict);
    PyGILState_Release(gil);
    return NULL;
}

// Called with the GIL. No CPU timers: only the exact, deliberate real SIGPROFs.
int pyroscope_signal_test_lifecycles(void)
{
    kept_globals = PyList_New(0);
    if (kept_globals == NULL) {
        return -5;
    }
    atomic_store(&ready, 0);
    atomic_store(&proceed, 0);
    atomic_store(&failed, 0);
    atomic_store(&reused, 0);
    // Other tests may already have installed the permanent hook. Reconstruct
    // first installation with no sampling active, on an otherwise idle runtime.
    _PyInterpreterState_SetEvalFrameFunc(PyInterpreterState_Main(), _PyEval_EvalFrameDefault);
    pthread_t thread;
    if (pthread_create(&thread, NULL, preexisting, NULL) != 0) {
        return -1;
    }
    PyThreadState *main = PyEval_SaveThread();
    while (!atomic_load(&ready)) {
        sched_yield();
    }
    PyEval_RestoreThread(main);
    int result = pyroscope_signal_prepare();
    atomic_store(&proceed, 1);
    main = PyEval_SaveThread();
    pthread_join(thread, NULL);
    PyEval_RestoreThread(main);
    if (result != 0 || atomic_load(&failed)) {
        return -2;
    }
    if (pthread_create(&abandoned_id, NULL, abandoning, NULL) != 0) {
        return -3;
    }
    main = PyEval_SaveThread();
    pthread_join(abandoned_id, NULL);
    for (int i = 0; i < 128 && !atomic_load(&reused); ++i) {
        if (pthread_create(&thread, NULL, replacement, NULL) != 0) {
            atomic_store(&failed, 1);
            break;
        }
        pthread_join(thread, NULL);
    }
    PyEval_RestoreThread(main);
    // The abandoned state remained bound for every reuse probe above. Its OS
    // thread is now dead; mark the binding gone before deleting from this thread
    // (CPython's normal unbind routine would clear THIS thread's GILState TLS).
    abandoned_state->_status.bound_gilstate = 0;
    PyThreadState_Clear(abandoned_state);
    PyThreadState_Delete(abandoned_state);
    return atomic_load(&failed) ? -4 : atomic_load(&reused);
}

void pyroscope_signal_test_lifecycles_cleanup(void)
{
    Py_CLEAR(kept_globals);
}
