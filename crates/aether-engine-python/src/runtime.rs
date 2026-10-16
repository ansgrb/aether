use crate::magic_bridge::__share_to_rust__;
use crate::stdio::IoPubStreamWriter;
use aether_protocol::{IoPubHandle, JupyterMessage, StreamName};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::ffi::CString;
use std::path::Path;
use std::sync::{Arc, Mutex};
use thiserror::Error;
use tracing::info;

#[derive(Debug, Error)]
pub enum PythonEngineError {
    #[error("{ename}: {evalue}")]
    Exception {
        ename: String,
        evalue: String,
        traceback: Vec<String>,
    },
    #[error("PyO3 runtime error: {0}")]
    PyO3(String),
}

#[derive(Debug, Clone)]
pub struct PythonExecutionOutput {
    pub data: HashMap<String, Value>,
    pub metadata: HashMap<String, Value>,
    pub stdout_collected: String,
    pub stderr_collected: String,
}

const CELL_RUNNER_SCRIPT: &str = r#"
import ast

def __aether_run_cell__(source, ns):
    tree = ast.parse(source, filename="<aether-python-cell>", mode="exec")
    if not tree.body:
        return None
    last_node = tree.body[-1]
    stripped = source.rstrip()
    is_trailing_expr = isinstance(last_node, ast.Expr) and not stripped.endswith(";")
    if is_trailing_expr:
        prefix_body = tree.body[:-1]
        if prefix_body:
            mod = ast.Module(body=prefix_body, type_ignores=[])
            ast.fix_missing_locations(mod)
            exec(compile(mod, "<aether-python-cell>", "exec"), ns, ns)
        expr = ast.Expression(body=last_node.value)
        ast.fix_missing_locations(expr)
        val = eval(compile(expr, "<aether-python-cell>", "eval"), ns, ns)
        if val is None:
            return None
        text_repr = repr(val)
        html_repr = val._repr_html_() if hasattr(val, "_repr_html_") and callable(val._repr_html_) else None
        svg_repr = val._repr_svg_() if hasattr(val, "_repr_svg_") and callable(val._repr_svg_) else None
        return (text_repr, html_repr, svg_repr)
    else:
        exec(compile(tree, "<aether-python-cell>", "exec"), ns, ns)
        return None
"#;

/// Stateful embedded Python execution engine backed by `pyo3`.
#[derive(Clone)]
pub struct PythonEngine {
    globals: Arc<Mutex<Py<PyDict>>>,
}

impl PythonEngine {
    /// Initializes the embedded Python interpreter, configures `sys.path` to include any workspace
    /// `.venv` site-packages, and injects `__share_to_rust__` into the persistent cell globals.
    pub fn new() -> Result<Self, PythonEngineError> {
        pyo3::prepare_freethreaded_python();

        let globals = Python::with_gil(|py| -> PyResult<Py<PyDict>> {
            Self::configure_venv_site_packages(py)?;

            let main_mod = py.import("__main__")?;
            let globals_dict = main_mod.dict().copy()?;

            // Inject the zero-copy Arrow bridge magic function `__share_to_rust__`
            let share_fn = wrap_pyfunction!(__share_to_rust__, py)?;
            globals_dict.set_item("__share_to_rust__", share_fn)?;

            // Compile and register the AST cell runner helper
            let c_script = CString::new(CELL_RUNNER_SCRIPT).expect("valid CString");
            py.run(&c_script, Some(&globals_dict), Some(&globals_dict))?;

            Ok(globals_dict.unbind())
        })
        .map_err(|e| PythonEngineError::PyO3(e.to_string()))?;

        info!("Initialized embedded Python runtime (PyO3) with __share_to_rust__ bridge");

        Ok(Self {
            globals: Arc::new(Mutex::new(globals)),
        })
    }

