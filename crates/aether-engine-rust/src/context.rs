use crate::display::{format_mime_bundle, EVCXR_STARTUP_PRELUDE};
use aether_bridge::list_shared_tables;
use aether_protocol::{IoPubHandle, JupyterMessage, StreamName};
use evcxr::{CommandContext, Error as EvcxrError};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use thiserror::Error;
use tracing::{debug, info};

#[derive(Debug, Error)]
pub enum RustEngineError {
    #[error("evcxr evaluation error: {0}")]
    Evcxr(#[from] EvcxrError),
    #[error("task join error: {0}")]
    Join(#[from] tokio::task::JoinError),
}

#[derive(Debug, Clone)]
pub struct RustExecutionOutput {
    pub data: HashMap<String, Value>,
    pub metadata: HashMap<String, Value>,
    pub stdout_collected: String,
    pub stderr_collected: String,
}

/// Manages the stateful `evcxr::CommandContext` (which wraps `EvalContext`), streaming
/// stdout/stderr sinks to `IOPub`, and automatic zero-copy Arrow preamble injection.
pub struct RustEngine {
    inner: Arc<Mutex<RustEngineInner>>,
}

struct RustEngineInner {
    context: CommandContext,
    active_parent: Arc<Mutex<Option<(IoPubHandle, Option<JupyterMessage>)>>>,
    stdout_buf: Arc<Mutex<String>>,
    stderr_buf: Arc<Mutex<String>>,
    prelude_initialized: bool,
    injected_tables: HashSet<(String, usize, usize)>,
    applied_env: HashMap<String, String>,
}

impl RustEngine {
    /// Initializes a new `evcxr` evaluation context and spawns background stdout/stderr forwarding
    /// threads that route compiler/program output to the active `IOPub` channel.
    pub fn new() -> Result<Self, RustEngineError> {
        let (context, outputs) = CommandContext::new()?;

        let active_parent: Arc<Mutex<Option<(IoPubHandle, Option<JupyterMessage>)>>> =
            Arc::new(Mutex::new(None));
        let stdout_buf = Arc::new(Mutex::new(String::new()));
        let stderr_buf = Arc::new(Mutex::new(String::new()));

        // Spawn stdout sink thread
        {
            let rx = outputs.stdout;
            let active = Arc::clone(&active_parent);
            let buf = Arc::clone(&stdout_buf);
            std::thread::Builder::new()
                .name("aether-evcxr-stdout".to_string())
                .spawn(move || {
                    while let Ok(line) = rx.recv() {
                        let text = format!("{}\n", line);
                        if let Ok(mut b) = buf.lock() {
                            b.push_str(&text);
                        }
                        if let Ok(guard) = active.lock() {
                            if let Some((ref iopub, ref parent)) = *guard {
                                let _ = iopub.send_stream(
                                    StreamName::Stdout,
                                    text,
                                    parent.as_ref(),
                                );
                            }
                        }
                    }
                })
                .expect("failed to spawn evcxr stdout reader thread");
        }

        // Spawn stderr sink thread
        {
            let rx = outputs.stderr;
            let active = Arc::clone(&active_parent);
            let buf = Arc::clone(&stderr_buf);
            std::thread::Builder::new()
                .name("aether-evcxr-stderr".to_string())
                .spawn(move || {
                    while let Ok(line) = rx.recv() {
                        let text = format!("{}\n", line);
                        if let Ok(mut b) = buf.lock() {
                            b.push_str(&text);
                        }
                        if let Ok(guard) = active.lock() {
                            if let Some((ref iopub, ref parent)) = *guard {
                                let _ = iopub.send_stream(
                                    StreamName::Stderr,
                                    text,
                                    parent.as_ref(),
                                );
                            }
                        }
                    }
                })
                .expect("failed to spawn evcxr stderr reader thread");
        }

        info!("Initialized evcxr Rust context subprocess");

        Ok(Self {
            inner: Arc::new(Mutex::new(RustEngineInner {
                context,
                active_parent,
                stdout_buf,
                stderr_buf,
                prelude_initialized: false,
                injected_tables: HashSet::new(),
                applied_env: HashMap::new(),
            })),
        })
    }

    /// Evaluates a Rust cell inside `evcxr`, injecting the startup prelude (on first Rust cell),
    /// any newly shared Arrow tables, or updated `%%env` variables beforehand, and streaming
    /// stdout/stderr to `IOPub`.
    pub async fn execute_cell(
        &self,
        code: String,
        env_vars: HashMap<String, String>,
        iopub: Option<IoPubHandle>,
        parent: Option<JupyterMessage>,
    ) -> Result<RustExecutionOutput, RustEngineError> {
        let inner = Arc::clone(&self.inner);

        tokio::task::spawn_blocking(move || {
            let mut guard = inner.lock().expect("RustEngine mutex poisoned");

            // Reset per-cell stdout/stderr buffers and bind active IOPub parent
            if let Ok(mut b) = guard.stdout_buf.lock() {
                b.clear();
            }
            if let Ok(mut b) = guard.stderr_buf.lock() {
                b.clear();
            }
            if let Ok(mut active) = guard.active_parent.lock() {
                *active = iopub.map(|h| (h, parent));
            }

            // Build any necessary preamble for the first-run prelude, newly updated environment
            // variables, or shared Arrow tables
            let mut preamble_statements = Vec::new();

            if !guard.prelude_initialized {
                preamble_statements.push(EVCXR_STARTUP_PRELUDE.to_string());
                guard.prelude_initialized = true;
            }

            for (k, v) in &env_vars {
                if guard.applied_env.get(k) != Some(v) {
                    let escaped_k = k.replace('\\', "\\\\").replace('"', "\\\"");
                    let escaped_v = v.replace('\\', "\\\\").replace('"', "\\\"");
                    preamble_statements.push(format!(
                        r#"unsafe {{ std::env::set_var("{escaped_k}", "{escaped_v}"); }}"#
                    ));
                    guard.applied_env.insert(k.clone(), v.clone());
                }
            }

            for (table_name, ffi_ptrs, shm_path) in list_shared_tables() {
                let key = (table_name.clone(), ffi_ptrs.0, ffi_ptrs.1);
                if code.contains(&table_name) && !guard.injected_tables.contains(&key) {
                    let _ = aether_bridge::ensure_shm_file_for(&table_name);
                    let escaped_path = shm_path
                        .to_string_lossy()
                        .replace('\\', "\\\\")
                        .replace('"', "\\\"");
                    preamble_statements.push(format!(
                        r#"let {name} = AetherFrame::open_zero_copy("{path}", ({p0:#x}, {p1:#x})).expect("failed to map shared Arrow table '{name}'");"#,
                        name = table_name,
                        path = escaped_path,
                        p0 = ffi_ptrs.0,
                        p1 = ffi_ptrs.1,
                    ));
                    guard.injected_tables.insert(key);
                }
            }

            let full_code = if preamble_statements.is_empty() {
                code
            } else {
                debug!(
                    count = preamble_statements.len(),
                    "Injecting Aether preamble statements into evcxr cell"
                );
                format!("{}\n{}", preamble_statements.join("\n"), code)
            };

            let eval_res = guard.context.execute(&full_code);

            // Allow a brief window for stdout/stderr reader threads to flush trailing lines
            std::thread::sleep(std::time::Duration::from_millis(15));

            if let Ok(mut active) = guard.active_parent.lock() {
                *active = None;
            }

            let stdout_collected = guard
                .stdout_buf
                .lock()
                .map(|b| b.clone())
                .unwrap_or_default();
            let stderr_collected = guard
                .stderr_buf
                .lock()
                .map(|b| b.clone())
                .unwrap_or_default();

            let outputs = eval_res?;
            let (data, metadata) = format_mime_bundle(&outputs.content_by_mime_type);

            Ok(RustExecutionOutput {
                data,
                metadata,
                stdout_collected,
                stderr_collected,
            })
        })
        .await?
    }
}
