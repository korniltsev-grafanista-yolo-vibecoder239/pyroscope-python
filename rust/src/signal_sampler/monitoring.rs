//! Maintain the 3.15 shadow binding using an independent sys.monitoring tool.

use pyo3::{exceptions::PyRuntimeError, prelude::*, wrap_pyfunction};
use pyroscope_python_unwind::bindings;

const ENTRY_EVENTS: [&str; 3] = ["PY_START", "PY_RESUME", "LINE"];
const EXIT_EVENTS: [&str; 3] = ["PY_RETURN", "PY_YIELD", "PY_UNWIND"];

#[pyfunction]
fn bind_thread(_code: &Bound<'_, PyAny>, _offset: &Bound<'_, PyAny>) -> PyResult<()> {
    // Monitoring calls us with an attached state and the GIL, outside SIGPROF.
    // Do not interpret unused offsets/line numbers or depend on their integer range.
    unsafe { bindings::bind() }.map_err(PyRuntimeError::new_err)
}

#[pyfunction]
fn throw_thread(
    code: &Bound<'_, PyAny>,
    offset: &Bound<'_, PyAny>,
    _exception: &Bound<'_, PyAny>,
) -> PyResult<()> {
    bind_thread(code, offset)
}

#[pyfunction]
fn leave_thread(_code: &Bound<'_, PyAny>, _offset: &Bound<'_, PyAny>, _value: &Bound<'_, PyAny>) {
    // Exit events run before CPython unlinks its current frame.
    unsafe { bindings::leave() };
}

pub(super) struct Monitor {
    monitoring: Py<PyAny>,
    tool: u8,
    epoch: usize,
}

impl Monitor {
    pub(super) fn start() -> Result<Self, String> {
        Python::attach(|py| -> PyResult<Self> {
            if unsafe {
                pyo3::ffi::PyInterpreterState_Get() != pyo3::ffi::PyInterpreterState_Main()
            } {
                return Err(PyRuntimeError::new_err(
                    "Signal supports only the main interpreter",
                ));
            }
            let monitoring = py.import("sys")?.getattr("monitoring")?;
            let mut available = None;
            for id in (0..=5).rev() {
                if monitoring.call_method1("get_tool", (id,))?.is_none() {
                    available = Some(id);
                    break;
                }
            }
            let tool = available.ok_or_else(|| {
                PyRuntimeError::new_err(
                    "Signal on CPython 3.15 needs a free sys.monitoring tool ID",
                )
            })?;
            // This reserves only an unused tool and never changes other tools' events.
            monitoring.call_method1("use_tool_id", (tool, "pyroscope.signal"))?;
            let epoch = match unsafe { bindings::start() } {
                Ok(epoch) => epoch,
                Err(message) => {
                    monitoring.call_method1("free_tool_id", (tool,))?;
                    return Err(PyRuntimeError::new_err(message));
                }
            };
            let monitor = Self {
                monitoring: monitoring.clone().unbind(),
                tool,
                epoch,
            };
            let callback = wrap_pyfunction!(bind_thread, py)?;
            let events = monitoring.getattr("events")?;
            let mut mask = 0u32;
            for name in ENTRY_EVENTS {
                let event: u32 = events.getattr(name)?.extract()?;
                monitoring.call_method1("register_callback", (tool, event, &callback))?;
                mask |= event;
            }
            let callback = wrap_pyfunction!(throw_thread, py)?;
            let event: u32 = events.getattr("PY_THROW")?.extract()?;
            monitoring.call_method1("register_callback", (tool, event, &callback))?;
            mask |= event;
            let callback = wrap_pyfunction!(leave_thread, py)?;
            for name in EXIT_EVENTS {
                let event: u32 = events.getattr(name)?.extract()?;
                monitoring.call_method1("register_callback", (tool, event, &callback))?;
                mask |= event;
            }
            unsafe { bindings::bind() }.map_err(PyRuntimeError::new_err)?;
            monitoring.call_method1("set_events", (tool, mask))?;
            Ok(monitor)
        })
        .map_err(|error| format!("cannot enable Signal thread binding: {error}"))
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        // The session drops this only after disarming timers and removing its SIGPROF handler.
        // Also handles partial startup: invalidating first makes residual callbacks inert.
        bindings::stop(self.epoch);
        Python::attach(|py| {
            let monitoring = self.monitoring.bind(py);
            let release = || -> PyResult<()> {
                monitoring.call_method1("set_events", (self.tool, 0))?;
                let events = monitoring.getattr("events")?;
                for name in ENTRY_EVENTS
                    .into_iter()
                    .chain(EXIT_EVENTS)
                    .chain(["PY_THROW"])
                {
                    let event: u32 = events.getattr(name)?.extract()?;
                    monitoring.call_method1("register_callback", (self.tool, event, py.None()))?;
                }
                monitoring.call_method1("free_tool_id", (self.tool,))?;
                Ok(())
            };
            if let Err(error) = release() {
                log::warn!("failed to release Signal's monitoring tool: {error}");
            }
        });
    }
}