    fn configure_venv_site_packages(py: Python<'_>) -> PyResult<()> {
        let sys = py.import("sys")?;
        let sys_path = sys.getattr("path")?.downcast_into::<PyList>()?;

        // Check common workspace .venv locations so packages installed in `.venv` (e.g., polars, pyarrow)
        // are discoverable when the kernel binary is invoked directly.
        let mut candidates = Vec::new();
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd.join(".venv"));
            if let Some(parent) = cwd.parent() {
                candidates.push(parent.join(".venv"));
                if let Some(grandparent) = parent.parent() {
                    candidates.push(grandparent.join(".venv"));
                }
            }
        }
        let manifest_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent());
        if let Some(root) = manifest_root {
            candidates.push(root.join(".venv"));
        }

        for venv in candidates {
            let lib_dir = venv.join("lib");
            if let Ok(entries) = std::fs::read_dir(&lib_dir) {
                for entry in entries.flatten() {
                    let site_pkgs = entry.path().join("site-packages");
                    if site_pkgs.is_dir() {
                        let path_str = site_pkgs.to_string_lossy().to_string();
                        if !sys_path.contains(&path_str)? {
                            sys_path.insert(0, &path_str)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Executes a Python cell inside `Python::with_gil`, redirecting `sys.stdout` and `sys.stderr`
    /// to `IoPubStreamWriter` and capturing any trailing expression MIME bundle.
    pub fn execute_cell(
        &self,
        code: &str,
        env_vars: &HashMap<String, String>,
        iopub: Option<IoPubHandle>,
        parent: Option<JupyterMessage>,
    ) -> Result<PythonExecutionOutput, PythonEngineError> {
        let globals_guard = self.globals.lock().expect("PythonEngine mutex poisoned");

        let stdout_buf = Arc::new(Mutex::new(String::new()));
        let stderr_buf = Arc::new(Mutex::new(String::new()));

        Python::with_gil(|py| {
            let globals = globals_guard.bind(py);

            // 1. Synchronize environment variables from `%%env` into `os.environ`
            if !env_vars.is_empty() {
                if let Ok(os_mod) = py.import("os") {
                    if let Ok(environ) = os_mod.getattr("environ") {
                        for (k, v) in env_vars {
                            let _ = environ.set_item(k, v);
                        }
                    }
                }
            }

            // 2. Replace `sys.stdout` and `sys.stderr` with `IoPubStreamWriter`
            let sys = py
                .import("sys")
                .map_err(|e| PythonEngineError::PyO3(e.to_string()))?;
            let orig_stdout = sys.getattr("stdout").ok();
            let orig_stderr = sys.getattr("stderr").ok();

            let stdout_writer = Py::new(
                py,
                IoPubStreamWriter::new(
                    StreamName::Stdout,
                    Arc::clone(&stdout_buf),
                    iopub.clone(),
                    parent.clone(),
                ),
            )
            .map_err(|e| PythonEngineError::PyO3(e.to_string()))?;

            let stderr_writer = Py::new(
                py,
                IoPubStreamWriter::new(
                    StreamName::Stderr,
                    Arc::clone(&stderr_buf),
                    iopub.clone(),
                    parent.clone(),
                ),
            )
            .map_err(|e| PythonEngineError::PyO3(e.to_string()))?;

            let _ = sys.setattr("stdout", stdout_writer.bind(py));
            let _ = sys.setattr("stderr", stderr_writer.bind(py));

            // 3. Execute the cell via `__aether_run_cell__(code, globals)`
            let runner = globals
                .get_item("__aether_run_cell__")
                .map_err(|e| PythonEngineError::PyO3(e.to_string()))?
                .expect("__aether_run_cell__ registered at startup");

            let exec_result = runner.call1((code, globals));

            // Restore original `sys.stdout` and `sys.stderr`
            if let Some(orig) = orig_stdout {
                let _ = sys.setattr("stdout", orig);
            }
            if let Some(orig) = orig_stderr {
                let _ = sys.setattr("stderr", orig);
            }

            let stdout_collected = stdout_buf
                .lock()
                .map(|b| b.clone())
                .unwrap_or_default();
            let stderr_collected = stderr_buf
                .lock()
                .map(|b| b.clone())
                .unwrap_or_default();

            match exec_result {
                Ok(val) => {
                    let mut data = HashMap::new();
                    let metadata = HashMap::new();

                    if !val.is_none() {
                        if let Ok((text_repr, html_repr, svg_repr)) =
                            val.extract::<(String, Option<String>, Option<String>)>()
                        {
                            data.insert("text/plain".to_string(), json!(text_repr));
                            if let Some(html) = html_repr {
                                data.insert("text/html".to_string(), json!(html));
                            }
                            if let Some(svg) = svg_repr {
                                data.insert("image/svg+xml".to_string(), json!(svg));
                            }
                        }
                    }

                    Ok(PythonExecutionOutput {
                        data,
                        metadata,
                        stdout_collected,
                        stderr_collected,
                    })
                }
                Err(py_err) => Err(Self::format_python_exception(py, py_err)),
            }
        })
    }

    fn format_python_exception(py: Python<'_>, err: PyErr) -> PythonEngineError {
        let ename = err
            .get_type(py)
            .qualname()
            .map(|s| s.to_string())
            .unwrap_or_else(|_| "PythonError".to_string());
        let evalue = err
            .value(py)
            .str()
            .map(|s| s.to_string())
            .unwrap_or_else(|_| err.to_string());

        let traceback = (|| -> PyResult<Vec<String>> {
            let tb_mod = py.import("traceback")?;
            let formatted =
                tb_mod.call_method1("format_exception", (err.get_type(py), err.value(py), err.traceback(py)))?;
            let lines: Vec<String> = formatted.extract()?;
            Ok(lines
                .into_iter()
                .map(|l| l.trim_end_matches('\n').to_string())
                .collect())
        })()
        .unwrap_or_else(|_| vec![format!("{ename}: {evalue}")]);

        PythonEngineError::Exception {
            ename,
            evalue,
            traceback,
        }
    }
}
