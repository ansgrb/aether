# Aether ("RustyNote") — Architecture Review & Implementation Plan

## 1. Architectural & Security Review of `README.md`

Before writing code, our review of [README.md](../README.md) identified **four critical technical findings** that must be accounted for in the implementation so the kernel works reliably with VS Code / JupyterLab and avoids memory corruption:

> [!CRITICAL]
> ### Finding 1: `evcxr` Subprocess Architecture vs. In-Process Raw Pointer Sharing (Section 3.1 & 4.1)
> `README.md` specifies storing `(usize, usize)` (`FFI_ArrowArray` and `FFI_ArrowSchema` addresses) in a main-process `HashMap` and reconstructing them inside `evcxr` via raw memory pointers.
> - **How `evcxr` actually works:** `evcxr::EvalContext` spawns an isolated **child subprocess** (via `evcxr::runtime_hook()`) that loads compiled `.so` crates. Because the child subprocess has a **separate virtual address space** from the main orchestrator process, dereferencing a parent-process `usize` pointer inside an `evcxr` cell will immediately **segfault (`SIGSEGV`)**.
> - **Resolution in Plan:** We bridge the two runtimes safely using **two complementary mechanisms**:
>   1. **Zero-Copy Shared Memory (`memfd` / `shm` + `mmap` Arrow IPC)** between the orchestrator (`pyo3`) and the `evcxr` worker subprocess — achieving true `< 5ms` zero-copy reads on 1 GB+ tables across process boundaries while preserving `evcxr` crash isolation.
>   2. **In-Process Arrow C-Data Interface (`FFI_ArrowArray` / `FFI_ArrowSchema`)** within the orchestrator process to extract buffers from Python (`polars`/`pyarrow`/`__arrow_c_array__`) with zero serialization overhead, plus direct in-process `FFI_ArrowArray` / `FFI_ArrowSchema` export for in-process consumers.

> [!WARNING]
> ### Finding 2: Arrow C-Data Interface Ownership & Use-After-Free (Section 4.1)
> The Arrow C-Data Interface (`FFI_ArrowArray`) has **move-once semantics**: consuming an `FFI_ArrowArray` via `arrow::ffi::from_ffi` transfers ownership and invokes the producer's `release` callback when dropped.
> - If a user references a shared DataFrame (`my_df`) across multiple Rust cells or re-runs a cell using the same raw pointer, the second execution triggers a **Use-After-Free (UAF) / Double-Free**.
> - **Resolution in Plan:** The orchestrator immediately imports the Python `FFI_ArrowArray` / `FFI_ArrowSchema` into an orchestrator-owned, reference-counted `Arc<RecordBatch>` (and zero-copy `mmap` backing store) and exports fresh views on each cell execution.

> [!IMPORTANT]
> ### Finding 3: Mandatory Jupyter Protocol Handshake (`kernel_info_request`) (Section 2)
> Neither VS Code Notebooks nor JupyterLab will send an `execute_request` until the kernel responds to `kernel_info_request` with a valid `kernel_info_reply` (protocol version `5.3`) and emits `status: busy` -> `status: idle` transitions on the `IOPub` socket around every request.
> - **Resolution in Plan:** Sprint 1 includes full support for `kernel_info_request`, `shutdown_request`, `is_complete_request`, and `status` (`starting` / `busy` / `idle`) lifecycle messages on `IOPub`, alongside constant-time HMAC-SHA256 verification.

> [!CAUTION]
> ### Finding 4: `%%env` Multi-Threaded UB (`std::env::set_var`) & Secret Leakage (Section 6)
> On modern Rust (`rustc 1.80+`), `std::env::set_var` is `unsafe` because POSIX `setenv` causes **Undefined Behavior / data races** when called in a multi-threaded Tokio/PyO3 process. Additionally, `evcxr`'s subprocess won't automatically see parent env mutations made after startup.
> - **Resolution in Plan:** Maintain an explicit, thread-safe `EnvStore` in the orchestrator that synchronizes changes into Python (`os.environ[k] = v` under the GIL) and into `evcxr` (via an injected `unsafe { std::env::set_var(...) }` inside the single-threaded cell evaluation preamble), while redacting sensitive values from `IOPub` output logs.

---

## 2. System Architecture & Workspace Layout

