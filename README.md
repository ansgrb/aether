# Technical Specification and Implementation Blueprint: "RustyNote"

## 1. System Architecture Overview

The system acts as a custom Jupyter Kernel backend written entirely in Rust. It bypasses standard single-language constraints by implementing an internal **Kernel Multiplexer (Orchestrator)**. 

### 1.1 Process Topology
* **Client:** VS Code Notebook / JupyterLab (communicates via ZeroMQ).
* **Main Process (`rust-notebook-core`):**
  * **ZMQ Router:** Binds to 5 local ports (Shell, IOPub, Stdin, Control, Heartbeat).
  * **Interpreter Multiplexer:** Reads cell metadata/magic (`%%`).
  * **Embedded Python Runtime:** Runs inside the main process via `pyo3` (requires GIL acquisition per execution).
  * **Rust Context Manager:** Manages the `evcxr` evaluation loop and dynamic library loading.
* **Shared Memory Pool:** Apache Arrow C-Data Interface bridging `pyo3` and `evcxr`.

## 2. Network & Protocol Layer (Jupyter Messaging Protocol)

To trick the frontend into thinking it's talking to a standard kernel, the orchestrator must implement the Jupyter Wire Protocol via ZeroMQ.

### 2.1 Socket Configuration (using `zeromq` crate)
The kernel connection file (`kernel.json`) will provide 5 ports. The orchestrator must bind to them:
1. **Heartbeat (REQ/REP):** Simple echo to prove the kernel is alive.
2. **Shell (ROUTER/DEALER):** Receives code execution requests (`execute_request`).
3. **IOPub (PUB/SUB):** Broadcasts side-effects (stdout, stderr, rich display HTML/images, execution status).
4. **Control (ROUTER/DEALER):** High-priority messages (interrupt, shutdown).
5. **Stdin (ROUTER/DEALER):** Prompting user for input (optional for v1).

### 2.2 Wire Message Format
Every ZMQ message must be framed correctly using HMAC-SHA256 signing.
```rust
struct JupyterMessage {
    zmq_identities: Vec<Vec<u8>>, // Frame 0
    delimiter: String,            // Frame 1: "<IDS|MSG>"
    hmac_signature: String,       // Frame 2
    header: String,               // Frame 3: JSON
    parent_header: String,        // Frame 4: JSON
    metadata: String,             // Frame 5: JSON
    content: String,              // Frame 6: JSON (The actual payload)
}
```
*Actionable:* Implement a serialization/deserialization pipeline using `serde_json` and `hmac` crates to parse `execute_request` and emit `execute_reply` and `stream` (stdout).

## 3. Execution Pipelines

### 3.1 Rust Execution Pipeline (`evcxr`)
* **Dependency:** `evcxr` crate.
* **State Management:** The orchestrator must instantiate a single `evcxr::EvalContext` on startup and hold it in a `RwLock` or `Mutex`.
* **Execution Flow:**
  1. Frontend sends Rust code block.
  2. Orchestrator calls `eval_context.eval("code block")`.
  3. `evcxr` internally wraps the code in a function, compiles it via `rustc` to a `.so`/`.dylib` in a temp directory, and uses `libloading` to execute it.
  4. Capture stdout/stderr via a custom sink and pipe it to the ZMQ IOPub socket.

### 3.2 Python Execution Pipeline (`pyo3`)
* **Dependency:** `pyo3` with `auto-initialize` feature.
* **State Management:** Python state lives in the process memory.
* **Execution Flow:**
  1. Orchestrator detects `%%python`.
  2. Acquires the Python Global Interpreter Lock (GIL): `Python::with_gil(|py| { ... })`.
  3. Uses `py.run(code, None, Some(&locals))` to execute the cell.
  4. **Crucial:** Stdout must be redirected. Before executing, inject a Python script via `sys.stdout` replacement to capture outputs into a Rust buffer, then pipe to IOPub.

## 4. Cross-Language Zero-Copy Memory Bridge

This is the core differentiator. We will use the **Apache Arrow C-Data Interface** to pass DataFrames between Python and Rust.

### 4.1 Passing Data: Python -> Rust
1. **Python Cell:** User creates a Pandas/Polars dataframe and flags it for sharing.
   ```python
   import polars as pl
   df = pl.DataFrame({"a": [1, 2, 3]})
   # Magic function injected by orchestrator
   __share_to_rust__("my_df", df) 
   ```
2. **Orchestrator Intercept (`pyo3` -> Rust):**
   * The `__share_to_rust__` function is a Rust function exposed to Python via `pyo3`.
   * It calls the `.to_arrow()` equivalent in Python, extracting the FFI pointers (`FFI_ArrowArray` and `FFI_ArrowSchema`).
   * The orchestrator stores these raw memory addresses in a global `HashMap<String, (usize, usize)>`.
3. **Rust Cell:** 
   * The user runs a Rust cell to consume `my_df`.
   * The orchestrator automatically generates a preamble injecting the memory pointers into the `evcxr` context, reconstructing the `arrow_rs::RecordBatch` directly from the raw pointers using `arrow::ffi::from_ffi`.

## 5. Development Sprints for the Engineering Team

### Sprint 1: ZMQ Backbone & Protocol
* **Crates:** `zeromq`, `tokio`, `serde`, `serde_json`, `hmac`, `sha2`, `uuid`.
* **Deliverable:** A running process that connects to VS Code/Jupyter, responds to Heartbeats, successfully parses an `execute_request`, and sends back an `execute_reply` (even if hardcoded).

### Sprint 2: The Rust Context (`evcxr` Integration)
* **Crates:** `evcxr`.
* **Deliverable:** Hook up the `execute_request` content to `evcxr::EvalContext`. Successfully compile and run simple math in a notebook cell, retaining variables between cells. Send execution results back via IOPub `execute_result`.

### Sprint 3: The Python Context (`pyo3` Integration)
* **Crates:** `pyo3`.
* **Deliverable:** Implement the cell parser. Route cells starting with `%%python` to the `pyo3` runtime. Implement stdout capturing to ensure Python `print()` statements appear in the notebook UI.

### Sprint 4: The Arrow Bridge (FFI)
* **Crates:** `arrow`, `polars` (optional, for native DF support).
* **Deliverable:** Implement the FFI C-Data interface. Prove that a 1GB dataframe created in a `%%python` cell can be queried in a `%%rust` cell in < 5ms (proving zero-copy).

### Sprint 5: Output Rendering & Extensibility
* **Deliverable:** Support rich outputs. If a Rust cell evaluates to an object implementing a specific display trait (returning HTML or SVG, like a `plotters` chart), format the IOPub message metadata to render it graphically in the UI instead of as plain text.

## 6. Monorepo / AI Setup (Future Proofing)
* **Config Context:** Implement a `%%env` magic that parses TOML/JSON and sets environment variables within the main orchestrator process. Because both `pyo3` and `evcxr` inherit the main process environment, database connection strings established here instantly propagate to both language runtimes.