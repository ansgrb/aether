use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConnectionError {
    #[error("failed to read kernel connection file: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse kernel connection JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported transport '{0}' (expected 'tcp')")]
    UnsupportedTransport(String),
    #[error("unsupported signature scheme '{0}' (expected 'hmac-sha256' or empty)")]
    UnsupportedSignatureScheme(String),
}

/// Represents the Jupyter connection file (`kernel.json`) provided via `-f <connection_file>`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KernelConnectionInfo {
    pub ip: String,
    pub transport: String,
    pub shell_port: u16,
    pub iopub_port: u16,
    pub stdin_port: u16,
    pub control_port: u16,
    pub hb_port: u16,
    #[serde(default)]
    pub key: String,
    #[serde(default = "default_signature_scheme")]
    pub signature_scheme: String,
    #[serde(default)]
    pub kernel_name: Option<String>,
}

fn default_signature_scheme() -> String {
    "hmac-sha256".to_string()
}

impl KernelConnectionInfo {
    /// Load and validate connection info from a file on disk.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ConnectionError> {
        let raw = fs::read_to_string(path)?;
        Self::from_json_str(&raw)
    }

    /// Parse and validate connection info from a JSON string.
    pub fn from_json_str(raw: &str) -> Result<Self, ConnectionError> {
        let info: Self = serde_json::from_str(raw)?;
        info.validate()?;
        Ok(info)
    }

    /// Validate transport and cryptographic signature scheme.
    pub fn validate(&self) -> Result<(), ConnectionError> {
        if self.transport != "tcp" {
            return Err(ConnectionError::UnsupportedTransport(
                self.transport.clone(),
            ));
        }
        let scheme = self.signature_scheme.to_ascii_lowercase();
        if !self.key.is_empty() && !scheme.is_empty() && scheme != "hmac-sha256" {
            return Err(ConnectionError::UnsupportedSignatureScheme(
                self.signature_scheme.clone(),
            ));
        }
        Ok(())
    }

    pub fn shell_endpoint(&self) -> String {
        format!("{}://{}:{}", self.transport, self.ip, self.shell_port)
    }

    pub fn iopub_endpoint(&self) -> String {
        format!("{}://{}:{}", self.transport, self.ip, self.iopub_port)
    }

    pub fn stdin_endpoint(&self) -> String {
        format!("{}://{}:{}", self.transport, self.ip, self.stdin_port)
    }

    pub fn control_endpoint(&self) -> String {
        format!("{}://{}:{}", self.transport, self.ip, self.control_port)
    }

    pub fn hb_endpoint(&self) -> String {
        format!("{}://{}:{}", self.transport, self.ip, self.hb_port)
    }
}
