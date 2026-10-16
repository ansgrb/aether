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

---

## 7. Workspace Architecture & Crate Layout

The implementation is organized as a 5-crate Cargo workspace (see [docs/IMPLEMENTATION_PLAN.md](docs/IMPLEMENTATION_PLAN.md) and [docs/IMPLEMENTATION_SUMMARY.md](docs/IMPLEMENTATION_SUMMARY.md) for deep-dive architectural details):

```text
aether/
├── Cargo.toml                      # Workspace root
├── docs/
│   ├── IMPLEMENTATION_PLAN.md      # Architecture review & sprint blueprint
│   └── IMPLEMENTATION_SUMMARY.md   # Implementation summary & benchmark results
├── kernelspec/
│   └── kernel.json                 # Jupyter kernelspec definition
└── crates/
    ├── aether-protocol/            # Jupyter Wire Protocol v5.3, HMAC-SHA256, ZMQ sockets
    ├── aether-bridge/              # Zero-copy Arrow C-Data Interface & /dev/shm mmap store
    ├── aether-engine-rust/         # Stateful evcxr pipeline & rich MIME display (Html, Svg, Markdown)
    ├── aether-engine-python/       # Embedded PyO3 pipeline, stdout capture, __share_to_rust__
    └── aether-kernel/              # Binary (`rust-notebook-core`), cell multiplexer, %%env manager
```

---

## 8. Prerequisites & Installation

### 8.1 Prerequisites
* **Rust Toolchain:** `rustc >= 1.85` (tested on `rustc 1.99.0`) with the `rust-src` component installed for `evcxr`:
  ```bash
  rustup component add rust-src
  ```
* **Python Runtime:** Python `>= 3.11` with shared library support (`libpython3.x.so`) and `uv` (recommended) or `venv`.

### 8.2 Set Up Python Environment & Build the Kernel
```bash
# 1. Create the local virtual environment and install Polars & PyArrow
uv venv .venv
uv pip install --python .venv/bin/python polars pyarrow

# 2. Build the kernel binary (`rust-notebook-core`)
cargo build --release --bin rust-notebook-core
```

### 8.3 Register the Kernel with Jupyter / VS Code
Install the kernelspec so JupyterLab and VS Code Notebooks discover **Aether (RustyNote)**:
```bash
# Point kernelspec/kernel.json to the compiled binary or add target/release to PATH
export PATH="$PWD/target/release:$PATH"
jupyter kernelspec install ./kernelspec --user --name aether
```
*(Alternatively, replace `"rust-notebook-core"` in `kernelspec/kernel.json` with the absolute path `"/path/to/aether/target/release/rust-notebook-core"` before running `jupyter kernelspec install`.)*

---

## 9. Usage Guide & Notebook Cell Magics

### 9.1 Running Directly
Jupyter or VS Code launches `rust-notebook-core` automatically using the `-f <connection_file.json>` flag:
```bash
./target/release/rust-notebook-core -f /path/to/kernel-connection.json
```

### 9.2 Setting Shared Environment Variables (`%%env`)
Define environment variables in **TOML** or **JSON** that automatically propagate to both the Python (`os.environ`) and Rust (`std::env::var`) runtimes. Sensitive keys (`*SECRET*`, `*PASSWORD*`, `*TOKEN*`, `*KEY*`, `*URL*`, `*DSN*`) are automatically redacted in notebook output:
```toml
%%env
APP_ENV = "staging"
DATABASE_URL = "postgres://user:pass@localhost:5432/analytics"

[cluster]
WORKERS = 8
API_TOKEN = "secret-token-value"
```

### 9.3 Executing Python Cells & Sharing DataFrames (`%%python`)
Prefix a cell with `%%python` (or `%%py`) to execute code inside the embedded `pyo3` runtime. Call `__share_to_rust__(name, df)` on any Polars `DataFrame` or PyArrow `Table`/`RecordBatch` to expose it to Rust via zero-copy memory:
```python
%%python
import os
import polars as pl

print("Running in env:", os.environ["APP_ENV"])

df = pl.DataFrame({
    "id": [1, 2, 3, 4],
    "revenue": [100.5, 250.0, 75.25, 400.0]
})

# Zero-copy export to the Aether Arrow bridge
__share_to_rust__("sales_df", df)
df
```

### 9.4 Consuming Zero-Copy DataFrames in Rust (`%%rust` or default)
Cells without a magic header (or prefixed with `%%rust`) execute inside `evcxr`. Any table registered via `__share_to_rust__` is automatically injected into scope when referenced:
```rust
%%rust
let total_rev: f64 = sales_df.col_f64("revenue").iter().sum();
let rows = sales_df.num_rows();
println!("Processed {rows} rows with zero copies!");
total_rev
```
For native Rust crates linking against `aether-bridge`, full `arrow_array::RecordBatch` reconstruction is available in `< 0.1ms` via:
```rust
let batch: arrow_array::RecordBatch = aether_bridge::load_shared_record_batch("sales_df")?;
```

### 9.5 Rich Graphical Output (`Svg`, `Html`, `Markdown`)
Return `Svg(...)`, `Html(...)`, or `Markdown(...)` (or any custom type implementing `AetherDisplay`) from the end of a Rust cell to render rich graphical output in the notebook UI:
```rust
%%rust
Svg(r#"<svg width="160" height="80" xmlns="http://www.w3.org/2000/svg">
  <rect width="160" height="80" rx="12" fill="#1e293b"/>
  <text x="80" y="45" fill="#38bdf8" font-family="monospace" font-size="16" text-anchor="middle">Aether Zero-Copy</text>
</svg>"#)
```

---

## 10. Running Tests & the 1 GiB Zero-Copy Benchmark

Run the full test suite (unit tests, ZeroMQ HMAC wire protocol tests, end-to-end polyglot binary test, and the 1 GiB Python $\rightarrow$ Rust zero-copy benchmark):
```bash
cargo test --workspace -- --nocapture
```

To run only the **1 GiB Zero-Copy Arrow Benchmark** (`< 5ms` requirement, typically `~50 µs`):
```bash
cargo test -p aether-kernel --test protocol_and_pipelines test_sprint4_one_gigabyte_arrow_zero_copy_under_5ms -- --nocapture
```