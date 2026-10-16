/// Cell execution target determined by inspecting the cell magic (`%%`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellTarget {
    /// Default or explicit `%%rust` cell routed to the `evcxr` pipeline.
    Rust(String),
    /// Explicit `%%python` / `%%py` cell routed to the `pyo3` pipeline.
    Python(String),
    /// Explicit `%%env` cell (TOML or JSON) routed to the orchestrator's environment manager.
    Env(String),
}

/// Parses a notebook cell's source string and extracts its magic header (`%%python`, `%%rust`, `%%env`)
/// if present on the first non-empty line.
pub fn parse_cell_magic(raw_code: &str) -> CellTarget {
    let trimmed_start = raw_code.trim_start_matches([' ', '\t', '\r', '\n']);
    let mut lines = trimmed_start.lines();
    let Some(first_line) = lines.next() else {
        return CellTarget::Rust(String::new());
    };

    let header = first_line.trim();
    if let Some(magic_rest) = header.strip_prefix("%%") {
        let directive = magic_rest
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let body = trimmed_start[first_line.len()..]
            .strip_prefix("\r\n")
            .or_else(|| trimmed_start[first_line.len()..].strip_prefix('\n'))
            .unwrap_or(&trimmed_start[first_line.len()..])
            .to_string();

        match directive.as_str() {
            "python" | "py" | "python3" => return CellTarget::Python(body),
            "env" => return CellTarget::Env(body),
            "rust" | "rs" => return CellTarget::Rust(body),
            _ => {}
        }
    }

    CellTarget::Rust(raw_code.to_string())
}
