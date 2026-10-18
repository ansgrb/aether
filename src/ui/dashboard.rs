//! Tactical Cockpit UI Layout (`src/ui/dashboard.rs`)
//!
//! High-density immediate-mode operational interface built on `egui` / `eframe`:
//! - Top Bar: Real-time hardware telemetry (`sysinfo`) & global mission controls
//! - Left Panel: Target binary selector & notebook cell tree
//! - Center Viewport: Multi-kernel interactive mission notebook (`Rust`, `Python`, `Bash`)
//! - Right Panel: Malware triage, section entropy, symbol inspector & `Capstone` disassembly stack
//! - Bottom Panel: Live diagnostics & audit stream

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

use chrono::Local;
use egui::{Color32, FontId, RichText, ScrollArea, Stroke, TextEdit, Vec2};

use crate::analysis::BinaryTriageResult;
use crate::kernel::{CellExecutionStatus, KernelBridge, KernelEvent, KernelLanguage};
use crate::storage::{MissionNotebook, NotebookCell};
use crate::telemetry::TelemetryMonitor;

const MAX_DIAGNOSTIC_LOGS: usize = 200;

/// Active tab in the right-hand Binary Triage & Disassembly panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriageTab {
    Overview,
    Sections,
    Disassembly,
    Symbols,
    HexView,
}

/// A timestamped entry in the bottom diagnostics console.
#[derive(Debug, Clone)]
pub struct DiagnosticEntry {
    pub timestamp: String,
    pub level: &'static str,
    pub message: String,
}

/// Primary state for the Tactical Cockpit application.
pub struct CockpitApp {
    pub telemetry: TelemetryMonitor,
    pub kernel_bridge: KernelBridge,
    pub notebook: MissionNotebook,
    pub mission_file_path: String,
    pub triage_result: Option<BinaryTriageResult>,
    pub triage_error: Option<String>,
    pub active_triage_tab: TriageTab,
    pub symbol_filter: String,
    pub diagnostics: VecDeque<DiagnosticEntry>,
    pub show_left_panel: bool,
    pub show_right_panel: bool,
    pub show_bottom_panel: bool,
    style_initialized: bool,
}

impl Default for CockpitApp {
    fn default() -> Self {
        Self::new()
    }
}

impl CockpitApp {
    pub fn new() -> Self {
        let notebook = MissionNotebook::default_tactical_mission();
        let initial_target = notebook.target_binary_path.clone();

        let mut app = Self {
            telemetry: TelemetryMonitor::new(),
            kernel_bridge: KernelBridge::spawn(),
            notebook,
            mission_file_path: "mission_aether.json".to_string(),
            triage_result: None,
            triage_error: None,
            active_triage_tab: TriageTab::Disassembly,
            symbol_filter: String::new(),
            diagnostics: VecDeque::with_capacity(MAX_DIAGNOSTIC_LOGS),
            show_left_panel: true,
            show_right_panel: true,
            show_bottom_panel: true,
            style_initialized: false,
        };

        app.push_log("INFO", "Aether Mission Cockpit initialized.");
        app.run_binary_triage(&initial_target);
        app
    }

    pub fn push_log(&mut self, level: &'static str, message: impl Into<String>) {
        if self.diagnostics.len() >= MAX_DIAGNOSTIC_LOGS {
            self.diagnostics.pop_front();
        }
        self.diagnostics.push_back(DiagnosticEntry {
            timestamp: Local::now().format("%H:%M:%S%.3f").to_string(),
            level,
            message: message.into(),
        });
    }

