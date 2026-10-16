# Aether — Polyglot Zero-Copy Rust & Python Jupyter Kernel

[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](https://www.gnu.org/licenses/gpl-3.0)
[![Rust](https://img.shields.io/badge/Rust-1.85%2B-orange.svg?logo=rust)](https://www.rust-lang.org/)
[![Python](https://img.shields.io/badge/Python-3.11%2B-3776AB.svg?logo=python&logoColor=white)](https://www.python.org/)
[![Apache Arrow](https://img.shields.io/badge/Apache%20Arrow-Zero--Copy%20FFI-D22128.svg?logo=apache)](https://arrow.apache.org/)
[![Jupyter Protocol](https://img.shields.io/badge/Jupyter%20Wire%20Protocol-v5.3-F37626.svg?logo=jupyter)](https://jupyter-client.readthedocs.io/)
[![GitHub Portfolio](https://img.shields.io/badge/Author-%40ansgrb-181717.svg?logo=github)](https://github.com/ansgrb)

> **Aether** (originally codenamed _"RustyNote"_) is a high-performance, polyglot Jupyter Kernel written in Rust. It multiplexes **Rust (`evcxr`)**, **Python (`pyo3`)**, and **Environment (`%%env`)** cells inside a single unified kernel session and bridges **Polars / PyArrow DataFrames** between Python and Rust in **~50 microseconds (`0.05 ms` for 1 GiB)** with **zero memory copies** via the Apache Arrow C-Data Interface and POSIX shared memory (`mmap`).

---

## Table of Contents

1. [System Architecture Overview](#1-system-architecture-overview)
2. [Network & Protocol Layer (Jupyter Messaging Protocol)](#2-network--protocol-layer-jupyter-messaging-protocol)
3. [Execution Pipelines](#3-execution-pipelines)
4. [Cross-Language Zero-Copy Memory Bridge](#4-cross-language-zero-copy-memory-bridge)
5. [Development Sprints for the Engineering Team](#5-development-sprints-for-the-engineering-team)
6. [Monorepo / AI Setup (Future Proofing)](#6-monorepo--ai-setup-future-proofing)
7. [Workspace Architecture & Crate Layout](#7-workspace-architecture--crate-layout)
8. [Prerequisites & Installation](#8-prerequisites--installation)
9. [Usage Guide & Notebook Cell Magics](#9-usage-guide--notebook-cell-magics)
10. [Running Tests & the 1 GiB Zero-Copy Benchmark](#10-running-tests--the-1-gib-zero-copy-benchmark)
11. [Project Status & Roadmap (What's Done vs. What's Due)](#11-project-status--roadmap-whats-done-vs-whats-due)
12. [Contributing Guidelines & Rules](#12-contributing-guidelines--rules)
13. [Credits & Acknowledgments](#13-credits--acknowledgments)
14. [Author & Contact](#14-author--contact)
15. [License (GNU GPL v3.0)](#15-license-gnu-gpl-v30)

---

## 1. System Architecture Overview

The system acts as a custom Jupyter Kernel backend written entirely in Rust. It bypasses standard single-language constraints by implementing an internal **Kernel Multiplexer (Orchestrator)**.

### 1.1 Process Topology

- **Client:** VS Code Notebook / JupyterLab (communicates via ZeroMQ).
- **Main Process (`rust-notebook-core`):**
  - **ZMQ Router:** Binds to 5 local ports (Shell, IOPub, Stdin, Control, Heartbeat).
  - **Interpreter Multiplexer:** Reads cell metadata/magic (`%%`).
  - **Embedded Python Runtime:** Runs inside the main process via `pyo3` (requires GIL acquisition per execution).
  - **Rust Context Manager:** Manages the `evcxr` evaluation loop and dynamic library loading.
- **Shared Memory Pool:** Apache Arrow C-Data Interface bridging `pyo3` and `evcxr`.

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

_Actionable:_ Implement a serialization/deserialization pipeline using `serde_json` and `hmac` crates to parse `execute_request` and emit `execute_reply` and `stream` (stdout).

## 3. Execution Pipelines

### 3.1 Rust Execution Pipeline (`evcxr`)

- **Dependency:** `evcxr` crate.
- **State Management:** The orchestrator must instantiate a single `evcxr::EvalContext` on startup and hold it in a `RwLock` or `Mutex`.
- **Execution Flow:**
  1. Frontend sends Rust code block.
  2. Orchestrator calls `eval_context.eval("code block")`.
  3. `evcxr` internally wraps the code in a function, compiles it via `rustc` to a `.so`/`.dylib` in a temp directory, and uses `libloading` to execute it.
  4. Capture stdout/stderr via a custom sink and pipe it to the ZMQ IOPub socket.

### 3.2 Python Execution Pipeline (`pyo3`)

- **Dependency:** `pyo3` with `auto-initialize` feature.
- **State Management:** Python state lives in the process memory.
- **Execution Flow:**
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
   - The `__share_to_rust__` function is a Rust function exposed to Python via `pyo3`.
   - It calls the `.to_arrow()` equivalent in Python, extracting the FFI pointers (`FFI_ArrowArray` and `FFI_ArrowSchema`).
   - The orchestrator stores these raw memory addresses in a global `HashMap<String, (usize, usize)>`.
3. **Rust Cell:**
   - The user runs a Rust cell to consume `my_df`.
   - The orchestrator automatically generates a preamble injecting the memory pointers into the `evcxr` context, reconstructing the `arrow_rs::RecordBatch` directly from the raw pointers using `arrow::ffi::from_ffi`.

## 5. Development Sprints for the Engineering Team

### Sprint 1: ZMQ Backbone & Protocol

- **Crates:** `zeromq`, `tokio`, `serde`, `serde_json`, `hmac`, `sha2`, `uuid`.
- **Deliverable:** A running process that connects to VS Code/Jupyter, responds to Heartbeats, successfully parses an `execute_request`, and sends back an `execute_reply` (even if hardcoded).

### Sprint 2: The Rust Context (`evcxr` Integration)

- **Crates:** `evcxr`.
- **Deliverable:** Hook up the `execute_request` content to `evcxr::EvalContext`. Successfully compile and run simple math in a notebook cell, retaining variables between cells. Send execution results back via IOPub `execute_result`.

### Sprint 3: The Python Context (`pyo3` Integration)

- **Crates:** `pyo3`.
- **Deliverable:** Implement the cell parser. Route cells starting with `%%python` to the `pyo3` runtime. Implement stdout capturing to ensure Python `print()` statements appear in the notebook UI.

### Sprint 4: The Arrow Bridge (FFI)

- **Crates:** `arrow`, `polars` (optional, for native DF support).
- **Deliverable:** Implement the FFI C-Data interface. Prove that a 1GB dataframe created in a `%%python` cell can be queried in a `%%rust` cell in < 5ms (proving zero-copy).

### Sprint 5: Output Rendering & Extensibility

- **Deliverable:** Support rich outputs. If a Rust cell evaluates to an object implementing a specific display trait (returning HTML or SVG, like a `plotters` chart), format the IOPub message metadata to render it graphically in the UI instead of as plain text.

## 6. Monorepo / AI Setup (Future Proofing)

- **Config Context:** Implement a `%%env` magic that parses TOML/JSON and sets environment variables within the main orchestrator process. Because both `pyo3` and `evcxr` inherit the main process environment, database connection strings established here instantly propagate to both language runtimes.

---

## 7. Workspace Architecture & Crate Layout

The implementation is organized as a 5-crate Cargo workspace (see [docs/IMPLEMENTATION_PLAN.md](docs/IMPLEMENTATION_PLAN.md) and [docs/IMPLEMENTATION_SUMMARY.md](docs/IMPLEMENTATION_SUMMARY.md) for deep-dive architectural details):

```text
aether/
├── Cargo.toml                      # Workspace root
├── LICENSE                         # GNU General Public License v3.0
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

- **Rust Toolchain:** `rustc >= 1.85` (tested on `rustc 1.99.0`) with the `rust-src` component installed for `evcxr`:
  ```bash
  rustup component add rust-src
  ```
- **Python Runtime:** Python `>= 3.11` with shared library support (`libpython3.x.so`) and `uv` (recommended) or `venv`.

### 8.2 Set Up Python Environment & Build the Kernel

```bash
# 1. Clone the repository
git clone https://github.com/ansgrb/aether.git
cd aether

# 2. Create the local virtual environment and install Polars & PyArrow
uv venv .venv
uv pip install --python .venv/bin/python polars pyarrow

# 3. Build the kernel binary (`rust-notebook-core`)
cargo build --release --bin rust-notebook-core
```

### 8.3 Register the Kernel with Jupyter / VS Code

Install the kernelspec so JupyterLab and VS Code Notebooks discover **Aether**:

```bash
# Point kernelspec/kernel.json to the compiled binary or add target/release to PATH
export PATH="$PWD/target/release:$PATH"
jupyter kernelspec install ./kernelspec --user --name aether
```

_(Alternatively, replace `"rust-notebook-core"` in `kernelspec/kernel.json` with the absolute path `"/path/to/aether/target/release/rust-notebook-core"` before running `jupyter kernelspec install`.)_

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

To run only the **1 GiB Zero-Copy Arrow Benchmark** (`< 5ms` requirement, measured at `~50.8 µs`):

```bash
cargo test -p aether-kernel --test protocol_and_pipelines test_sprint4_one_gigabyte_arrow_zero_copy_under_5ms -- --nocapture
```

---

## 11. Project Status & Roadmap (What's Done vs. What's Due)

We welcome community contributors! Below is the current implementation status across all subsystems and what is open for contribution:

| Area / Feature                                                        | Crate                           |      Status       | Notes / Benchmark                                                                                  |
| :-------------------------------------------------------------------- | :------------------------------ | :---------------: | :------------------------------------------------------------------------------------------------- |
| **Jupyter Wire Protocol v5.3 (`<IDS\|MSG>`)**                         | `aether-protocol`               |    ✅ **Done**    | Full multipart framing over ZeroMQ (`Shell`, `IOPub`, `Control`, `Stdin`, `Heartbeat`).            |
| **Constant-Time `HMAC-SHA256` Auth**                                  | `aether-protocol`               |    ✅ **Done**    | Enforced on all inbound frames via `hmac::Mac::verify_slice`.                                      |
| **Kernel Handshake & Lifecycle**                                      | `aether-protocol`               |    ✅ **Done**    | `kernel_info_request`, `execute_request`, `is_complete_request`, `shutdown_request`, `status`.     |
| **Stateful Rust Execution (`evcxr`)**                                 | `aether-engine-rust`            |    ✅ **Done**    | Persistent variables/functions across cells with live `stdout`/`stderr` streaming to `IOPub`.      |
| **Embedded Python Execution (`pyo3`)**                                | `aether-engine-python`          |    ✅ **Done**    | Persistent `__main__` globals, AST trailing-expression evaluation, and `IoPubStreamWriter`.        |
| **Python $\rightarrow$ Rust Zero-Copy Arrow Bridge**                  | `aether-bridge`                 |    ✅ **Done**    | `FFI_ArrowArray` + POSIX `/dev/shm` `mmap`. **1 GiB handoff + query in `50.8 µs` (`0.05 ms`)**.    |
| **Rich MIME Rendering (`Html`, `Svg`, `Markdown`)**                   | `aether-engine-rust` / `python` |    ✅ **Done**    | `AetherDisplay` trait in Rust + `_repr_html_()` / `_repr_svg_()` in Python.                        |
| **Polyglot `%%env` Config & Secret Redaction**                        | `aether-kernel`                 |    ✅ **Done**    | TOML/JSON parsing synced to `os.environ` & `std::env` with automatic secret redaction.             |
| **Reverse Bridge: Rust $\rightarrow$ Python (`__share_to_python__`)** | `aether-bridge` / `python`      | 🚧 **Due (Open)** | Export Rust `RecordBatch` / Polars `DataFrame` back into Python globals via PyCapsule.             |
| **Code Completion & Hover (`complete_request` / `inspect_request`)**  | `aether-kernel`                 | 🚧 **Due (Open)** | Wire `evcxr::CommandContext::completions` and Python `rlcompleter`/`jedi` to Shell socket.         |
| **Interactive Stdin (`input_request` / `input_reply`)**               | `aether-protocol`               | 🚧 **Due (Open)** | Wire the bound `Stdin` (`ROUTER`) socket for interactive `input()` prompts in notebooks.           |
| **Cross-Platform Shared Memory (macOS / Windows)**                    | `aether-bridge`                 | 🚧 **Due (Open)** | Extend `/dev/shm` backing store with POSIX `shm_open` on macOS and Named File Mappings on Windows. |
| **String / Nested List Zero-Copy Views in `AetherFrame`**             | `aether-engine-rust`            | 🚧 **Due (Open)** | Add zero-copy UTF-8 string offset views (`col_str`) alongside `col_i64` and `col_f64`.             |
| **SQL Cell Magic (`%%sql` over Arrow / DuckDB / Polars SQL)**         | `aether-kernel`                 |  💡 **Planned**   | Execute SQL queries directly against shared Arrow tables registered in `aether-bridge`.            |

---

## 12. Contributing Guidelines & Rules

We are excited to have you contribute to **Aether**! To keep the codebase fast, memory-safe, and reliable, please follow these engineering rules when submitting a Pull Request:

1. **Zero-Copy & Memory Safety First (`unsafe` Discipline):**
   - Any `unsafe` block touching `FFI_ArrowArray`, `FFI_ArrowSchema`, `mmap`, or `pyo3` FFI pointers **must** include a `// SAFETY:` comment explaining pointer provenance, alignment, and ownership lifetimes.
   - Never expose raw unvalidated `usize` pointers to user-supplied strings; always validate identifiers with `aether_bridge::validate_identifier` and ensure repeated cell runs cannot trigger Use-After-Free (UAF) or double-free.
2. **Preserve the 5-Crate Modular Boundary:**
   - Keep wire-protocol logic in `aether-protocol`, Arrow FFI/shared-memory logic in `aether-bridge`, language runtimes in `aether-engine-rust` and `aether-engine-python`, and orchestration in `aether-kernel`.
3. **Security & Secret Hygiene:**
   - Never log or broadcast unredacted credentials, tokens, or connection strings on `IOPub` or `tracing` logs.
   - All incoming ZMQ frames must pass constant-time `HMAC-SHA256` verification before deserialization.
4. **Testing & Benchmark Gate:**
   - Every PR must pass `cargo test --workspace` with zero warnings in workspace crates.
   - Changes to `aether-bridge` must maintain the `< 5ms` (sub-millisecond target) zero-copy benchmark on the 1 GiB test (`test_sprint4_one_gigabyte_arrow_zero_copy_under_5ms`).
5. **Pull Request Workflow:**
   - Fork the repository and create a feature branch (`git checkout -b feat/your-feature-name`).
   - Format code with `cargo fmt --all` and check lints with `cargo clippy --workspace`.
   - Open a Pull Request against `main` with a clear description of the architectural change and test coverage.

---

## 13. Credits & Acknowledgments

**Aether** was conceived, architected, and engineered by **Anas Ghareib ([@ansgrb](https://github.com/ansgrb))** and stands on the shoulders of awesome open-source projects in the Rust, Python, and data engineering ecosystems:

- **[evcxr](https://github.com/evcxr/evcxr)** — For pioneering interactive Rust evaluation and dynamic crate compilation.
- **[PyO3](https://github.com/PyO3/pyo3)** — For ergonomic, zero-overhead Rust $\leftrightarrow$ Python bindings.
- **[Apache Arrow (`arrow-rs`)](https://github.com/apache/arrow-rs)** & **[Polars](https://github.com/pola-rs/polars)** — For the columnar memory specification and C-Data Interface enabling zero-copy cross-language analytics.
- **[ZeroMQ (`zeromq-rs`)](https://github.com/zeromq/zmq.rs)** & **[Project Jupyter](https://jupyter.org/)** — For the asynchronous messaging backbone and interactive notebook protocol.

---

## 14. Author & Contact

**Anas Ghareib**  
_Principal Systems & Cybersecurity Architect / Software Engineer_

- **GitHub Portfolio:** [https://github.com/ansgrb](https://github.com/ansgrb)
- **Project Repository:** [https://github.com/ansgrb/aether](https://github.com/ansgrb/aether)
- **Email:** [ansgrb@gmail.com](mailto:ansgrb@gmail.com)
- **Issues & Feature Requests:** [https://github.com/ansgrb/aether/issues](https://github.com/ansgrb/aether/issues)

---

## 15. License (GNU GPL v3.0)

Copyright (C) 2026 **Anas Ghareib** (<ansgrb@gmail.com>).

**Aether** is free and open-source software licensed under the **GNU General Public License v3.0 (or later)** (`GPL-3.0-or-later`). You are free to use, study, share, and modify this software under the terms of the GNU GPL v3.0. See the [LICENSE](LICENSE) file or [https://www.gnu.org/licenses/gpl-3.0.html](https://www.gnu.org/licenses/gpl-3.0.html) for the full license text.