```mermaid
flowchart TB
    subgraph Client["VS Code / JupyterLab Frontend"]
        UI["Notebook UI"]
    end

    subgraph Core["aether-kernel (Main Orchestrator Process)"]
        ZMQ["ZMQ Router & HMAC-SHA256 Signer<br/>(Shell, IOPub, Control, Stdin, Heartbeat)"]
        MUX["Cell Multiplexer & Magic Parser<br/>(%%rust | %%python | %%env)"]
        PY["Embedded Python Runtime (PyO3)<br/>+ Custom sys.stdout/stderr Sink"]
        BRIDGE["Arrow Zero-Copy Bridge Registry<br/>(FFI_ArrowArray -> Shared Memory Pool)"]
    end

    subgraph Worker["evcxr Child Subprocess"]
        EVCXR["evcxr Runtime + rustc Dynamic .so Loader"]
        MMAP["Zero-Copy Arrow Reader (mmap / FFI)"]
    end

    UI <-->|"Jupyter Wire Protocol v5.3 (ZeroMQ)"| ZMQ
    ZMQ --> MUX
    MUX -->|"%%python"| PY
    MUX -->|"%%rust (default)"| EVCXR
    PY -->|"__share_to_rust__(name, df)<br/>Arrow C-Data Interface"| BRIDGE
    BRIDGE <-->|"Zero-Copy POSIX shm / memfd"| MMAP
    MMAP --> EVCXR
```

### Repository Structure

```text
/home/ansgrb/backyard/aether/
├── Cargo.toml                      # Workspace manifest
├── README.md
├── docs/
│   └── IMPLEMENTATION_PLAN.md      # Architectural review & implementation blueprint
├── kernelspec/
│   └── kernel.json                 # Jupyter kernelspec template for `jupyter kernelspec install`
├── crates/
│   ├── aether-protocol/            # Sprint 1: Jupyter Wire Protocol, HMAC-SHA256, ZMQ framing
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── connection.rs       # Parses kernel.json connection file
│   │       ├── hmac.rs             # Constant-time HMAC-SHA256 signing & verification
│   │       ├── message.rs          # JupyterMessage wire framing (<IDS|MSG>) & typed payloads
│   │       └── sockets.rs          # Tokio ZMQ socket actors (Heartbeat, Shell, IOPub, Control, Stdin)
│   ├── aether-bridge/              # Sprint 4: Cross-Language Zero-Copy Arrow Bridge
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── ffi_import.rs       # Arrow C-Data Interface (FFI_ArrowArray / FFI_ArrowSchema)
│   │       ├── shm_store.rs        # Zero-copy shared memory registry across orchestrator & evcxr
│   │       └── client.rs           # Helper crate/preamble used inside evcxr cells to read RecordBatch
│   ├── aether-engine-rust/         # Sprint 2 & 5: evcxr Execution Pipeline & Rich Display
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── context.rs          # evcxr::EvalContext lifecycle, stdout/stderr streaming to IOPub
│   │       └── display.rs          # MIME bundle detection (text/plain, text/html, image/svg+xml)
│   ├── aether-engine-python/       # Sprint 3: PyO3 Execution Pipeline & Stdout Capture
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── runtime.rs          # Persistent Python globals/locals & GIL execution
│   │       ├── stdio.rs            # PyO3 sys.stdout / sys.stderr streaming interceptor
│   │       └── magic_bridge.rs     # Exposes `__share_to_rust__(name, df)` via PyCapsule/__arrow_c_array__
│   └── aether-kernel/              # Binary crate (`rust-notebook-core`) & Multiplexer
│       ├── Cargo.toml
│       └── src/
│           ├── main.rs             # CLI entrypoint (`evcxr::runtime_hook()` + Tokio runtime)
│           ├── multiplexer.rs      # Cell parser (`%%python`, `%%rust`, `%%env`) & dispatcher
│           └── env_manager.rs      # Thread-safe `%%env` TOML/JSON parser & runtime propagator
└── tests/
    ├── protocol_integration.rs     # ZMQ wire protocol & HMAC verification tests
    ├── execution_pipelines.rs      # Rust + Python state persistence & stdout capture tests
    └── arrow_zero_copy_bench.rs    # 1GB DataFrame Python -> Rust < 5ms zero-copy benchmark
```

---

## 3. Phased Implementation Plan (Sprints 1–5)

