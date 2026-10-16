use serde_json::{json, Value};
use std::collections::HashMap;

/// Rust source code injected into the `evcxr` context at startup to provide:
/// 1. The `AetherDisplay` trait and rich output wrappers (`Html`, `Svg`, `Markdown`) for Sprint 5.
/// 2. The zero-copy `AetherFrame` POSIX `mmap` reader so shared Python DataFrames are immediately
///    accessible in `evcxr` cells as zero-copy slices (`&[i64]`, `&[f64]`, etc.) without waiting
///    for external crate compilation.
pub const EVCXR_STARTUP_PRELUDE: &str = r#"
pub trait AetherDisplay {
    fn mime_type(&self) -> &'static str;
    fn render_content(&self) -> String;
}

#[derive(Debug, Clone)]
pub struct Html<T: AsRef<str>>(pub T);
impl<T: AsRef<str>> AetherDisplay for Html<T> {
    fn mime_type(&self) -> &'static str { "text/html" }
    fn render_content(&self) -> String { self.0.as_ref().to_string() }
}
impl<T: AsRef<str>> Html<T> {
    pub fn evcxr_display(&self) {
        println!(
            "EVCXR_BEGIN_CONTENT {}\n{}\nEVCXR_END_CONTENT",
            AetherDisplay::mime_type(self),
            AetherDisplay::render_content(self)
        );
    }
}

#[derive(Debug, Clone)]
pub struct Svg<T: AsRef<str>>(pub T);
impl<T: AsRef<str>> AetherDisplay for Svg<T> {
    fn mime_type(&self) -> &'static str { "image/svg+xml" }
    fn render_content(&self) -> String { self.0.as_ref().to_string() }
}
impl<T: AsRef<str>> Svg<T> {
    pub fn evcxr_display(&self) {
        println!(
            "EVCXR_BEGIN_CONTENT {}\n{}\nEVCXR_END_CONTENT",
            AetherDisplay::mime_type(self),
            AetherDisplay::render_content(self)
        );
    }
}

#[derive(Debug, Clone)]
pub struct Markdown<T: AsRef<str>>(pub T);
impl<T: AsRef<str>> AetherDisplay for Markdown<T> {
    fn mime_type(&self) -> &'static str { "text/markdown" }
    fn render_content(&self) -> String { self.0.as_ref().to_string() }
}
impl<T: AsRef<str>> Markdown<T> {
    pub fn evcxr_display(&self) {
        println!(
            "EVCXR_BEGIN_CONTENT {}\n{}\nEVCXR_END_CONTENT",
            AetherDisplay::mime_type(self),
            AetherDisplay::render_content(self)
        );
    }
}

#[derive(Debug)]
pub struct AetherColumnMeta {
    pub name: String,
    pub data_offset: usize,
    pub byte_len: usize,
    pub elem_len: usize,
}

pub struct AetherFrame {
    ptr: *const u8,
    map_len: usize,
    num_rows: usize,
    ffi_ptrs: (usize, usize),
    columns: std::collections::HashMap<String, AetherColumnMeta>,
}

unsafe impl Send for AetherFrame {}
unsafe impl Sync for AetherFrame {}

