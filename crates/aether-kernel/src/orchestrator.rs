use crate::env_manager::EnvManager;
use crate::multiplexer::{parse_cell_magic, CellTarget};
use aether_engine_python::{PythonEngine, PythonEngineError};
use aether_engine_rust::RustEngine;
use aether_protocol::{
    ExecuteRequest, ExecutionState, IoPubHandle, IsCompleteRequest, JupyterMessage,
    KernelConnectionInfo, KernelSockets, ShutdownRequest, StreamName,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::{error, info, warn};
use zeromq::{SocketRecv, SocketSend};

#[derive(Debug)]
pub enum CellOutcome {
    Success {
        execution_count: u64,
        data: HashMap<String, Value>,
        metadata: HashMap<String, Value>,
    },
    Error {
        execution_count: u64,
        ename: String,
        evalue: String,
        traceback: Vec<String>,
    },
}

/// Core polyglot coordinator multiplexing cells across `evcxr` (Rust), `pyo3` (Python), and `%%env`.
pub struct PolyglotOrchestrator {
    rust_engine: RustEngine,
    python_engine: PythonEngine,
    env_manager: EnvManager,
    execution_count: AtomicU64,
}

impl PolyglotOrchestrator {
    pub fn new() -> anyhow::Result<Self> {
        let rust_engine = RustEngine::new()?;
        let python_engine = PythonEngine::new()?;
        let env_manager = EnvManager::new();

        Ok(Self {
            rust_engine,
            python_engine,
            env_manager,
            execution_count: AtomicU64::new(0),
        })
    }

    pub fn current_execution_count(&self) -> u64 {
        self.execution_count.load(Ordering::SeqCst)
    }

    /// Multiplexes and executes a notebook cell (`%%rust`, `%%python`, or `%%env`).
    pub async fn execute_cell(
        &self,
        raw_code: &str,
        store_history: bool,
        iopub: Option<IoPubHandle>,
        parent: Option<JupyterMessage>,
    ) -> CellOutcome {
        let exec_count = if store_history {
            self.execution_count.fetch_add(1, Ordering::SeqCst) + 1
        } else {
            self.execution_count.load(Ordering::SeqCst)
        };

        if let (Some(ref pub_handle), Some(ref parent_msg)) = (&iopub, &parent) {
            let _ = pub_handle.send_execute_input(raw_code, exec_count, parent_msg);
        }

        let target = parse_cell_magic(raw_code);
        match target {
            CellTarget::Env(env_body) => match self.env_manager.apply_env_block(&env_body) {
                Ok(applied) => {
                    let summary_lines = applied
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let msg = format!(
                        "Updated {} environment variable(s): {}\n",
                        applied.len(),
                        summary_lines
                    );
                    if let Some(ref pub_handle) = iopub {
                        let _ = pub_handle.send_stream(StreamName::Stdout, msg, parent.as_ref());
                    }
                    CellOutcome::Success {
                        execution_count: exec_count,
                        data: HashMap::new(),
                        metadata: HashMap::new(),
                    }
                }
                Err(err) => CellOutcome::Error {
                    execution_count: exec_count,
                    ename: "EnvConfigError".to_string(),
                    evalue: err.to_string(),
                    traceback: vec![err.to_string()],
                },
            },

            CellTarget::Python(py_code) => {
                let env_snapshot = self.env_manager.snapshot();
                match self.python_engine.execute_cell(
                    &py_code,
                    &env_snapshot,
                    iopub.clone(),
                    parent.clone(),
                ) {
                    Ok(out) => CellOutcome::Success {
                        execution_count: exec_count,
                        data: out.data,
                        metadata: out.metadata,
                    },
                    Err(PythonEngineError::Exception {
                        ename,
                        evalue,
                        traceback,
                    }) => CellOutcome::Error {
                        execution_count: exec_count,
                        ename,
                        evalue,
                        traceback,
                    },
                    Err(err) => CellOutcome::Error {
                        execution_count: exec_count,
                        ename: "PythonRuntimeError".to_string(),
                        evalue: err.to_string(),
                        traceback: vec![err.to_string()],
                    },
                }
            }

            CellTarget::Rust(rust_code) => {
                let env_snapshot = self.env_manager.snapshot();
                match self
                    .rust_engine
                    .execute_cell(rust_code, env_snapshot, iopub.clone(), parent.clone())
                    .await
                {
                    Ok(out) => CellOutcome::Success {
                        execution_count: exec_count,
                        data: out.data,
                        metadata: out.metadata,
                    },
                    Err(err) => {
                        let msg = err.to_string();
                        let lines = msg.lines().map(|s| s.to_string()).collect::<Vec<_>>();
                        CellOutcome::Error {
                            execution_count: exec_count,
                            ename: "RustCompileOrEvalError".to_string(),
                            evalue: msg,
                            traceback: lines,
                        }
                    }
                }
            }
        }
    }
}

/// Starts the full Jupyter Wire Protocol ZeroMQ server loop for `KernelConnectionInfo`.
pub async fn run_kernel_server(
    info: KernelConnectionInfo,
    orchestrator: Arc<PolyglotOrchestrator>,
) -> anyhow::Result<()> {
    let sockets = KernelSockets::bind(&info).await?;
    let signer = sockets.signer.clone();
    let iopub = sockets.iopub.clone();

    let _ = iopub.send_status(ExecutionState::Starting, None);
    info!("Aether Polyglot Kernel is online and ready for ZMQ requests");

    loop {
        let shell_sock = Arc::clone(&sockets.shell);
        let control_sock = Arc::clone(&sockets.control);

        tokio::select! {
            shell_res = async {
                let mut guard = shell_sock.lock().await;
                guard.recv().await
            } => {
                let raw_zmq = match shell_res {
                    Ok(m) => m,
                    Err(err) => {
                        error!(error = %err, "Error receiving from Shell socket");
                        break;
                    }
                };

                let msg = match JupyterMessage::from_zmq_message(raw_zmq, &signer) {
                    Ok(m) => m,
                    Err(err) => {
                        warn!(error = %err, "Rejected invalid/unauthenticated message on Shell socket");
                        continue;
                    }
                };

                let msg_type = msg.msg_type().unwrap_or_default();
                let _ = iopub.send_status(ExecutionState::Busy, Some(&msg));

                match msg_type.as_str() {
                    "kernel_info_request" => {
                        if let Ok(reply) = JupyterMessage::kernel_info_reply(&msg, &signer) {
                            let mut guard = sockets.shell.lock().await;
                            let _ = guard.send(reply.into_zmq_message(true)).await;
                        }
                    }
                    "is_complete_request" => {
                        let status = match msg.parse_content::<IsCompleteRequest>() {
                            Ok(req) if req.code.trim().ends_with('{') => "incomplete",
                            _ => "complete",
                        };
                        if let Ok(reply) = JupyterMessage::is_complete_reply(status, &msg, &signer) {
                            let mut guard = sockets.shell.lock().await;
                            let _ = guard.send(reply.into_zmq_message(true)).await;
                        }
                    }
                    "execute_request" => {
                        match msg.parse_content::<ExecuteRequest>() {
                            Ok(req) => {
                                let outcome = orchestrator
                                    .execute_cell(&req.code, req.store_history, Some(iopub.clone()), Some(msg.clone()))
                                    .await;

                                match outcome {
                                    CellOutcome::Success { execution_count, data, metadata } => {
                                        if !req.silent && !data.is_empty() {
                                            let _ = iopub.send_execute_result(execution_count, data, metadata, &msg);
                                        }
                                        if let Ok(reply) = JupyterMessage::execute_reply_ok(execution_count, &msg, &signer) {
                                            let mut guard = sockets.shell.lock().await;
                                            let _ = guard.send(reply.into_zmq_message(true)).await;
                                        }
                                    }
                                    CellOutcome::Error { execution_count, ename, evalue, traceback } => {
                                        let _ = iopub.send_error(&ename, &evalue, &traceback, Some(&msg));
                                        if let Ok(reply) = JupyterMessage::execute_reply_error(
                                            execution_count,
                                            &ename,
                                            &evalue,
                                            &traceback,
                                            &msg,
                                            &signer,
                                        ) {
                                            let mut guard = sockets.shell.lock().await;
                                            let _ = guard.send(reply.into_zmq_message(true)).await;
                                        }
                                    }
                                }
                            }
                            Err(err) => {
                                warn!(error = %err, "Failed to parse execute_request payload");
                            }
                        }
                    }
                    "shutdown_request" => {
                        let restart = msg
                            .parse_content::<ShutdownRequest>()
                            .map(|r| r.restart)
                            .unwrap_or(false);
                        if let Ok(reply) = JupyterMessage::shutdown_reply(restart, &msg, &signer) {
                            let mut guard = sockets.shell.lock().await;
                            let _ = guard.send(reply.into_zmq_message(true)).await;
                        }
                        let _ = iopub.send_status(ExecutionState::Idle, Some(&msg));
                        info!("Processed shutdown_request on Shell socket; exiting");
                        break;
                    }
                    other => {
                        // Respond with an empty or standard reply if needed, or log unsupported message
                        debug_unhandled_msg(other);
                    }
                }

                let _ = iopub.send_status(ExecutionState::Idle, Some(&msg));
            }

            control_res = async {
                let mut guard = control_sock.lock().await;
                guard.recv().await
            } => {
                let raw_zmq = match control_res {
                    Ok(m) => m,
                    Err(err) => {
                        error!(error = %err, "Error receiving from Control socket");
                        break;
                    }
                };

                let msg = match JupyterMessage::from_zmq_message(raw_zmq, &signer) {
                    Ok(m) => m,
                    Err(err) => {
                        warn!(error = %err, "Rejected invalid/unauthenticated message on Control socket");
                        continue;
                    }
                };

                let msg_type = msg.msg_type().unwrap_or_default();
                let _ = iopub.send_status(ExecutionState::Busy, Some(&msg));

                match msg_type.as_str() {
                    "shutdown_request" => {
                        let restart = msg
                            .parse_content::<ShutdownRequest>()
                            .map(|r| r.restart)
                            .unwrap_or(false);
                        if let Ok(reply) = JupyterMessage::shutdown_reply(restart, &msg, &signer) {
                            let mut guard = sockets.control.lock().await;
                            let _ = guard.send(reply.into_zmq_message(true)).await;
                        }
                        let _ = iopub.send_status(ExecutionState::Idle, Some(&msg));
                        info!("Processed shutdown_request on Control socket; exiting");
                        break;
                    }
                    "interrupt_request" => {
                        if let Ok(reply) = JupyterMessage::new_signed(
                            msg.zmq_identities.clone(),
                            "interrupt_reply",
                            Some(&msg),
                            None,
                            &json!({ "status": "ok" }),
                            &signer,
                        ) {
                            let mut guard = sockets.control.lock().await;
                            let _ = guard.send(reply.into_zmq_message(true)).await;
                        }
                    }
                    "kernel_info_request" => {
                        if let Ok(reply) = JupyterMessage::kernel_info_reply(&msg, &signer) {
                            let mut guard = sockets.control.lock().await;
                            let _ = guard.send(reply.into_zmq_message(true)).await;
                        }
                    }
                    other => {
                        debug_unhandled_msg(other);
                    }
                }

                let _ = iopub.send_status(ExecutionState::Idle, Some(&msg));
            }
        }
    }

    sockets.hb_task.abort();
    // Allow brief drain of IOPub messages before aborting task
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    sockets.iopub_task.abort();
    Ok(())
}

fn debug_unhandled_msg(msg_type: &str) {
    tracing::debug!(msg_type = %msg_type, "Received unhandled Jupyter message type");
}