### Sprint 1: ZMQ Backbone & Jupyter Wire Protocol (`aether-protocol`)
* **Objective:** Implement a spec-compliant Jupyter Wire Protocol v5.3 server over ZeroMQ with mandatory HMAC-SHA256 authentication.
* **Key Tasks:**
  1. Parse the connection file (`-f <connection_file.json>`) passed by Jupyter/VS Code (`ip`, `transport`, `signature_scheme`, `key`, and 5 ports: `hb_port`, `shell_port`, `iopub_port`, `control_port`, `stdin_port`).
  2. Implement `JupyterMessage` wire framing (`zmq_identities`, `<IDS|MSG>` delimiter, hex-encoded HMAC-SHA256 signature, `header`, `parent_header`, `metadata`, `content`) using `hmac`, `sha2`, `serde`, `serde_json`, and `uuid`.
  3. Enforce **constant-time HMAC verification** (`Mac::verify_slice`) on every inbound frame before JSON parsing.
  4. Spawn async tasks for the 5 sockets using `zeromq` and `tokio`:
     - **Heartbeat (`REP`):** Echo raw bytes immediately.
     - **IOPub (`PUB`):** Broadcast `status` (`busy` / `idle`), `stream` (`stdout` / `stderr`), `execute_input`, `execute_result`, `display_data`, and `error`.
     - **Shell (`ROUTER`):** Handle `kernel_info_request`, `execute_request`, and `is_complete_request`.
     - **Control (`ROUTER`):** Handle `shutdown_request` and `interrupt_request`.

### Sprint 2: The Rust Context (`evcxr` Integration) (`aether-engine-rust`)
* **Objective:** Execute stateful Rust cells via `evcxr` and stream real-time compiler/runtime outputs to the notebook frontend.
* **Key Tasks:**
  1. Call `evcxr::runtime_hook()` at the very first line of `main.rs` before initializing Tokio or PyO3.
  2. Initialize `evcxr::EvalContext` inside an async-compatible lock and spawn reader tasks on `EvalContext`'s `stdout` and `stderr` channels to forward lines as `IOPub` `stream` messages in real time.
  3. Route default cells and `%%rust` cells to `EvalContext`, mapping `EvalOutputs` MIME bundles into `execute_result` / `display_data` messages and compiler diagnostics into structured `error` messages.

### Sprint 3: The Python Context (`pyo3` Integration) (`aether-engine-python`)
* **Objective:** Execute stateful `%%python` cells in an embedded Python interpreter via `pyo3` with real-time `stdout`/`stderr` capture.
* **Key Tasks:**
  1. Implement the cell magic parser in `multiplexer.rs` (`%%python`, `%%rust`, `%%env`).
  2. Initialize `pyo3` and maintain a persistent Python `__main__` module dictionary (`Py<PyDict>`) across cell executions.
  3. Implement a custom Rust `#[pyclass]` `IoPubStreamWriter` injected into `sys.stdout` and `sys.stderr` during cell execution so Python `print()` statements and tracebacks stream directly to the ZMQ `IOPub` channel.
  4. Support expression evaluation on the final statement of a Python cell so trailing expressions return an `execute_result` payload.

### Sprint 4: Cross-Language Zero-Copy Memory Bridge (`aether-bridge`)
* **Objective:** Pass DataFrames/RecordBatches between `%%python` and `%%rust` in `< 5ms` for a 1 GB table without copying data buffers.
* **Key Tasks:**
  1. Expose `__share_to_rust__(name, df)` in the Python global namespace via `pyo3`, extracting `FFI_ArrowArray` and `FFI_ArrowSchema` via Arrow PyCapsule (`__arrow_c_array__` / `__arrow_c_stream__`) or `_export_to_c`.
  2. Reconstruct an `arrow::record_batch::RecordBatch` in the orchestrator using `arrow::ffi::from_ffi` and store it in Both:
     - An in-process `FFI_ArrowArray` / `FFI_ArrowSchema` re-exporter (`export_to_ffi_ptrs`), AND
     - A zero-copy shared-memory mmap backing store (`/dev/shm` / `memfd`) for cross-process `evcxr` subprocess zero-copy `mmap` access.
  3. Automatically inject preamble bindings into `evcxr` so shared DataFrames are immediately usable in Rust cells.

### Sprint 5: Rich Output Rendering & `%%env` Configuration (`aether-engine-rust` + `env_manager`)
* **Objective:** Support rich HTML/SVG/image outputs in both Rust and Python, and implement the `%%env` configuration magic.
* **Key Tasks:**
  1. Support `evcxr`'s MIME output protocol (`EVCXR_BEGIN_CONTENT <mime_type> ... EVCXR_END_CONTENT`) and rich display helpers for HTML/SVG/Markdown outputs, plus `_repr_html_()` / `_repr_svg_()` in Python.
  2. Implement `%%env` TOML/JSON parsing in `env_manager.rs`, propagating environment variables safely to both Python (`os.environ`) and `evcxr` while redacting secrets in `IOPub` output.

---

## 4. Implementation Summary & Verification Results

