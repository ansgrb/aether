use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use thiserror::Error;
use toml::Value as TomlValue;

#[derive(Debug, Error)]
pub enum EnvParseError {
    #[error("failed to parse %%env block as either JSON or TOML: {0}")]
    InvalidFormat(String),
    #[error("environment variable key '{0}' is invalid")]
    InvalidKey(String),
}

/// Thread-safe environment variable store for the `%%env` magic (README Section 6).
///
/// Avoids calling `std::env::set_var` from concurrent Tokio threads (which is UB on POSIX)
/// and instead synchronizes variables safely into both the embedded Python runtime (`os.environ`)
/// and the `evcxr` Rust evaluation context, while redacting sensitive values in `IOPub` output.
#[derive(Clone, Default)]
pub struct EnvManager {
    vars: Arc<RwLock<HashMap<String, String>>>,
}

impl EnvManager {
    pub fn new() -> Self {
        Self {
            vars: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Parses a `%%env` block (supporting both TOML and JSON) and updates the store.
    /// Returns a list of `(key, redacted_preview)` pairs for safe `IOPub` reporting.
    pub fn apply_env_block(&self, raw: &str) -> Result<Vec<(String, String)>, EnvParseError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }

        let parsed_pairs = if trimmed.starts_with('{') {
            Self::parse_json_env(trimmed)?
        } else {
            match Self::parse_toml_env(trimmed) {
                Ok(pairs) => pairs,
                Err(toml_err) => Self::parse_json_env(trimmed).map_err(|_| toml_err)?,
            }
        };

        let mut guard = self.vars.write().expect("EnvManager RwLock poisoned");
        let mut summaries = Vec::with_capacity(parsed_pairs.len());
        for (k, v) in parsed_pairs {
            Self::validate_env_key(&k)?;
            let preview = Self::redact_if_sensitive(&k, &v);
            guard.insert(k.clone(), v);
            summaries.push((k, preview));
        }

        Ok(summaries)
    }

    pub fn snapshot(&self) -> HashMap<String, String> {
        self.vars
            .read()
            .expect("EnvManager RwLock poisoned")
            .clone()
    }

    fn validate_env_key(key: &str) -> Result<(), EnvParseError> {
        if key.is_empty() || key.contains('=') || key.contains('\0') {
            return Err(EnvParseError::InvalidKey(key.to_string()));
        }
        Ok(())
    }

    fn redact_if_sensitive(key: &str, value: &str) -> String {
        let upper = key.to_ascii_uppercase();
        let is_sensitive = upper.contains("SECRET")
            || upper.contains("PASSWORD")
            || upper.contains("TOKEN")
            || upper.contains("KEY")
            || upper.contains("URL")
            || upper.contains("DSN")
            || upper.contains("CREDENTIAL");
        if is_sensitive {
            "[REDACTED]".to_string()
        } else {
            value.to_string()
        }
    }

    fn parse_json_env(raw: &str) -> Result<Vec<(String, String)>, EnvParseError> {
        let val: JsonValue =
            serde_json::from_str(raw).map_err(|e| EnvParseError::InvalidFormat(e.to_string()))?;
        let mut out = Vec::new();
        if let JsonValue::Object(map) = val {
            Self::flatten_json("", &JsonValue::Object(map), &mut out);
            Ok(out)
        } else {
            Err(EnvParseError::InvalidFormat(
                "JSON %%env payload must be a top-level object".to_string(),
            ))
        }
    }

    fn flatten_json(prefix: &str, val: &JsonValue, out: &mut Vec<(String, String)>) {
        match val {
            JsonValue::Object(map) => {
                for (k, v) in map {
                    let next_key = if prefix.is_empty() {
                        k.to_string()
                    } else {
                        format!("{}_{}", prefix, k)
                    };
                    Self::flatten_json(&next_key, v, out);
                }
            }
            JsonValue::String(s) => out.push((prefix.to_string(), s.clone())),
            JsonValue::Number(n) => out.push((prefix.to_string(), n.to_string())),
            JsonValue::Bool(b) => out.push((prefix.to_string(), b.to_string())),
            JsonValue::Null => {}
            JsonValue::Array(arr) => {
                if let Ok(s) = serde_json::to_string(arr) {
                    out.push((prefix.to_string(), s));
                }
            }
        }
    }

    fn parse_toml_env(raw: &str) -> Result<Vec<(String, String)>, EnvParseError> {
        let val: TomlValue =
            toml::from_str(raw).map_err(|e| EnvParseError::InvalidFormat(e.to_string()))?;
        let mut out = Vec::new();
        if let TomlValue::Table(tbl) = val {
            Self::flatten_toml("", &TomlValue::Table(tbl), &mut out);
            Ok(out)
        } else {
            Err(EnvParseError::InvalidFormat(
                "TOML %%env payload must be a table".to_string(),
            ))
        }
    }

    fn flatten_toml(prefix: &str, val: &TomlValue, out: &mut Vec<(String, String)>) {
        match val {
            TomlValue::Table(tbl) => {
                for (k, v) in tbl {
                    let next_key = if prefix.is_empty() {
                        k.to_string()
                    } else {
                        format!("{}_{}", prefix, k)
                    };
                    Self::flatten_toml(&next_key, v, out);
                }
            }
            TomlValue::String(s) => out.push((prefix.to_string(), s.clone())),
            TomlValue::Integer(i) => out.push((prefix.to_string(), i.to_string())),
            TomlValue::Float(f) => out.push((prefix.to_string(), f.to_string())),
            TomlValue::Boolean(b) => out.push((prefix.to_string(), b.to_string())),
            TomlValue::Datetime(dt) => out.push((prefix.to_string(), dt.to_string())),
            TomlValue::Array(arr) => {
                if let Ok(s) = serde_json::to_string(arr) {
                    out.push((prefix.to_string(), s));
                }
            }
        }
    }
}
