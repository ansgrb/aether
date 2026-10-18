//! Polyglot Kernel Manager (`src/kernel/mod.rs`)
//!
//! Manages isolated local language runtimes (Rust via `evcxr`/`rustc`, Python, and Bash)
//! using asynchronous Tokio standard input/output pipes and real-time event channels.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, Mutex};
use uuid::Uuid;

/// Supported execution languages in the Mission Notebook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum KernelLanguage {
    Rust,
    Python,
    Bash,
}

impl KernelLanguage {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Rust => "Rust (EvCxR / rustc)",
            Self::Python => "Python 3",
            Self::Bash => "Bash Shell",
        }
    }

    pub fn short_tag(&self) -> &'static str {
        match self {
            Self::Rust => "RUST",
            Self::Python => "PY3",
            Self::Bash => "BASH",
        }
    }
}

/// Execution state of an individual notebook cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CellExecutionStatus {
    Idle,
    Running,
    Success { duration_ms: u64 },
    Error { duration_ms: u64, message: String },
}

/// Persistent `evcxr --json` subprocess session backed by async Tokio pipes.
pub struct KernelInstance {
    process: Child,
    sender: mpsc::Sender<String>,
}

impl KernelInstance {
    /// Spawns a persistent `evcxr --json` process wired to Tokio async stdin/stdout pipes.
    pub async fn spawn_evcxr(output_tx: mpsc::Sender<String>) -> Result<Self, std::io::Error> {
        let mut child = Command::new("evcxr")
            .arg("--json")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let stdin = child.stdin.take().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Failed to open stdin pipe")
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Failed to open stdout pipe")
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Failed to open stderr pipe")
        })?;
        let (tx, mut rx) = mpsc::channel::<String>(32);

        // Async loop feeding input to the kernel process
        tokio::spawn(async move {
            let mut writer = stdin;
            while let Some(code) = rx.recv().await {
                if writer.write_all(code.as_bytes()).await.is_err() {
                    break;
                }
                if writer.flush().await.is_err() {
                    break;
                }
            }
        });

        // Async reader fetching streaming results from stdout
        let out_tx_clone = output_tx.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let _ = out_tx_clone.send(line).await;
            }
        });

        // Async reader fetching diagnostics from stderr
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let _ = output_tx.send(format!("[stderr] {}", line)).await;
            }
        });

        Ok(KernelInstance {
            process: child,
            sender: tx,
        })
    }

    /// Sends a code block to the persistent `evcxr` stdin pipe.
    pub async fn execute_block(&self, code: String) {
        let formatted_execution = format!("{}\n", code);
        let _ = self.sender.send(formatted_execution).await;
    }

    /// Terminates the underlying `evcxr` child process.
    pub async fn kill(&mut self) -> Result<(), std::io::Error> {
        self.process.kill().await
    }
}

/// Commands dispatched from the Cockpit UI to the background Kernel Orchestrator.
#[derive(Debug, Clone)]
pub enum KernelCommand {
    ExecuteCell {
        cell_id: Uuid,
        language: KernelLanguage,
        code: String,
    },
    KillCell {
        cell_id: Uuid,
    },
    ResetAll,
}

/// Streaming events emitted by the Kernel Orchestrator back to the Cockpit UI.
#[derive(Debug, Clone)]
pub enum KernelEvent {
    CellStarted {
        cell_id: Uuid,
        language: KernelLanguage,
    },
    CellStdout {
        cell_id: Uuid,
        line: String,
    },
    CellStderr {
        cell_id: Uuid,
        line: String,
    },
    CellFinished {
        cell_id: Uuid,
        status: CellExecutionStatus,
    },
    DiagnosticLog {
        level: &'static str,
        message: String,
    },
}

/// Persistent Python REPL driver that preserves variables/imports across cells.
struct PersistentPythonWorker {
    child: Child,
    stdin: tokio::process::ChildStdin,
    stdout_lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
}

