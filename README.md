<div align="center">

# ⚡ AETHER

### **Rust-Native Multi-Kernel Notebook & Binary Analysis Cockpit**

[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](https://www.gnu.org/licenses/gpl-3.0)
[![Rust Edition](https://img.shields.io/badge/Rust-2021-orange.svg?logo=rust)](https://www.rust-lang.org/)
[![UI: egui / eframe](https://img.shields.io/badge/UI-egui%20%2F%20eframe-00ff66.svg)](https://github.com/emilk/egui)
[![Analysis: Goblin & Capstone](https://img.shields.io/badge/Binary%20Triage-Goblin%20%2B%20Capstone-blueviolet.svg)](#)
[![PRs Welcome](https://img.shields.io/badge/PRs-welcome-brightgreen.svg)](#contributing-guidelines)

_A local-first, low-footprint operations workspace combining polyglot interactive notebook execution (`Rust`, `Python 3`, `Bash`) with pure-Rust static binary triage, `Capstone` disassembly, and real-time hardware telemetry._

[**Architecture**](#-system-architecture) • [**Quick Start**](#-quick-start) • [**Roadmap**](#-project-status--roadmap) • [**Contributing**](#-contributing-guidelines) • [**Author & Credits**](#-credits--author)

</div>

---

## 📖 Overview

Traditional notebook environments rely on multi-layered browser stacks, WebSocket servers, and heavy Electron/Chromium runtimes that introduce latency and fragility during security research and reverse-engineering workflows.

**Aether** takes a pure-Rust, systems-first approach:

1. **Immediate-Mode Native Cockpit (`egui` / `eframe`):** Renders directly via OpenGL/Glow with zero browser or WebView overhead, organized into a 4-zone operational layout:
   - **Top Telemetry Bar:** Real-time CPU utilization, core count, and memory metrics (`sysinfo`) alongside global kernel controls (`Run All`, `Reset Kernels`).
   - **Left Target & Cell Tree:** Binary sample selector with quick presets, notebook cell status navigator, and JSON mission persistence (`Save` / `Load`).
   - **Center Polyglot Notebook:** Interactive cells supporting **Rust (`rustc` / `evcxr`)**, **Python 3** (stateful persistent session), and **Bash** (stateful shell session) with real-time line streaming and instant process abort.
   - **Right Binary Triage & Disassembly Stack:** Static inspection of `ELF`, `PE32/PE32+`, and `Mach-O` binaries powered by `goblin` and `capstone`—surfacing SHA-256 digests, security mitigations (`PIE`, `NX`, `Stripped`), per-section **Shannon Entropy** (`0.00–8.00 bits/byte`), imported/exported symbols, printable strings, 16-byte Hex/ASCII views, and native CPU disassembly (`x86_64`, `x86`, `ARM64`, `ARM`).
   - **Bottom Live Diagnostics Stream:** Timestamped execution and triage audit log.

---

## 🏗 System Architecture

```text
+-----------------------------------------------------------------------------------+
|                        NATIVE WINDOW & GPU VIEWPORT (eframe)                      |
+-----------------------------------------+-----------------------------------------+
|                        EGUI IMMEDIATE-MODE MISSION COCKPIT                        |
|  [Top Bar: CPU/RAM Telemetry & Controls]  [Bottom Panel: Live Diagnostics Stream] |
|  [Left: Target Loader & Cell Tree] [Center: Polyglot Notebook] [Right: Disasm/Hex]|
+--------------------+--------------------+--------------------+--------------------+
|   KERNEL ENGINE    |  BINARY ANALYSIS   |  MISSION STORAGE   |  SYSTEM TELEMETRY  |
| Tokio Subprocesses | Goblin + Capstone  | JSON Mission Files | Sysinfo Subsystem  |
| Rust / Py3 / Bash  | ELF/PE/Mach-O/Hex  | (Phase 2: SQLite)  | Throttled Polling  |
+--------------------+--------------------+--------------------+--------------------+
```

### Repository Structure

| Path                                               | Description                                                                                                                                                                                     |
| :------------------------------------------------- | :---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [`Cargo.toml`](Cargo.toml)                         | Crate manifest and dependency configuration.                                                                                                                                                    |
| [`src/main.rs`](src/main.rs)                       | Application entry point supporting the native `eframe` GUI, `--triage <path>` CLI analyzer, and `--headless` mission runner.                                                                    |
| [`src/lib.rs`](src/lib.rs)                         | Library root exporting all core subsystems.                                                                                                                                                     |
| [`src/kernel/mod.rs`](src/kernel/mod.rs)           | **Polyglot Kernel Manager:** `KernelInstance` (`evcxr --json`) and `KernelBridge` orchestrating persistent `Python 3`, persistent `Bash`, and `rustc` sandbox execution over async Tokio pipes. |
| [`src/analysis/binary.rs`](src/analysis/binary.rs) | **Binary Triage & Disassembly Pipeline:** `BinaryTriageResult` (`goblin` ELF/PE/Mach-O parser, Shannon entropy analyzer, `capstone` CPU disassembler, symbol/string extractor, and Hex viewer). |
| [`src/telemetry/mod.rs`](src/telemetry/mod.rs)     | **Hardware Telemetry Subsystem:** Throttled `sysinfo` CPU/RAM/Swap monitor.                                                                                                                     |
| [`src/storage/mod.rs`](src/storage/mod.rs)         | **Mission Storage:** `MissionNotebook` and `NotebookCell` models, default tactical templates, and JSON serialization.                                                                           |
| [`src/ui/dashboard.rs`](src/ui/dashboard.rs)       | **Tactical Cockpit UI:** `CockpitApp` immediate-mode `egui` 4-zone interface.                                                                                                                   |

---

## 🚀 Quick Start

### Prerequisites

- **Rust Toolchain:** `rustup` (`stable`, Rust 2021 edition, `rustc` on `PATH`)
- **Python 3 & Bash:** `python3` and `bash` on `PATH` for polyglot cell execution

### 1. Launch the Native Desktop Cockpit

```bash
cargo run --release
```

### 2. Run Static Binary Triage & Disassembly from the Terminal

Inspect any `ELF`, `PE32/PE32+`, or `Mach-O` binary without launching the GUI:

```bash
cargo run -- --triage /bin/ls
```

### 3. Run Headless Polyglot Verification & Unit Tests

Execute the test suite and verify all three language runtimes (`Rust`, `Python 3`, `Bash`) plus static binary triage headlessly:

```bash
cargo test
cargo run -- --headless
```

---

## 📊 Project Status & Roadmap

| Component / Feature                                      |  Phase  |   Status    | Notes                                                            |
| :------------------------------------------------------- | :-----: | :---------: | :--------------------------------------------------------------- |
| **Single-Crate Pure-Rust Core (`src/*`)**                | Phase 1 | ✅ **Done** | Zero compiler or Clippy warnings (`cargo clippy --all-targets`)  |
| **Tokio Subprocess Kernel (`Rust`, `Python 3`, `Bash`)** | Phase 1 | ✅ **Done** | Persistent Python & Bash state + `rustc` / `evcxr` execution     |
| **Real-Time Streaming `stdout`/`stderr` & Cell Abort**   | Phase 1 | ✅ **Done** | Non-blocking `mpsc` channels + instant process kill/reset        |
| **Static Binary Triage (`goblin` ELF / PE32 / Mach-O)**  | Phase 1 | ✅ **Done** | Entry point, SHA-256, `PIE`/`NX`/`Stripped` mitigation detection |
| **Section Shannon Entropy (`0.00–8.00 b/B`)**            | Phase 1 | ✅ **Done** | Flags high-entropy packed/encrypted sections (`> 7.0 bits/byte`) |
| **Native CPU Disassembly (`capstone`) & Hex Matrix**     | Phase 1 | ✅ **Done** | `x86_64`, `x86`, `ARM64`, and `ARM` instruction decoding         |
| **Native Immediate-Mode Cockpit UI (`egui` / `eframe`)** | Phase 1 | ✅ **Done** | 4-zone docking panels + live `sysinfo` hardware telemetry bar    |
| **SQLite / SeaORM Historical Run & IOC Storage**         | Phase 2 | ⏳ **Due**  | Persist historical cell executions and local IOC tables          |
| **Embedded Vector Search (`LanceDB`)**                   | Phase 2 | ⏳ **Due**  | Semantic indexing of code cells and disassembled functions       |
| **Control-Flow Graph (CFG) Visualizer**                  | Phase 2 | ⏳ **Due**  | Interactive basic-block graph rendering inside the Triage panel  |

---

## 🤝 Contributing Guidelines

Contributions are welcome! To keep the codebase clean and maintainable:

1. **Keep the Core Simple & Deterministic:** Favor straightforward, idiomatic Rust over heavy external runtimes or build-time code generation.
2. **Never Block the UI Thread:** All subprocess execution and file I/O must run asynchronously or off the render loop.
3. **Zero Warnings Policy:** Ensure all changes pass `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, and `cargo run -- --headless`.

---

## 👏 Credits & Author

**Aether** is architected, designed, and maintained by **Anas Ghareib**.

- **Author:** Anas Ghareib
- **GitHub Portfolio:** [@ansgrb](https://github.com/ansgrb)
- **Email:** [ansgrb@gmail.com](mailto:anasghareib@gmail.com)
- **LinkedIn:** [linkedin.com/in/ansgrb](https://www.linkedin.com/in/ansgrb)
- **X / Twitter:** [@ansgrb](https://x.com/ansgrb)

---

## 📄 License

**Aether** is free and open-source software licensed under the **GNU General Public License v3.0 (GPL-3.0)**. See the [LICENSE](LICENSE) file for the full license text.