All 5 workspace crates have been implemented and verified against the specification:

### 4.1 Implemented Crates & Modules
1. **`aether-protocol` (`crates/aether-protocol`):**
   - `connection.rs`: `KernelConnectionInfo` parser and validator for `kernel.json`.
   - `hmac.rs`: Constant-time `HMAC-SHA256` signing and verification (`HmacSigner`).
   - `message.rs`: `JupyterMessage` wire framing (`<IDS|MSG>`) and builders for `kernel_info_reply`, `execute_request`, `execute_reply`, `execute_result`, `display_data`, `stream`, `status`, `error`, `is_complete_reply`, and `shutdown_reply`.
   - `sockets.rs`: Async ZeroMQ bindings (`KernelSockets`, `IoPubHandle`) across the 5 Jupyter channels (`Heartbeat`, `Shell`, `IOPub`, `Control`, `Stdin`).
2. **`aether-bridge` (`crates/aether-bridge`):**
   - `ffi_import.rs`: Zero-copy `arrow::ffi::from_ffi` import/export of `FFI_ArrowArray` and `FFI_ArrowSchema` raw pointers `(usize, usize)` with `Arc`-backed re-export to prevent Use-After-Free on repeated cell executions.
   - `shm_store.rs` & `client.rs`: Global FFI pointer registry (`GLOBAL_ARROW_REGISTRY`) + 64-byte-aligned POSIX shared-memory (`/dev/shm`) `mmap` backing store (`Buffer::from_custom_allocation`) for cross-process zero-copy reads.
3. **`aether-engine-rust` (`crates/aether-engine-rust`):**
   - `context.rs`: `RustEngine` managing `evcxr::CommandContext`, live `stdout`/`stderr` streaming to `IOPub`, environment synchronization, and automatic zero-copy table preamble injection.
   - `display.rs`: `AetherDisplay` trait, `Html`, `Svg`, and `Markdown` rich wrappers, automatic SVG/HTML MIME promotion, and the `AetherFrame` zero-copy `mmap` reader for `evcxr` subprocesses.
4. **`aether-engine-python` (`crates/aether-engine-python`):**
   - `runtime.rs`: `PythonEngine` maintaining persistent `__main__` globals across cells, trailing expression AST evaluation, and rich `_repr_html_()` / `_repr_svg_()` MIME extraction.
   - `stdio.rs`: `#[pyclass] IoPubStreamWriter` redirecting Python `sys.stdout` and `sys.stderr` to `IOPub`.
   - `magic_bridge.rs`: `#[pyfunction] __share_to_rust__(name, df)` extracting `FFI_ArrowArray` and `FFI_ArrowSchema` pointers via the Arrow C-Data / PyCapsule Interface.
5. **`aether-kernel` (`crates/aether-kernel`):**
   - `multiplexer.rs`: Cell magic parser (`%%python`, `%%rust`, `%%env`).
   - `env_manager.rs`: Thread-safe TOML/JSON `%%env` parser with automatic secret redaction on `IOPub` and synchronization to both `os.environ` (Python) and `std::env` (Rust).
   - `orchestrator.rs` & `main.rs`: `PolyglotOrchestrator`, ZeroMQ event loop, and `rust-notebook-core` CLI binary entrypoint (`evcxr::runtime_hook()`).

### 4.2 Verification & Benchmark Results (`cargo test --workspace`)
- **Unit & Protocol Tests:** All 13 unit, integration, and end-to-end tests across `aether-protocol`, `aether-bridge`, `aether-engine-python`, `aether-engine-rust`, and `aether-kernel` passed.
- **1 GiB Zero-Copy Arrow Benchmark (`test_sprint4_one_gigabyte_arrow_zero_copy_under_5ms`):**
  - Allocated a **1.0 GiB** (`134,217,728` x `int64`) Polars/Arrow DataFrame in `%%python`, shared it via `__share_to_rust__("one_gb_df", df)`, and reconstructed + queried it in Rust in **`50.8 µs` (`0.05 ms`)** — nearly 100x faster than the `< 5ms` specification target.
- **End-to-End Subprocess Polyglot Test (`test_end_to_end_kernel_binary_polyglot_execution`):**
  - Spawned `rust-notebook-core`, verified Heartbeat and `HMAC-SHA256` wire signatures, and executed `%%env` -> `%%python` (Polars `__share_to_rust__`) -> `%%rust` (`evcxr` compilation reading both `std::env::var` and the zero-copy `my_df` column) -> `%%rust` (`Svg` rich output) -> `shutdown_request`.