impl PersistentPythonWorker {
    async fn spawn() -> Result<Self, String> {
        let python_bin = Self::resolve_python_binary();
        let bootstrap = r#"
import sys, traceback

_SESSION_GLOBALS = {"__name__": "__main__"}

while True:
    header = sys.stdin.readline()
    if not header:
        break
    header = header.strip()
    if not header.startswith("EXEC "):
        continue
    parts = header.split(" ")
    sentinel = parts[1]
    num_bytes = int(parts[2])
    code = sys.stdin.read(num_bytes)
    try:
        import ast
        tree = ast.parse(code, filename="<cell>", mode="exec")
        if tree.body and isinstance(tree.body[-1], ast.Expr):
            last_expr = ast.Expression(tree.body.pop().value)
            if tree.body:
                exec(compile(tree, "<cell>", "exec"), _SESSION_GLOBALS)
            val = eval(compile(last_expr, "<cell>", "eval"), _SESSION_GLOBALS)
            if val is not None:
                print(repr(val))
        else:
            exec(compile(tree, "<cell>", "exec"), _SESSION_GLOBALS)
        sys.stdout.flush()
        sys.stderr.flush()
        print(f"{sentinel}:0", flush=True)
    except Exception:
        traceback.print_exc(file=sys.stdout)
        sys.stdout.flush()
        print(f"{sentinel}:1", flush=True)
"#;

        let mut child = Command::new(&python_bin)
            .arg("-u")
            .arg("-c")
            .arg(bootstrap)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to spawn Python ({python_bin}): {e}"))?;

        let stdin = child.stdin.take().ok_or("Failed to open Python stdin")?;
        let stdout = child.stdout.take().ok_or("Failed to open Python stdout")?;

        Ok(Self {
            child,
            stdin,
            stdout_lines: BufReader::new(stdout).lines(),
        })
    }

    fn resolve_python_binary() -> String {
        let venv_py = PathBuf::from(".venv/bin/python3");
        if venv_py.exists() {
            return venv_py.to_string_lossy().into_owned();
        }
        "python3".to_string()
    }

