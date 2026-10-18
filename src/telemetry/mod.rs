//! Real-Time Hardware & System Telemetry (`src/telemetry/mod.rs`)
//!
//! Samples CPU, memory, swap, and host metrics via `sysinfo` on a throttled cadence
//! to keep the immediate-mode Cockpit UI responsive at 60+ FPS.

use std::collections::VecDeque;
use std::time::{Duration, Instant};
use sysinfo::System;

const MAX_HISTORY_SAMPLES: usize = 60;
const REFRESH_INTERVAL: Duration = Duration::from_millis(750);

pub struct TelemetryMonitor {
    sys: System,
    last_refresh: Instant,
    pub cpu_usage: f32,
    pub cpu_cores: usize,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
    pub swap_used_bytes: u64,
    pub swap_total_bytes: u64,
    pub cpu_history: VecDeque<f32>,
    pub host_label: String,
}

impl Default for TelemetryMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl TelemetryMonitor {
    pub fn new() -> Self {
        let mut sys = System::new_all();
        sys.refresh_cpu();
        sys.refresh_memory();

        let cpu_usage = sys.global_cpu_info().cpu_usage();
        let cpu_cores = sys.cpus().len();
        let memory_used_bytes = sys.used_memory();
        let memory_total_bytes = sys.total_memory().max(1);
        let swap_used_bytes = sys.used_swap();
        let swap_total_bytes = sys.total_swap();

        let host = System::host_name().unwrap_or_else(|| "aether-node".to_string());
        let kernel = System::kernel_version().unwrap_or_else(|| "linux".to_string());

        let mut cpu_history = VecDeque::with_capacity(MAX_HISTORY_SAMPLES);
        cpu_history.push_back(cpu_usage);

        Self {
            sys,
            last_refresh: Instant::now(),
            cpu_usage,
            cpu_cores,
            memory_used_bytes,
            memory_total_bytes,
            swap_used_bytes,
            swap_total_bytes,
            cpu_history,
            host_label: format!("{host} ({kernel})"),
        }
    }

    /// Updates hardware metrics if at least `REFRESH_INTERVAL` has elapsed.
    pub fn tick(&mut self) -> bool {
        if self.last_refresh.elapsed() < REFRESH_INTERVAL {
            return false;
        }
        self.force_refresh();
        true
    }

    /// Immediately refreshes CPU and memory metrics.
    pub fn force_refresh(&mut self) {
        self.sys.refresh_cpu();
        self.sys.refresh_memory();
        self.last_refresh = Instant::now();

        self.cpu_usage = self.sys.global_cpu_info().cpu_usage();
        self.cpu_cores = self.sys.cpus().len();
        self.memory_used_bytes = self.sys.used_memory();
        self.memory_total_bytes = self.sys.total_memory().max(1);
        self.swap_used_bytes = self.sys.used_swap();
        self.swap_total_bytes = self.sys.total_swap();

        if self.cpu_history.len() >= MAX_HISTORY_SAMPLES {
            self.cpu_history.pop_front();
        }
        self.cpu_history.push_back(self.cpu_usage);
    }

    pub fn memory_used_mb(&self) -> u64 {
        self.memory_used_bytes / (1024 * 1024)
    }

    pub fn memory_total_mb(&self) -> u64 {
        self.memory_total_bytes / (1024 * 1024)
    }

    pub fn memory_fraction(&self) -> f32 {
        (self.memory_used_bytes as f64 / self.memory_total_bytes.max(1) as f64) as f32
    }
}
