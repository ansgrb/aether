//! Aether Entry Point (`src/main.rs`)
//!
//! Launches the native `eframe`/`egui` Tactical Mission Cockpit by default,
//! or executes headless binary triage (`--triage <path>`) / polyglot self-test (`--headless`).

use std::time::{Duration, Instant};

use aether::analysis::BinaryTriageResult;
use aether::kernel::{CellExecutionStatus, KernelBridge, KernelEvent};
use aether::storage::MissionNotebook;
use aether::telemetry::TelemetryMonitor;
use aether::ui::CockpitApp;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() >= 2 {
        match args[1].as_str() {
            "--triage" | "triage" => {
                let target = args.get(2).map(|s| s.as_str()).unwrap_or("/bin/ls");
                run_cli_triage(target)?;
                return Ok(());
            }
            "--headless" | "--self-test" | "test-mission" => {
                run_cli_headless_mission()?;
                return Ok(());
            }
            "--help" | "-h" => {
                println!(
                    "Aether v0.2.0 — Rust-Native Multi-Kernel Notebook & Binary Analysis Cockpit"
                );
                println!("Usage:");
                println!(
                    "  aether                     Launch Native Tactical Cockpit UI (egui/eframe)"
                );
                println!("  aether --triage <path>     Run static ELF/PE/Mach-O triage & Capstone disassembly");
                println!("  aether --headless          Execute the default polyglot mission notebook headlessly");
                return Ok(());
            }
            _ => {}
        }
    }

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("AETHER — Tactical Mission Cockpit")
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([1024.0, 680.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Aether",
        native_options,
        Box::new(|_cc| Box::new(CockpitApp::new())),
    )
    .map_err(|e| anyhow::anyhow!("Failed to launch native Cockpit window: {e}"))?;

    Ok(())
}

fn run_cli_triage(path: &str) -> anyhow::Result<()> {
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("⚡ AETHER STATIC BINARY TRIAGE: {path}");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    let report = BinaryTriageResult::analyze_sample(path)
        .map_err(|e| anyhow::anyhow!("Triage failed: {e}"))?;

    println!("  Format Class   : {}", report.file_class);
    println!(
        "  Architecture   : {} (64-bit: {})",
        report.arch.label(),
        report.is_64_bit
    );
    println!("  Entry Point    : 0x{:016x}", report.entry_point);
    println!("  File Size      : {} bytes", report.file_size);
    println!("  SHA-256        : {}", report.sha256);
    println!("  Security Flags : {}", report.security_flags.join(", "));
    println!("\n  [Sections & Shannon Entropy]");
    for sec in report.sections.iter().take(12) {
        println!(
            "    {:<16} VAddr=0x{:08x}  Size={:<8}  Entropy={:.2} b/B  Exec={}",
            sec.name, sec.virtual_address, sec.size, sec.entropy, sec.is_executable
        );
    }

    println!(
        "\n  [Capstone Disassembly — showing first {} of {} instructions]",
        report.disassembly.len().min(15),
        report.disassembly.len()
    );
    for insn in report.disassembly.iter().take(15) {
        println!(
            "    0x{:08x}: {:<24} {:<8} {}",
            insn.address, insn.bytes_hex, insn.mnemonic, insn.operands
        );
    }
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    Ok(())
}

fn run_cli_headless_mission() -> anyhow::Result<()> {
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("⚡ AETHER HEADLESS MISSION VERIFICATION");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    let mut telemetry = TelemetryMonitor::new();
    telemetry.force_refresh();
    println!(
        "[TELEMETRY] Host: {} | CPU ({} cores): {:.1}% | MEM: {} / {} MB",
        telemetry.host_label,
        telemetry.cpu_cores,
        telemetry.cpu_usage,
        telemetry.memory_used_mb(),
        telemetry.memory_total_mb()
    );

    let notebook = MissionNotebook::default_tactical_mission();
    run_cli_triage(&notebook.target_binary_path)?;

    let mut bridge = KernelBridge::spawn();
    let total_cells = notebook.cells.len();

    for (idx, cell) in notebook.cells.iter().enumerate() {
        println!(
            "\n▶ Executing Cell #{idx}: '{}' [{}] ...",
            cell.title,
            cell.language.label()
        );
        bridge.execute_cell(cell.id, cell.language, cell.code.clone());

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if Instant::now() > deadline {
                anyhow::bail!("Timeout waiting for cell '{}'", cell.title);
            }
            if let Ok(ev) = bridge.event_rx.try_recv() {
                match ev {
                    KernelEvent::CellStdout { cell_id, line } if cell_id == cell.id => {
                        println!("  [stdout] {line}");
                    }
                    KernelEvent::CellStderr { cell_id, line } if cell_id == cell.id => {
                        eprintln!("  [stderr] {line}");
                    }
                    KernelEvent::CellFinished { cell_id, status } if cell_id == cell.id => {
                        match status {
                            CellExecutionStatus::Success { duration_ms } => {
                                println!("  ✓ Completed in {duration_ms} ms");
                                break;
                            }
                            CellExecutionStatus::Error {
                                duration_ms,
                                message,
                            } => {
                                anyhow::bail!(
                                    "Cell '{}' failed after {duration_ms} ms: {message}",
                                    cell.title
                                );
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    println!("\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("✓ All {total_cells} polyglot mission cells and binary triage verified cleanly.");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    Ok(())
}