    async fn execute_cell(
        &mut self,
        cell_id: Uuid,
        code: &str,
        event_tx: &mpsc::UnboundedSender<KernelEvent>,
    ) -> Result<bool, String> {
        let sentinel = format!("__AETHER_PY_DONE_{}__", Uuid::new_v4().simple());
        let header = format!("EXEC {} {}\n", sentinel, code.len());

        self.stdin
            .write_all(header.as_bytes())
            .await
            .map_err(|e| format!("Python stdin write error: {e}"))?;
        self.stdin
            .write_all(code.as_bytes())
            .await
            .map_err(|e| format!("Python payload write error: {e}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| format!("Python stdin flush error: {e}"))?;

        while let Some(line) = self
            .stdout_lines
            .next_line()
            .await
            .map_err(|e| format!("Python stdout read error: {e}"))?
        {
            if let Some(rest) = line.strip_prefix(&sentinel) {
                let ok = rest.trim() == ":0";
                return Ok(ok);
            }
            let _ = event_tx.send(KernelEvent::CellStdout { cell_id, line });
        }

        Err("Python subprocess terminated unexpectedly".to_string())
    }

    async fn kill(&mut self) {
        let _ = self.child.kill().await;
    }
}

/// Persistent Bash worker that preserves working directory and environment variables across cells.
struct PersistentBashWorker {
    child: Child,
    stdin: tokio::process::ChildStdin,
    stdout_lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
}

impl PersistentBashWorker {
    async fn spawn() -> Result<Self, String> {
        let mut child = Command::new("bash")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to spawn bash: {e}"))?;

        let stdin = child.stdin.take().ok_or("Failed to open bash stdin")?;
        let stdout = child.stdout.take().ok_or("Failed to open bash stdout")?;

        Ok(Self {
            child,
            stdin,
            stdout_lines: BufReader::new(stdout).lines(),
        })
    }

    async fn execute_cell(
        &mut self,
        cell_id: Uuid,
        code: &str,
        event_tx: &mpsc::UnboundedSender<KernelEvent>,
    ) -> Result<i32, String> {
        let sentinel = format!("__AETHER_BASH_DONE_{}__", Uuid::new_v4().simple());
        let wrapped = format!(
            "{{\n{}\n}} 2>&1\n__ec=$?\necho \"{}:${{__ec}}\"\n",
            code, sentinel
        );

        self.stdin
            .write_all(wrapped.as_bytes())
            .await
            .map_err(|e| format!("Bash stdin write error: {e}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| format!("Bash flush error: {e}"))?;

        while let Some(line) = self
            .stdout_lines
            .next_line()
            .await
            .map_err(|e| format!("Bash stdout read error: {e}"))?
        {
            if let Some(rest) = line.strip_prefix(&sentinel) {
                let code_str = rest.strip_prefix(':').unwrap_or("0").trim();
                let exit_code = code_str.parse::<i32>().unwrap_or(0);
                return Ok(exit_code);
            }
            let _ = event_tx.send(KernelEvent::CellStdout { cell_id, line });
        }

        Err("Bash subprocess terminated unexpectedly".to_string())
    }

    async fn kill(&mut self) {
        let _ = self.child.kill().await;
    }
}

/// Handle held by the UI to communicate with the background Tokio Kernel Orchestrator.
pub struct KernelBridge {
    pub cmd_tx: mpsc::UnboundedSender<KernelCommand>,
    pub event_rx: mpsc::UnboundedReceiver<KernelEvent>,
}

impl KernelBridge {
    /// Spawns the dedicated Tokio runtime thread managing all polyglot kernels.
    pub fn spawn() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<KernelCommand>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<KernelEvent>();

        std::thread::Builder::new()
            .name("aether-kernel-orchestrator".to_string())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to initialize Tokio runtime");

                rt.block_on(run_orchestrator_loop(cmd_rx, event_tx));
            })
            .expect("Failed to spawn kernel orchestrator thread");

        Self { cmd_tx, event_rx }
    }

    pub fn execute_cell(&self, cell_id: Uuid, language: KernelLanguage, code: String) {
        let _ = self.cmd_tx.send(KernelCommand::ExecuteCell {
            cell_id,
            language,
            code,
        });
    }

    pub fn kill_cell(&self, cell_id: Uuid) {
        let _ = self.cmd_tx.send(KernelCommand::KillCell { cell_id });
    }

    pub fn reset_all(&self) {
        let _ = self.cmd_tx.send(KernelCommand::ResetAll);
    }
}

async fn run_orchestrator_loop(
    mut cmd_rx: mpsc::UnboundedReceiver<KernelCommand>,
    event_tx: mpsc::UnboundedSender<KernelEvent>,
) {
    let py_worker: Arc<Mutex<Option<PersistentPythonWorker>>> = Arc::new(Mutex::new(None));
    let bash_worker: Arc<Mutex<Option<PersistentBashWorker>>> = Arc::new(Mutex::new(None));
    let rust_session_items: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let active_tasks: Arc<Mutex<HashMap<Uuid, tokio::task::AbortHandle>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let _ = event_tx.send(KernelEvent::DiagnosticLog {
        level: "INFO",
        message: "Polyglot Kernel Orchestrator online (Rust / Python3 / Bash)".to_string(),
    });

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            KernelCommand::ExecuteCell {
                cell_id,
                language,
                code,
            } => {
                let event_tx_clone = event_tx.clone();
                let py_worker_clone = Arc::clone(&py_worker);
                let bash_worker_clone = Arc::clone(&bash_worker);
                let rust_items_clone = Arc::clone(&rust_session_items);
                let active_tasks_clone = Arc::clone(&active_tasks);

                let _ = event_tx.send(KernelEvent::CellStarted { cell_id, language });
                let _ = event_tx.send(KernelEvent::DiagnosticLog {
                    level: "EXEC",
                    message: format!(
                        "Executing cell {} [{}]",
                        &cell_id.to_string()[..8],
                        language.short_tag()
                    ),
                });

                let join_handle = tokio::spawn(async move {
                    let start = Instant::now();
                    let status = match language {
                        KernelLanguage::Rust => {
                            execute_rust_cell(cell_id, &code, &rust_items_clone, &event_tx_clone)
                                .await
                        }
                        KernelLanguage::Python => {
                            execute_python_cell(cell_id, &code, &py_worker_clone, &event_tx_clone)
                                .await
                        }
                        KernelLanguage::Bash => {
                            execute_bash_cell(cell_id, &code, &bash_worker_clone, &event_tx_clone)
                                .await
                        }
                    };

                    let duration_ms = start.elapsed().as_millis() as u64;
                    let final_status = match status {
                        Ok(()) => CellExecutionStatus::Success { duration_ms },
                        Err(message) => CellExecutionStatus::Error {
                            duration_ms,
                            message,
                        },
                    };

                    active_tasks_clone.lock().await.remove(&cell_id);
                    let _ = event_tx_clone.send(KernelEvent::CellFinished {
                        cell_id,
                        status: final_status,
                    });
                });

                active_tasks
                    .lock()
                    .await
                    .insert(cell_id, join_handle.abort_handle());
            }

            KernelCommand::KillCell { cell_id } => {
                if let Some(handle) = active_tasks.lock().await.remove(&cell_id) {
                    handle.abort();
                }
                if let Some(mut py) = py_worker.lock().await.take() {
                    py.kill().await;
                }
                if let Some(mut sh) = bash_worker.lock().await.take() {
                    sh.kill().await;
                }
                let _ = event_tx.send(KernelEvent::CellFinished {
                    cell_id,
                    status: CellExecutionStatus::Error {
                        duration_ms: 0,
                        message: "Execution aborted by operator".to_string(),
                    },
                });
                let _ = event_tx.send(KernelEvent::DiagnosticLog {
                    level: "WARN",
                    message: format!("Aborted cell {}", &cell_id.to_string()[..8]),
                });
            }

            KernelCommand::ResetAll => {
                for (_, handle) in active_tasks.lock().await.drain() {
                    handle.abort();
                }
                if let Some(mut py) = py_worker.lock().await.take() {
                    py.kill().await;
                }
                if let Some(mut sh) = bash_worker.lock().await.take() {
                    sh.kill().await;
                }
                rust_session_items.lock().await.clear();
                let _ = event_tx.send(KernelEvent::DiagnosticLog {
                    level: "WARN",
                    message: "All kernel sessions and state buffers reset.".to_string(),
                });
            }
        }
    }
}

