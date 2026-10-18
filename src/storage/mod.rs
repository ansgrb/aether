//! Mission Notebook Persistence & Tactical Templates (`src/storage/mod.rs`)

use std::path::Path;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::kernel::{CellExecutionStatus, KernelLanguage};

/// A single executable cell in a Mission Notebook.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotebookCell {
    pub id: Uuid,
    pub title: String,
    pub language: KernelLanguage,
    pub code: String,
    #[serde(default)]
    pub stdout: Vec<String>,
    #[serde(default)]
    pub stderr: Vec<String>,
    #[serde(default = "default_idle_status")]
    pub status: CellExecutionStatus,
}

fn default_idle_status() -> CellExecutionStatus {
    CellExecutionStatus::Idle
}

impl NotebookCell {
    pub fn new(
        title: impl Into<String>,
        language: KernelLanguage,
        code: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            title: title.into(),
            language,
            code: code.into(),
            stdout: Vec::new(),
            stderr: Vec::new(),
            status: CellExecutionStatus::Idle,
        }
    }

    pub fn clear_output(&mut self) {
        self.stdout.clear();
        self.stderr.clear();
        self.status = CellExecutionStatus::Idle;
    }
}

/// Complete serializable Mission Notebook document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionNotebook {
    pub mission_name: String,
    pub target_binary_path: String,
    pub cells: Vec<NotebookCell>,
}

impl Default for MissionNotebook {
    fn default() -> Self {
        Self::default_tactical_mission()
    }
}

impl MissionNotebook {
    pub fn default_tactical_mission() -> Self {
        let default_target = if Path::new("/bin/ls").exists() {
            "/bin/ls".to_string()
        } else {
            std::env::current_exe()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "/bin/sh".to_string())
        };

        Self {
            mission_name: "OP-AETHER-01 // Malware & Telemetry Triage".to_string(),
            target_binary_path: default_target,
            cells: vec![
                NotebookCell::new(
                    "XOR Payload Decoder & Beacon Check",
                    KernelLanguage::Rust,
                    r#"const XOR_KEY: u8 = 0x5A;
let encoded: [u8; 18] = [
    0x18, 0x16, 0x1B, 0x19, 0x11, 0x18, 0x13, 0x08, 0x1E,
    0x05, 0x15, 0x14, 0x16, 0x13, 0x14, 0x1F, 0x7B, 0x5A,
];
let decoded: String = encoded
    .iter()
    .take_while(|&&b| b != XOR_KEY)
    .map(|&b| (b ^ XOR_KEY) as char)
    .collect();
println!("[+] Decoded marker: {}", decoded);
println!("[+] Key byte: 0x{:02X} | Payload length: {} bytes", XOR_KEY, encoded.len());"#,
                ),
                NotebookCell::new(
                    "Stateful IOC & Hash Analysis",
                    KernelLanguage::Python,
                    r#"import hashlib

sample_iocs = [
    b"198.51.100.42:8443/stage2.elf",
    b"mutex://aether_tactical_lock",
]

for idx, ioc in enumerate(sample_iocs, 1):
    digest = hashlib.sha256(ioc).hexdigest()[:24]
    print(f"[IOC #{idx}] {ioc.decode()} -> sha256:{digest}...")"#,
                ),
                NotebookCell::new(
                    "Host Recon & Target File Verification",
                    KernelLanguage::Bash,
                    r#"echo "[*] Kernel: $(uname -sr) | Arch: $(uname -m)"
echo "[*] Active user: $(whoami) | Uptime: $(uptime -p 2>/dev/null || uptime)"
ls -lh /bin/ls 2>/dev/null || true"#,
                ),
            ],
        }
    }

    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize mission notebook: {e}"))?;
        std::fs::write(path, json).map_err(|e| format!("Failed to write mission file: {e}"))
    }

    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read mission file: {e}"))?;
        serde_json::from_str(&content)
            .map_err(|e| format!("Failed to parse mission notebook JSON: {e}"))
    }
}