impl AetherFrame {
    pub fn open_zero_copy(path: &str, ffi_ptrs: (usize, usize)) -> Result<Self, String> {
        use std::os::unix::io::AsRawFd;
        let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let map_len = file.metadata().map_err(|e| e.to_string())?.len() as usize;
        if map_len < 16 {
            return Err("invalid Aether shared memory file".to_string());
        }
        unsafe extern "C" {
            fn mmap(addr: *mut std::ffi::c_void, length: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut std::ffi::c_void;
        }
        let raw = unsafe { mmap(std::ptr::null_mut(), map_len, 1 /* PROT_READ */, 1 /* MAP_SHARED */, file.as_raw_fd(), 0) };
        if raw as isize == -1 {
            return Err("mmap failed on shared Arrow segment".to_string());
        }
        let ptr = raw as *const u8;
        let slice = unsafe { std::slice::from_raw_parts(ptr, map_len) };
        if &slice[0..8] != b"AETHARW1" {
            return Err("magic mismatch in shared Arrow segment".to_string());
        }
        let mut len_bytes = [0u8; 8];
        len_bytes.copy_from_slice(&slice[8..16]);
        let header_len = u64::from_le_bytes(len_bytes) as usize;
        let prefix_len = 16 + header_len;
        let data_base = (prefix_len + 63) & !63;
        let header_str = std::str::from_utf8(&slice[16..prefix_len]).map_err(|e| e.to_string())?;

        // Lightweight extraction of num_rows and column buffer offsets from the JSON header
        let num_rows = Self::extract_usize_after(header_str, "\"num_rows\":").unwrap_or(0);
        let columns = Self::parse_columns(header_str, data_base);

        Ok(Self { ptr, map_len, num_rows, ffi_ptrs, columns })
    }

    fn extract_usize_after(haystack: &str, needle: &str) -> Option<usize> {
        let idx = haystack.find(needle)? + needle.len();
        let rest = haystack[idx..].trim_start();
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse().ok()
    }

    fn parse_columns(header: &str, data_base: usize) -> std::collections::HashMap<String, AetherColumnMeta> {
        let mut map = std::collections::HashMap::new();
        let mut field_names = Vec::new();
        if let Some(fields_pos) = header.find("\"fields\":[") {
            let mut rest = &header[fields_pos..];
            while let Some(np) = rest.find("\"name\":\"") {
                let after = &rest[np + 8..];
                if let Some(end_q) = after.find('"') {
                    field_names.push(after[..end_q].to_string());
                    rest = &after[end_q + 1..];
                } else {
                    break;
                }
            }
        }
        if let Some(cols_pos) = header.find("\"columns\":[") {
            let mut rest = &header[cols_pos..];
            for name in field_names {
                if let Some(len_val) = Self::extract_usize_after(rest, "\"len\":") {
                    if let Some(bufs_pos) = rest.find("\"buffers\":[") {
                        let after_bufs = &rest[bufs_pos + 11..];
                        let buf_offset = Self::extract_usize_after(after_bufs, "\"offset\":").unwrap_or(0);
                        let buf_len = Self::extract_usize_after(after_bufs, "\"len\":").unwrap_or(0);
                        map.insert(name, AetherColumnMeta {
                            name: String::new(),
                            data_offset: data_base + buf_offset,
                            byte_len: buf_len,
                            elem_len: len_val,
                        });
                        rest = after_bufs;
                    }
                }
            }
        }
        map
    }

    pub fn num_rows(&self) -> usize { self.num_rows }
    pub fn num_columns(&self) -> usize { self.columns.len() }
    pub fn ffi_ptrs(&self) -> (usize, usize) { self.ffi_ptrs }

    pub fn col_i64(&self, name: &str) -> &'static [i64] {
        let col = self.columns.get(name).expect("column not found in AetherFrame");
        unsafe {
            std::slice::from_raw_parts(
                self.ptr.add(col.data_offset) as *const i64,
                col.byte_len / std::mem::size_of::<i64>(),
            )
        }
    }

    pub fn col_f64(&self, name: &str) -> &'static [f64] {
        let col = self.columns.get(name).expect("column not found in AetherFrame");
        unsafe {
            std::slice::from_raw_parts(
                self.ptr.add(col.data_offset) as *const f64,
                col.byte_len / std::mem::size_of::<f64>(),
            )
        }
    }
}
"#;

/// Converts `evcxr::EvalOutputs` into a Jupyter MIME bundle (`HashMap<String, Value>`) and
/// accompanying display metadata.
///
/// Also inspects plain-text outputs for embedded SVG or HTML payloads so expressions that
/// evaluate to raw SVG strings (e.g., from `plotters`) or HTML fragments render graphically.
pub fn format_mime_bundle(
    outputs_by_mime: &HashMap<String, String>,
) -> (HashMap<String, Value>, HashMap<String, Value>) {
    let mut data = HashMap::new();
    let metadata = HashMap::new();

    for (mime, content) in outputs_by_mime {
        data.insert(mime.clone(), json!(content));
    }

    // If only text/plain was emitted, check if the evaluated value is an unquoted or Debug-quoted
    // SVG / HTML document and promote it into a rich MIME bundle automatically.
    if data.len() == 1 {
        if let Some(Value::String(plain)) = data.get("text/plain").cloned() {
            let trimmed = plain.trim();
            let unquoted = trimmed
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .map(|s| s.replace("\\\"", "\"").replace("\\n", "\n"))
                .unwrap_or_else(|| trimmed.to_string());

            let candidate = unquoted.trim();
            if candidate.starts_with("<svg") && candidate.ends_with("</svg>") {
                data.insert("image/svg+xml".to_string(), json!(candidate));
            } else if (candidate.starts_with("<!DOCTYPE html")
                || candidate.starts_with("<html")
                || candidate.starts_with("<div")
                || candidate.starts_with("<table"))
                && candidate.ends_with('>')
            {
                data.insert("text/html".to_string(), json!(candidate));
            }
        }
    }

    (data, metadata)
}