async fn execute_python_cell(
    cell_id: Uuid,
    code: &str,
    worker_slot: &Arc<Mutex<Option<PersistentPythonWorker>>>,
    event_tx: &mpsc::UnboundedSender<KernelEvent>,
) -> Result<(), String> {
    let mut guard = worker_slot.lock().await;
    if guard.is_none() {
        *guard = Some(PersistentPythonWorker::spawn().await?);
    }

    let worker = guard
        .as_mut()
        .ok_or_else(|| "Python worker unavailable".to_string())?;
    match worker.execute_cell(cell_id, code, event_tx).await {
        Ok(true) => Ok(()),
        Ok(false) => Err("Python cell raised an exception".to_string()),
        Err(err) => {
            *guard = None;
            let _ = event_tx.send(KernelEvent::CellStderr {
                cell_id,
                line: err.clone(),
            });
            Err(err)
        }
    }
}

async fn execute_bash_cell(
    cell_id: Uuid,
    code: &str,
    worker_slot: &Arc<Mutex<Option<PersistentBashWorker>>>,
    event_tx: &mpsc::UnboundedSender<KernelEvent>,
) -> Result<(), String> {
    let mut guard = worker_slot.lock().await;
    if guard.is_none() {
        *guard = Some(PersistentBashWorker::spawn().await?);
    }

    let worker = guard
        .as_mut()
        .ok_or_else(|| "Bash worker unavailable".to_string())?;
    match worker.execute_cell(cell_id, code, event_tx).await {
        Ok(0) => Ok(()),
        Ok(code_num) => Err(format!("Process exited with status {code_num}")),
        Err(err) => {
            *guard = None;
            let _ = event_tx.send(KernelEvent::CellStderr {
                cell_id,
                line: err.clone(),
            });
            Err(err)
        }
    }
}