    /// Runs pure-Rust `goblin` + `capstone` binary analysis on the specified file path.
    pub fn run_binary_triage(&mut self, path: &str) {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            return;
        }
        match BinaryTriageResult::analyze_sample(trimmed) {
            Ok(report) => {
                self.push_log(
                    "INFO",
                    format!(
                        "Triaged '{}' ({}, {} bytes, {} sections, {} instructions)",
                        trimmed,
                        report.file_class,
                        report.file_size,
                        report.sections.len(),
                        report.disassembly.len()
                    ),
                );
                self.triage_result = Some(report);
                self.triage_error = None;
            }
            Err(err) => {
                self.push_log(
                    "ERR",
                    format!("Binary triage failed for '{trimmed}': {err}"),
                );
                self.triage_error = Some(err);
            }
        }
    }

    /// Drains all pending events from the background Tokio kernel orchestrator.
    fn poll_kernel_events(&mut self) {
        while let Ok(ev) = self.kernel_bridge.event_rx.try_recv() {
            match ev {
                KernelEvent::CellStarted { cell_id, .. } => {
                    if let Some(cell) = self.notebook.cells.iter_mut().find(|c| c.id == cell_id) {
                        cell.stdout.clear();
                        cell.stderr.clear();
                        cell.status = CellExecutionStatus::Running;
                    }
                }
                KernelEvent::CellStdout { cell_id, line } => {
                    if let Some(cell) = self.notebook.cells.iter_mut().find(|c| c.id == cell_id) {
                        cell.stdout.push(line);
                    }
                }
                KernelEvent::CellStderr { cell_id, line } => {
                    if let Some(cell) = self.notebook.cells.iter_mut().find(|c| c.id == cell_id) {
                        cell.stderr.push(line);
                    }
                }
                KernelEvent::CellFinished { cell_id, status } => {
                    let mut log_msg = None;
                    if let Some(cell) = self.notebook.cells.iter_mut().find(|c| c.id == cell_id) {
                        match &status {
                            CellExecutionStatus::Success { duration_ms } => {
                                log_msg = Some((
                                    "INFO",
                                    format!(
                                        "Cell '{}' [{}] completed in {} ms",
                                        cell.title,
                                        cell.language.short_tag(),
                                        duration_ms
                                    ),
                                ));
                            }
                            CellExecutionStatus::Error {
                                duration_ms,
                                message,
                            } => {
                                log_msg = Some((
                                    "ERR",
                                    format!(
                                        "Cell '{}' [{}] failed ({} ms): {}",
                                        cell.title,
                                        cell.language.short_tag(),
                                        duration_ms,
                                        message
                                    ),
                                ));
                            }
                            _ => {}
                        }
                        cell.status = status;
                    }
                    if let Some((lvl, msg)) = log_msg {
                        self.push_log(lvl, msg);
                    }
                }
                KernelEvent::DiagnosticLog { level, message } => {
                    self.push_log(level, message);
                }
            }
        }
    }

    fn configure_tactical_theme(&mut self, ctx: &egui::Context) {
        if self.style_initialized {
            return;
        }
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = Color32::from_rgb(13, 17, 23);
        visuals.window_fill = Color32::from_rgb(16, 22, 30);
        visuals.extreme_bg_color = Color32::from_rgb(8, 11, 15);
        visuals.faint_bg_color = Color32::from_rgb(20, 26, 35);
        visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(18, 24, 33);
        visuals.widgets.noninteractive.bg_stroke =
            Stroke::new(1.0_f32, Color32::from_rgb(44, 54, 68));
        visuals.widgets.inactive.bg_fill = Color32::from_rgb(24, 32, 44);
        visuals.widgets.hovered.bg_fill = Color32::from_rgb(34, 48, 66);
        visuals.widgets.active.bg_fill = Color32::from_rgb(42, 62, 86);
        ctx.set_visuals(visuals);
        self.style_initialized = true;
    }

    /// Renders the entire Cockpit viewport inside `egui::Context`.
    pub fn render_cockpit(&mut self, ctx: &egui::Context) {
        self.configure_tactical_theme(ctx);
        self.telemetry.tick();
        self.poll_kernel_events();

        self.render_top_telemetry_bar(ctx);
        if self.show_bottom_panel {
            self.render_bottom_diagnostics_panel(ctx);
        }
        if self.show_left_panel {
            self.render_left_mission_panel(ctx);
        }
        if self.show_right_panel {
            self.render_right_triage_panel(ctx);
        }
        self.render_center_notebook_panel(ctx);

        ctx.request_repaint_after(Duration::from_millis(120));
    }

    fn render_top_telemetry_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("telemetry_bar")
            .exact_height(38.0)
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.colored_label(
                        Color32::from_rgb(63, 185, 80),
                        RichText::new("⚡ AETHER CORE").strong(),
                    );
                    ui.separator();

                    let cpu_color = if self.telemetry.cpu_usage > 80.0 {
                        Color32::from_rgb(248, 81, 73)
                    } else if self.telemetry.cpu_usage > 50.0 {
                        Color32::from_rgb(210, 153, 34)
                    } else {
                        Color32::from_rgb(88, 166, 255)
                    };
                    ui.colored_label(
                        cpu_color,
                        RichText::new(format!(
                            "CPU ({}c): {:>5.1}%",
                            self.telemetry.cpu_cores, self.telemetry.cpu_usage
                        ))
                        .monospace(),
                    );

                    ui.separator();

                    ui.colored_label(
                        Color32::from_rgb(163, 113, 247),
                        RichText::new(format!(
                            "MEM: {} / {} MB ({:.0}%)",
                            self.telemetry.memory_used_mb(),
                            self.telemetry.memory_total_mb(),
                            self.telemetry.memory_fraction() * 100.0
                        ))
                        .monospace(),
                    );

                    ui.separator();
                    ui.colored_label(
                        Color32::from_rgb(248, 81, 73),
                        RichText::new("SANDBOX: ISOLATED_SUBPROCESS")
                            .monospace()
                            .strong(),
                    );

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.toggle_value(&mut self.show_bottom_panel, "📟 Logs");
                        ui.toggle_value(&mut self.show_right_panel, "🔬 Triage");
                        ui.toggle_value(&mut self.show_left_panel, "📁 Tree");
                        ui.separator();

                        if ui.button("🔄 Reset Kernels").clicked() {
                            self.kernel_bridge.reset_all();
                        }
                        if ui.button("▶ Run All").clicked() {
                            let cells_snapshot: Vec<_> = self
                                .notebook
                                .cells
                                .iter()
                                .map(|c| (c.id, c.language, c.code.clone()))
                                .collect();
                            for (id, lang, code) in cells_snapshot {
                                self.kernel_bridge.execute_cell(id, lang, code);
                            }
                        }
                    });
                });
            });
    }

    fn render_left_mission_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("left_mission_tree")
            .resizable(true)
            .default_width(250.0)
            .min_width(200.0)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.colored_label(
                    Color32::from_rgb(88, 166, 255),
                    RichText::new("TARGET BINARY INGESTION").strong(),
                );
                ui.separator();

                ui.label(RichText::new("Sample Path (ELF / PE / Mach-O):").small());
                ui.add(
                    TextEdit::singleline(&mut self.notebook.target_binary_path)
                        .font(FontId::monospace(12.0))
                        .desired_width(f32::INFINITY),
                );

                ui.horizontal(|ui| {
                    if ui.button("🔍 Analyze Sample").clicked() {
                        let path = self.notebook.target_binary_path.clone();
                        self.run_binary_triage(&path);
                    }
                    if ui.button("Self Exe").clicked() {
                        if let Ok(exe) = std::env::current_exe() {
                            let path_str = exe.to_string_lossy().into_owned();
                            self.notebook.target_binary_path = path_str.clone();
                            self.run_binary_triage(&path_str);
                        }
                    }
                });

                ui.horizontal(|ui| {
                    ui.label(RichText::new("Presets:").small().weak());
                    for preset in ["/bin/ls", "/usr/bin/env", "/bin/sh"] {
                        if PathBuf::from(preset).exists() && ui.small_button(preset).clicked() {
                            self.notebook.target_binary_path = preset.to_string();
                            self.run_binary_triage(preset);
                        }
                    }
                });

                ui.add_space(12.0);
                ui.colored_label(
                    Color32::from_rgb(88, 166, 255),
                    RichText::new("NOTEBOOK CELLS").strong(),
                );
                ui.separator();

                ui.horizontal(|ui| {
                    if ui.small_button("+ 🦀 Rust").clicked() {
                        self.notebook.cells.push(NotebookCell::new(
                            format!("Rust Cell #{}", self.notebook.cells.len() + 1),
                            KernelLanguage::Rust,
                            "println!(\"Check OK\");",
                        ));
                    }
                    if ui.small_button("+ 🐍 Py3").clicked() {
                        self.notebook.cells.push(NotebookCell::new(
                            format!("Python Cell #{}", self.notebook.cells.len() + 1),
                            KernelLanguage::Python,
                            "print('Python kernel ready')",
                        ));
                    }
                    if ui.small_button("+ 🐚 Bash").clicked() {
                        self.notebook.cells.push(NotebookCell::new(
                            format!("Shell Cell #{}", self.notebook.cells.len() + 1),
                            KernelLanguage::Bash,
                            "uname -a",
                        ));
                    }
                });

                ui.add_space(4.0);
                ScrollArea::vertical()
                    .id_source("left_cell_list_scroll")
                    .max_height(260.0)
                    .show(ui, |ui| {
                        for (idx, cell) in self.notebook.cells.iter().enumerate() {
                            let (badge_color, badge_txt) = match &cell.status {
                                CellExecutionStatus::Idle => {
                                    (Color32::from_rgb(139, 148, 158), "IDLE")
                                }
                                CellExecutionStatus::Running => {
                                    (Color32::from_rgb(210, 153, 34), "RUN")
                                }
                                CellExecutionStatus::Success { .. } => {
                                    (Color32::from_rgb(63, 185, 80), "OK")
                                }
                                CellExecutionStatus::Error { .. } => {
                                    (Color32::from_rgb(248, 81, 73), "ERR")
                                }
                            };
                            ui.horizontal(|ui| {
                                ui.colored_label(
                                    badge_color,
                                    RichText::new(format!("[{badge_txt}]")).monospace().small(),
                                );
                                ui.label(
                                    RichText::new(format!(
                                        "#{idx} [{}] {}",
                                        cell.language.short_tag(),
                                        cell.title
                                    ))
                                    .small(),
                                );
                            });
                        }
                    });

                ui.add_space(12.0);
                ui.colored_label(
                    Color32::from_rgb(88, 166, 255),
                    RichText::new("MISSION PERSISTENCE").strong(),
                );
                ui.separator();
                ui.add(
                    TextEdit::singleline(&mut self.mission_file_path)
                        .font(FontId::monospace(12.0))
                        .desired_width(f32::INFINITY),
                );
                ui.horizontal(|ui| {
                    if ui.button("💾 Save JSON").clicked() {
                        let path = self.mission_file_path.clone();
                        match self.notebook.save_to_file(&path) {
                            Ok(()) => self.push_log("INFO", format!("Saved mission to '{path}'")),
                            Err(e) => self.push_log("ERR", e),
                        }
                    }
                    if ui.button("📂 Load JSON").clicked() {
                        let path = self.mission_file_path.clone();
                        match MissionNotebook::load_from_file(&path) {
                            Ok(loaded) => {
                                let target = loaded.target_binary_path.clone();
                                self.notebook = loaded;
                                self.push_log("INFO", format!("Loaded mission from '{path}'"));
                                self.run_binary_triage(&target);
                            }
                            Err(e) => self.push_log("ERR", e),
                        }
                    }
                });
            });
    }

    fn render_right_triage_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("right_triage_stack")
            .resizable(true)
            .default_width(470.0)
            .min_width(320.0)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.colored_label(
                    Color32::from_rgb(63, 185, 80),
                    RichText::new("STATIC BINARY TRIAGE & DISASSEMBLY STACK").strong(),
                );
                ui.separator();

                ui.horizontal_wrapped(|ui| {
                    ui.selectable_value(
                        &mut self.active_triage_tab,
                        TriageTab::Disassembly,
                        "⚙ Disasm (Capstone)",
                    );
                    ui.selectable_value(
                        &mut self.active_triage_tab,
                        TriageTab::Overview,
                        "📋 Headers",
                    );
                    ui.selectable_value(
                        &mut self.active_triage_tab,
                        TriageTab::Sections,
                        "📊 Sections & Entropy",
                    );
                    ui.selectable_value(
                        &mut self.active_triage_tab,
                        TriageTab::Symbols,
                        "🔗 Symbols & Strings",
                    );
                    ui.selectable_value(
                        &mut self.active_triage_tab,
                        TriageTab::HexView,
                        "🔢 Hex Matrix",
                    );
                });
                ui.separator();

                if let Some(err) = &self.triage_error {
                    ui.colored_label(Color32::from_rgb(248, 81, 73), format!("⚠ {err}"));
                }

                let Some(report) = &self.triage_result else {
                    ui.label("No binary sample loaded. Select a target on the left panel.");
                    return;
                };

                match self.active_triage_tab {
                    TriageTab::Overview => {
                        egui::Grid::new("triage_overview_grid")
                            .num_columns(2)
                            .spacing([12.0, 6.0])
                            .striped(true)
                            .show(ui, |ui| {
                                ui.label(RichText::new("Sample Path:").strong());
                                ui.label(RichText::new(&report.sample_path).monospace());
                                ui.end_row();

                                ui.label(RichText::new("Format Class:").strong());
                                ui.colored_label(
                                    Color32::from_rgb(88, 166, 255),
                                    &report.file_class,
                                );
                                ui.end_row();

                                ui.label(RichText::new("CPU Architecture:").strong());
                                ui.label(RichText::new(report.arch.label()).monospace());
                                ui.end_row();

                                ui.label(RichText::new("Entry Point:").strong());
                                ui.colored_label(
                                    Color32::from_rgb(63, 185, 80),
                                    RichText::new(format!("0x{:016x}", report.entry_point))
                                        .monospace(),
                                );
                                ui.end_row();

                                ui.label(RichText::new("File Size:").strong());
                                ui.label(
                                    RichText::new(format!(
                                        "{} bytes ({:.2} KB)",
                                        report.file_size,
                                        report.file_size as f64 / 1024.0
                                    ))
                                    .monospace(),
                                );
                                ui.end_row();

                                ui.label(RichText::new("SHA-256:").strong());
                                ui.label(RichText::new(&report.sha256).monospace().small());
                                ui.end_row();
                            });

                        ui.add_space(10.0);
                        ui.label(RichText::new("Security Mitigations & Flags:").strong());
                        for flag in &report.security_flags {
                            ui.colored_label(
                                Color32::from_rgb(210, 153, 34),
                                RichText::new(format!("  • {flag}")).monospace(),
                            );
                        }
                    }

                    TriageTab::Sections => {
                        ui.label(
                            RichText::new(
                                "Shannon Entropy > 7.0 bits/byte indicates packed/encrypted payload",
                            )
                            .small()
                            .weak(),
                        );
                        ScrollArea::vertical()
                            .id_source("sections_scroll")
                            .show(ui, |ui| {
                                egui::Grid::new("sections_table_grid")
                                    .num_columns(5)
                                    .spacing([10.0, 4.0])
                                    .striped(true)
                                    .show(ui, |ui| {
                                        ui.label(RichText::new("Section").strong());
                                        ui.label(RichText::new("VAddr").strong());
                                        ui.label(RichText::new("Size").strong());
                                        ui.label(RichText::new("Entropy").strong());
                                        ui.label(RichText::new("Exec").strong());
                                        ui.end_row();

                                        for sec in &report.sections {
                                            ui.label(RichText::new(&sec.name).monospace());
                                            ui.label(
                                                RichText::new(format!(
                                                    "0x{:08x}",
                                                    sec.virtual_address
                                                ))
                                                .monospace(),
                                            );
                                            ui.label(
                                                RichText::new(format!("{} B", sec.size))
                                                    .monospace(),
                                            );
                                            let ent_color = if sec.entropy >= 7.0 {
                                                Color32::from_rgb(248, 81, 73)
                                            } else if sec.entropy >= 6.0 {
                                                Color32::from_rgb(210, 153, 34)
                                            } else {
                                                Color32::from_rgb(63, 185, 80)
                                            };
                                            ui.colored_label(
                                                ent_color,
                                                RichText::new(format!("{:.2} b/B", sec.entropy))
                                                    .monospace(),
                                            );
                                            ui.label(if sec.is_executable { "RX" } else { "R" });
                                            ui.end_row();
                                        }
                                    });
                            });
                    }

                    TriageTab::Disassembly => {
                        ui.label(
                            RichText::new(format!(
                                "Capstone Disassembly ({}) — {} instructions decoded",
                                report.arch.label(),
                                report.disassembly.len()
                            ))
                            .small()
                            .weak(),
                        );
                        ScrollArea::vertical()
                            .id_source("disasm_scroll")
                            .show(ui, |ui| {
                                egui::Grid::new("disasm_grid")
                                    .num_columns(4)
                                    .spacing([10.0, 3.0])
                                    .striped(true)
                                    .show(ui, |ui| {
                                        for insn in &report.disassembly {
                                            ui.colored_label(
                                                Color32::from_rgb(139, 148, 158),
                                                RichText::new(format!("0x{:08x}", insn.address))
                                                    .monospace()
                                                    .small(),
                                            );
                                            ui.colored_label(
                                                Color32::from_rgb(110, 118, 129),
                                                RichText::new(&insn.bytes_hex).monospace().small(),
                                            );
                                            ui.colored_label(
                                                Color32::from_rgb(88, 166, 255),
                                                RichText::new(&insn.mnemonic)
                                                    .monospace()
                                                    .strong()
                                                    .small(),
                                            );
                                            ui.colored_label(
                                                Color32::from_rgb(201, 209, 217),
                                                RichText::new(&insn.operands).monospace().small(),
                                            );
                                            ui.end_row();
                                        }
                                    });
                            });
                    }

                    TriageTab::Symbols => {
                        ui.horizontal(|ui| {
                            ui.label("Filter:");
                            ui.add(
                                TextEdit::singleline(&mut self.symbol_filter)
                                    .hint_text("Search imports, exports, strings...")
                                    .desired_width(240.0),
                            );
                        });
                        let filter_lc = self.symbol_filter.to_lowercase();

                        ScrollArea::vertical()
                            .id_source("symbols_scroll")
                            .show(ui, |ui| {
                                ui.colored_label(
                                    Color32::from_rgb(88, 166, 255),
                                    RichText::new(format!(
                                        "Imported Symbols ({})",
                                        report.imported_symbols.len()
                                    ))
                                    .strong(),
                                );
                                for sym in report
                                    .imported_symbols
                                    .iter()
                                    .filter(|s| {
                                        filter_lc.is_empty()
                                            || s.to_lowercase().contains(&filter_lc)
                                    })
                                    .take(100)
                                {
                                    ui.label(
                                        RichText::new(format!("  ↳ {sym}")).monospace().small(),
                                    );
                                }

                                ui.add_space(8.0);
                                ui.colored_label(
                                    Color32::from_rgb(63, 185, 80),
                                    RichText::new(format!(
                                        "Extracted Printable Strings ({})",
                                        report.strings_sample.len()
                                    ))
                                    .strong(),
                                );
                                for s in report
                                    .strings_sample
                                    .iter()
                                    .filter(|s| {
                                        filter_lc.is_empty()
                                            || s.to_lowercase().contains(&filter_lc)
                                    })
                                    .take(100)
                                {
                                    ui.label(
                                        RichText::new(format!("  \"{s}\"")).monospace().small(),
                                    );
                                }
                            });
                    }

                    TriageTab::HexView => {
                        ScrollArea::vertical()
                            .id_source("hex_view_scroll")
                            .show(ui, |ui| {
                                for row in &report.hex_preview {
                                    ui.horizontal(|ui| {
                                        ui.colored_label(
                                            Color32::from_rgb(139, 148, 158),
                                            RichText::new(format!("{:08x}", row.offset))
                                                .monospace()
                                                .small(),
                                        );
                                        ui.colored_label(
                                            Color32::from_rgb(88, 166, 255),
                                            RichText::new(&row.hex_bytes).monospace().small(),
                                        );
                                        ui.colored_label(
                                            Color32::from_rgb(63, 185, 80),
                                            RichText::new(format!("|{}|", row.ascii))
                                                .monospace()
                                                .small(),
                                        );
                                    });
                                }
                            });
                    }
                }
            });
    }

    fn render_bottom_diagnostics_panel(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("bottom_diagnostics_stream")
            .resizable(true)
            .default_height(135.0)
            .min_height(80.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(
                        Color32::from_rgb(210, 153, 34),
                        RichText::new("LIVE DIAGNOSTICS & AUDIT STREAM").strong(),
                    );
                    ui.label(
                        RichText::new(format!("— {}", self.telemetry.host_label))
                            .small()
                            .weak(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("🧹 Clear Log").clicked() {
                            self.diagnostics.clear();
                        }
                    });
                });
                ui.separator();

                ScrollArea::vertical()
                    .id_source("diagnostics_scroll")
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for entry in &self.diagnostics {
                            let lvl_color = match entry.level {
                                "ERR" => Color32::from_rgb(248, 81, 73),
                                "WARN" => Color32::from_rgb(210, 153, 34),
                                "EXEC" => Color32::from_rgb(88, 166, 255),
                                _ => Color32::from_rgb(63, 185, 80),
                            };
                            ui.horizontal(|ui| {
                                ui.colored_label(
                                    Color32::from_rgb(110, 118, 129),
                                    RichText::new(&entry.timestamp).monospace().small(),
                                );
                                ui.colored_label(
                                    lvl_color,
                                    RichText::new(format!("[{:^4}]", entry.level))
                                        .monospace()
                                        .strong()
                                        .small(),
                                );
                                ui.label(RichText::new(&entry.message).monospace().small());
                            });
                        }
                    });
            });
    }

    fn render_center_notebook_panel(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.colored_label(
                    Color32::from_rgb(88, 166, 255),
                    RichText::new("MISSION NOTEBOOK:").strong(),
                );
                ui.add(
                    TextEdit::singleline(&mut self.notebook.mission_name)
                        .font(FontId::proportional(15.0))
                        .desired_width(360.0),
                );
            });
            ui.separator();

            let mut cell_to_execute: Option<(uuid::Uuid, KernelLanguage, String)> = None;
            let mut cell_to_kill: Option<uuid::Uuid> = None;
            let mut cell_to_delete: Option<usize> = None;
            let mut swap_pair: Option<(usize, usize)> = None;

            let num_cells = self.notebook.cells.len();

            ScrollArea::vertical()
                .id_source("center_notebook_scroll")
                .show(ui, |ui| {
                    for idx in 0..num_cells {
                        let cell = &mut self.notebook.cells[idx];
                        let border_color = match &cell.status {
                            CellExecutionStatus::Idle => Color32::from_rgb(48, 54, 61),
                            CellExecutionStatus::Running => Color32::from_rgb(210, 153, 34),
                            CellExecutionStatus::Success { .. } => Color32::from_rgb(46, 160, 67),
                            CellExecutionStatus::Error { .. } => Color32::from_rgb(248, 81, 73),
                        };

                        egui::Frame::group(ui.style())
                            .fill(Color32::from_rgb(16, 22, 30))
                            .stroke(Stroke::new(1.2_f32, border_color))
                            .inner_margin(egui::Margin::same(10.0))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.colored_label(
                                        Color32::from_rgb(88, 166, 255),
                                        RichText::new(format!("CELL #{idx}")).monospace().strong(),
                                    );

                                    egui::ComboBox::from_id_source(("lang_combo", cell.id))
                                        .selected_text(cell.language.label())
                                        .width(155.0)
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(
                                                &mut cell.language,
                                                KernelLanguage::Rust,
                                                KernelLanguage::Rust.label(),
                                            );
                                            ui.selectable_value(
                                                &mut cell.language,
                                                KernelLanguage::Python,
                                                KernelLanguage::Python.label(),
                                            );
                                            ui.selectable_value(
                                                &mut cell.language,
                                                KernelLanguage::Bash,
                                                KernelLanguage::Bash.label(),
                                            );
                                        });

                                    ui.add(
                                        TextEdit::singleline(&mut cell.title).desired_width(210.0),
                                    );

                                    match &cell.status {
                                        CellExecutionStatus::Idle => {
                                            ui.colored_label(
                                                Color32::from_rgb(139, 148, 158),
                                                RichText::new("● IDLE").monospace().small(),
                                            );
                                        }
                                        CellExecutionStatus::Running => {
                                            ui.colored_label(
                                                Color32::from_rgb(210, 153, 34),
                                                RichText::new("⟳ RUNNING...").monospace().strong(),
                                            );
                                        }
                                        CellExecutionStatus::Success { duration_ms } => {
                                            ui.colored_label(
                                                Color32::from_rgb(63, 185, 80),
                                                RichText::new(format!("✓ OK ({duration_ms} ms)"))
                                                    .monospace()
                                                    .small(),
                                            );
                                        }
                                        CellExecutionStatus::Error {
                                            duration_ms,
                                            message,
                                        } => {
                                            ui.colored_label(
                                                Color32::from_rgb(248, 81, 73),
                                                RichText::new(format!(
                                                    "✗ ERR ({duration_ms} ms): {message}"
                                                ))
                                                .monospace()
                                                .small(),
                                            );
                                        }
                                    }

                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui
                                                .small_button("🗑")
                                                .on_hover_text("Delete cell")
                                                .clicked()
                                            {
                                                cell_to_delete = Some(idx);
                                            }
                                            if idx + 1 < num_cells
                                                && ui
                                                    .small_button("⬇")
                                                    .on_hover_text("Move down")
                                                    .clicked()
                                            {
                                                swap_pair = Some((idx, idx + 1));
                                            }
                                            if idx > 0
                                                && ui
                                                    .small_button("⬆")
                                                    .on_hover_text("Move up")
                                                    .clicked()
                                            {
                                                swap_pair = Some((idx - 1, idx));
                                            }
                                            if ui.small_button("🧹 Clear").clicked() {
                                                cell.clear_output();
                                            }
                                            if cell.status == CellExecutionStatus::Running {
                                                if ui.button("⏹ Abort").clicked() {
                                                    cell_to_kill = Some(cell.id);
                                                }
                                            } else if ui.button("🚀 Execute").clicked() {
                                                cell_to_execute = Some((
                                                    cell.id,
                                                    cell.language,
                                                    cell.code.clone(),
                                                ));
                                            }
                                        },
                                    );
                                });

                                ui.add_space(6.0);

                                ui.add(
                                    TextEdit::multiline(&mut cell.code)
                                        .code_editor()
                                        .font(FontId::monospace(13.0))
                                        .desired_rows(5)
                                        .desired_width(f32::INFINITY),
                                );

                                if !cell.stdout.is_empty() || !cell.stderr.is_empty() {
                                    ui.add_space(6.0);
                                    egui::Frame::none()
                                        .fill(Color32::from_rgb(9, 12, 16))
                                        .stroke(Stroke::new(1.0_f32, Color32::from_rgb(33, 38, 45)))
                                        .inner_margin(egui::Margin::same(8.0))
                                        .show(ui, |ui| {
                                            ui.set_min_size(Vec2::new(ui.available_width(), 24.0));
                                            for line in &cell.stdout {
                                                ui.colored_label(
                                                    Color32::from_rgb(201, 209, 217),
                                                    RichText::new(line).monospace().size(12.5),
                                                );
                                            }
                                            for err_line in &cell.stderr {
                                                ui.colored_label(
                                                    Color32::from_rgb(248, 81, 73),
                                                    RichText::new(err_line).monospace().size(12.5),
                                                );
                                            }
                                        });
                                }
                            });

                        ui.add_space(10.0);
                    }
                });

            if let Some((id, lang, code)) = cell_to_execute {
                self.kernel_bridge.execute_cell(id, lang, code);
            }
            if let Some(id) = cell_to_kill {
                self.kernel_bridge.kill_cell(id);
            }
            if let Some((a, b)) = swap_pair {
                self.notebook.cells.swap(a, b);
            }
            if let Some(del_idx) = cell_to_delete {
                if self.notebook.cells.len() > 1 {
                    self.notebook.cells.remove(del_idx);
                }
            }
        });
    }
}

impl eframe::App for CockpitApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.render_cockpit(ctx);
    }
}
