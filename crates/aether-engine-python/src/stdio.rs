use aether_protocol::{IoPubHandle, JupyterMessage, StreamName};
use pyo3::prelude::*;
use std::sync::{Arc, Mutex};

/// Custom Python file-like stream object exposed via `pyo3` to replace `sys.stdout` and `sys.stderr`
/// during `%%python` cell execution.
///
/// Captures all written text into an in-memory Rust buffer AND immediately forwards non-empty chunks
/// to the ZMQ `IOPub` socket as `stream` (`stdout` / `stderr`) messages.
#[pyclass]
pub struct IoPubStreamWriter {
    stream_name: StreamName,
    buffer: Arc<Mutex<String>>,
    iopub: Option<IoPubHandle>,
    parent: Option<JupyterMessage>,
}

impl IoPubStreamWriter {
    pub fn new(
        stream_name: StreamName,
        buffer: Arc<Mutex<String>>,
        iopub: Option<IoPubHandle>,
        parent: Option<JupyterMessage>,
    ) -> Self {
        Self {
            stream_name,
            buffer,
            iopub,
            parent,
        }
    }
}

#[pymethods]
impl IoPubStreamWriter {
    pub fn write(&self, text: &str) -> PyResult<usize> {
        if text.is_empty() {
            return Ok(0);
        }
        if let Ok(mut guard) = self.buffer.lock() {
            guard.push_str(text);
        }
        if let Some(ref iopub) = self.iopub {
            let _ = iopub.send_stream(
                self.stream_name,
                text.to_string(),
                self.parent.as_ref(),
            );
        }
        Ok(text.len())
    }

    pub fn flush(&self) -> PyResult<()> {
        Ok(())
    }

    pub fn isatty(&self) -> PyResult<bool> {
        Ok(false)
    }

    #[getter]
    pub fn encoding(&self) -> PyResult<&'static str> {
        Ok("utf-8")
    }
}