/// Compiles and executes a Rust cell via `rustc` in an isolated sandbox directory,
/// preserving top-level `use` and `const` definitions across cells.
async fn execute_rust_cell(
    cell_id: Uuid,
    code: &str,
    session_items: &Arc<Mutex<Vec<String>>>,
    event_tx: &mpsc::UnboundedSender<KernelEvent>,
) -> Result<(), String> {
    let sandbox_dir = std::env::temp_dir().join("aether_rust_sandbox");
    std::fs::create_dir_all(&sandbox_dir)
        .map_err(|e| format!("Failed to create Rust sandbox directory: {e}"))?;

    let src_path = sandbox_dir.join(format!("cell_{}.rs", cell_id.simple()));
    let bin_path = sandbox_dir.join(format!("cell_{}_bin", cell_id.simple()));

    let prior_items = session_items.lock().await.clone();
    let (new_top_items, full_source) = compose_rust_source(code, &prior_items);

    std::fs::write(&src_path, &full_source)
        .map_err(|e| format!("Failed to write Rust source: {e}"))?;

    let compile_out = Command::new("rustc")
        .arg("--edition=2021")
        .arg("-O")
        .arg(&src_path)
        .arg("-o")
        .arg(&bin_path)
        .output()
        .await
        .map_err(|e| format!("Failed to invoke rustc: {e}"))?;

    let _ = std::fs::remove_file(&src_path);

    if !compile_out.status.success() {
        let stderr_text = String::from_utf8_lossy(&compile_out.stderr);
        for line in stderr_text.lines() {
            let _ = event_tx.send(KernelEvent::CellStderr {
                cell_id,
                line: line.to_string(),
            });
        }
        return Err("Rust compilation failed".to_string());
    }

    if !new_top_items.is_empty() {
        let mut guard = session_items.lock().await;
        for item in new_top_items {
            if !guard.contains(&item) {
                guard.push(item);
            }
        }
    }

    let mut child = Command::new(&bin_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to launch compiled Rust cell: {e}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture Rust cell stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Failed to capture Rust cell stderr".to_string())?;

    let tx_out = event_tx.clone();
    let out_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            let _ = tx_out.send(KernelEvent::CellStdout { cell_id, line });
        }
    });

    let tx_err = event_tx.clone();
    let err_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            let _ = tx_err.send(KernelEvent::CellStderr { cell_id, line });
        }
    });

    let status = child
        .wait()
        .await
        .map_err(|e| format!("Failed waiting on Rust cell process: {e}"))?;

    let _ = out_task.await;
    let _ = err_task.await;
    let _ = std::fs::remove_file(&bin_path);

    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "Rust cell exited with status {}",
            status.code().unwrap_or(-1)
        ))
    }
}

fn compose_rust_source(code: &str, prior_items: &[String]) -> (Vec<String>, String) {
    if code.contains("fn main") {
        return (Vec::new(), code.to_string());
    }

    let mut extracted_imports = Vec::new();
    let mut body_lines = Vec::new();

    for line in code.lines() {
        let trimmed = line.trim();
        if (trimmed.starts_with("use ") || trimmed.starts_with("const ")) && trimmed.ends_with(';')
        {
            extracted_imports.push(trimmed.to_string());
        } else {
            body_lines.push(line);
        }
    }

    let mut source = String::from("#![allow(unused, dead_code)]\n");
    for item in prior_items {
        source.push_str(item);
        source.push('\n');
    }
    for item in &extracted_imports {
        if !prior_items.contains(item) {
            source.push_str(item);
            source.push('\n');
        }
    }

    source.push_str("\nfn main() {\n");
    for line in body_lines {
        source.push_str("    ");
        source.push_str(line);
        source.push('\n');
    }
    source.push_str("}\n");

    (extracted_imports, source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_python_and_bash_persistent_workers() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let py_slot = Arc::new(Mutex::new(None));
        let cell1 = Uuid::new_v4();
        let cell2 = Uuid::new_v4();

        execute_python_cell(cell1, "mission_code = 42\nprint('init_ok')", &py_slot, &tx)
            .await
            .expect("Python cell 1 should succeed");

        execute_python_cell(cell2, "print(f'val={mission_code * 2}')", &py_slot, &tx)
            .await
            .expect("Python cell 2 should read state from cell 1");

        let mut lines = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let KernelEvent::CellStdout { line, .. } = ev {
                lines.push(line);
            }
        }
        assert!(lines.contains(&"init_ok".to_string()));
        assert!(lines.contains(&"val=84".to_string()));
    }

    #[tokio::test]
    async fn test_rust_cell_execution() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let items = Arc::new(Mutex::new(Vec::new()));
        let cell_id = Uuid::new_v4();

        execute_rust_cell(
            cell_id,
            "const MAGIC: u32 = 1337;\nprintln!(\"AETHER_RUST={}\", MAGIC);",
            &items,
            &tx,
        )
        .await
        .expect("Rust cell should compile and run");

        let mut lines = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let KernelEvent::CellStdout { line, .. } = ev {
                lines.push(line);
            }
        }
        assert_eq!(lines, vec!["AETHER_RUST=1337".to_string()]);
    }
}
