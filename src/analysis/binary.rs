//! Pure-Rust Malware Triage, Section Entropy, Hex & Capstone Disassembly Pipeline (`src/analysis/binary.rs`)

use std::path::Path;

use capstone::prelude::*;
use goblin::Object;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Target CPU architecture detected for disassembly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CpuArch {
    X86_64,
    X86_32,
    Arm64,
    Arm32,
    Unknown,
}

impl CpuArch {
    pub fn label(&self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64 (AMD64)",
            Self::X86_32 => "x86 (i386)",
            Self::Arm64 => "AArch64 (ARM64)",
            Self::Arm32 => "ARM32",
            Self::Unknown => "Unknown ISA",
        }
    }
}

/// Metadata and Shannon entropy for a single binary section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinarySection {
    pub name: String,
    pub virtual_address: u64,
    pub file_offset: u64,
    pub size: u64,
    pub entropy: f64,
    pub is_executable: bool,
}

/// A single disassembled CPU instruction from Capstone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisassembledInstruction {
    pub address: u64,
    pub bytes_hex: String,
    pub mnemonic: String,
    pub operands: String,
}

/// A single 16-byte row for the Hex & ASCII inspector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HexRow {
    pub offset: usize,
    pub hex_bytes: String,
    pub ascii: String,
}

/// Complete static triage report for an inspected binary sample.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinaryTriageResult {
    pub sample_path: String,
    pub file_size: usize,
    pub sha256: String,
    pub file_class: String,
    pub arch: CpuArch,
    pub is_64_bit: bool,
    pub is_little_endian: bool,
    pub entry_point: u64,
    pub security_flags: Vec<String>,
    pub sections: Vec<BinarySection>,
    pub imported_symbols: Vec<String>,
    pub exported_symbols: Vec<String>,
    pub disassembly: Vec<DisassembledInstruction>,
    pub strings_sample: Vec<String>,
    pub hex_preview: Vec<HexRow>,
}

impl BinaryTriageResult {
    /// Parses an untrusted binary from disk using pure-Rust `goblin` + `capstone` without executing it.
    pub fn analyze_sample<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let path_ref = path.as_ref();
        let buffer = std::fs::read(path_ref).map_err(|e| format!("IO Storage Failure: {}", e))?;

        Self::analyze_bytes(&buffer, path_ref.to_string_lossy().into_owned())
    }

    /// Parses an in-memory binary buffer.
    pub fn analyze_bytes(buffer: &[u8], sample_path: String) -> Result<Self, String> {
        let mut hasher = Sha256::new();
        hasher.update(buffer);
        let sha256 = hex::encode(hasher.finalize());

        let hex_preview = build_hex_rows(buffer, 0, 64);
        let strings_sample = extract_printable_strings(buffer, 5, 200);

        match Object::parse(buffer).map_err(|e| format!("Goblin Parse Abort: {}", e))? {
            Object::Elf(elf) => {
                let arch = match elf.header.e_machine {
                    goblin::elf::header::EM_X86_64 => CpuArch::X86_64,
                    goblin::elf::header::EM_386 => CpuArch::X86_32,
                    goblin::elf::header::EM_AARCH64 => CpuArch::Arm64,
                    goblin::elf::header::EM_ARM => CpuArch::Arm32,
                    _ => CpuArch::Unknown,
                };

                let mut imported_symbols = Vec::new();
                let mut exported_symbols = Vec::new();

                for sym in elf.dynsyms.iter() {
                    if let Some(name) = elf.dynstrtab.get_at(sym.st_name) {
                        if !name.is_empty() {
                            if sym.is_import() {
                                imported_symbols.push(name.to_string());
                            } else {
                                exported_symbols.push(name.to_string());
                            }
                        }
                    }
                }

                let mut sections = Vec::new();
                let mut code_slice: Option<(&[u8], u64)> = None;

                for sh in &elf.section_headers {
                    let name = elf
                        .shdr_strtab
                        .get_at(sh.sh_name)
                        .unwrap_or("<unnamed>")
                        .to_string();
                    let offset = sh.sh_offset as usize;
                    let size = sh.sh_size as usize;
                    let slice = if offset < buffer.len() {
                        let end = offset.saturating_add(size).min(buffer.len());
                        &buffer[offset..end]
                    } else {
                        &[]
                    };
                    let entropy = compute_shannon_entropy(slice);
                    let is_exec =
                        (sh.sh_flags as u32 & goblin::elf::section_header::SHF_EXECINSTR) != 0;

                    if (name == ".text" || (code_slice.is_none() && is_exec)) && !slice.is_empty() {
                        code_slice = Some((slice, sh.sh_addr));
                    }

                    if !name.is_empty() || size > 0 {
                        sections.push(BinarySection {
                            name,
                            virtual_address: sh.sh_addr,
                            file_offset: sh.sh_offset,
                            size: sh.sh_size,
                            entropy,
                            is_executable: is_exec,
                        });
                    }
                }

                let mut security_flags = Vec::new();
                if elf.header.e_type == goblin::elf::header::ET_DYN {
                    security_flags.push("PIE (Position-Independent)".to_string());
                } else {
                    security_flags.push("No-PIE (Fixed Address)".to_string());
                }
                let has_gnu_stack_nx = elf.program_headers.iter().any(|ph| {
                    ph.p_type == goblin::elf::program_header::PT_GNU_STACK
                        && (ph.p_flags & goblin::elf::program_header::PF_X) == 0
                });
                if has_gnu_stack_nx {
                    security_flags.push("NX (Non-Executable Stack)".to_string());
                }
                if elf.syms.is_empty() {
                    security_flags.push("Stripped Symbols".to_string());
                } else {
                    security_flags.push(format!("Unstripped ({} static syms)", elf.syms.len()));
                }

                let disassembly = if let Some((bytes, base_addr)) = code_slice {
                    disassemble_slice(arch, bytes, base_addr, 120)
                } else {
                    Vec::new()
                };

                Ok(BinaryTriageResult {
                    sample_path,
                    file_size: buffer.len(),
                    sha256,
                    file_class: "Executable and Linkable Format (ELF)".to_string(),
                    arch,
                    is_64_bit: elf.is_64,
                    is_little_endian: elf.little_endian,
                    entry_point: elf.entry,
                    security_flags,
                    sections,
                    imported_symbols,
                    exported_symbols,
                    disassembly,
                    strings_sample,
                    hex_preview,
                })
            }

            Object::PE(pe) => {
                let arch = if pe.is_64 {
                    CpuArch::X86_64
                } else {
                    CpuArch::X86_32
                };

                let imported_symbols: Vec<String> = pe
                    .imports
                    .iter()
                    .map(|imp| format!("{}!{}", imp.dll, imp.name))
                    .collect();

                let exported_symbols: Vec<String> = pe
                    .exports
                    .iter()
                    .filter_map(|exp| exp.name.map(|s| s.to_string()))
                    .collect();

                let mut sections = Vec::new();
                let mut code_slice: Option<(&[u8], u64)> = None;

                for sec in &pe.sections {
                    let name = sec.name().unwrap_or("<noname>").to_string();
                    let offset = sec.pointer_to_raw_data as usize;
                    let size = sec.size_of_raw_data as usize;
                    let slice = if offset < buffer.len() {
                        let end = offset.saturating_add(size).min(buffer.len());
                        &buffer[offset..end]
                    } else {
                        &[]
                    };
                    let entropy = compute_shannon_entropy(slice);
                    let is_exec = (sec.characteristics & 0x2000_0000) != 0;

                    if (name == ".text" || (code_slice.is_none() && is_exec)) && !slice.is_empty() {
                        code_slice =
                            Some((slice, pe.image_base as u64 + sec.virtual_address as u64));
                    }

                    sections.push(BinarySection {
                        name,
                        virtual_address: sec.virtual_address as u64,
                        file_offset: sec.pointer_to_raw_data as u64,
                        size: sec.size_of_raw_data as u64,
                        entropy,
                        is_executable: is_exec,
                    });
                }

                let disassembly = if let Some((bytes, base_addr)) = code_slice {
                    disassemble_slice(arch, bytes, base_addr, 120)
                } else {
                    Vec::new()
                };

                Ok(BinaryTriageResult {
                    sample_path,
                    file_size: buffer.len(),
                    sha256,
                    file_class: if pe.is_64 {
                        "Portable Executable (PE32+ 64-bit)".to_string()
                    } else {
                        "Portable Executable (PE32 32-bit)".to_string()
                    },
                    arch,
                    is_64_bit: pe.is_64,
                    is_little_endian: true,
                    entry_point: pe.entry as u64,
                    security_flags: vec![if pe.is_lib {
                        "DLL Library".to_string()
                    } else {
                        "Executable Image".to_string()
                    }],
                    sections,
                    imported_symbols,
                    exported_symbols,
                    disassembly,
                    strings_sample,
                    hex_preview,
                })
            }

            Object::Mach(_mach) => Ok(BinaryTriageResult {
                sample_path,
                file_size: buffer.len(),
                sha256,
                file_class: "Mach-O Binary".to_string(),
                arch: CpuArch::Unknown,
                is_64_bit: true,
                is_little_endian: true,
                entry_point: 0,
                security_flags: vec!["Mach-O Container".to_string()],
                sections: Vec::new(),
                imported_symbols: Vec::new(),
                exported_symbols: Vec::new(),
                disassembly: Vec::new(),
                strings_sample,
                hex_preview,
            }),

            Object::Archive(_archive) => Ok(BinaryTriageResult {
                sample_path,
                file_size: buffer.len(),
                sha256,
                file_class: "Static Archive (.a / .lib)".to_string(),
                arch: CpuArch::Unknown,
                is_64_bit: true,
                is_little_endian: true,
                entry_point: 0,
                security_flags: vec!["Static Library Archive".to_string()],
                sections: Vec::new(),
                imported_symbols: Vec::new(),
                exported_symbols: Vec::new(),
                disassembly: Vec::new(),
                strings_sample,
                hex_preview,
            }),

            Object::Unknown(magic) => Ok(BinaryTriageResult {
                sample_path,
                file_size: buffer.len(),
                sha256,
                file_class: format!("Raw / Unknown Binary (magic: 0x{magic:x})"),
                arch: CpuArch::X86_64,
                is_64_bit: true,
                is_little_endian: true,
                entry_point: 0,
                security_flags: vec![format!(
                    "Overall Entropy: {:.2} bits/B",
                    compute_shannon_entropy(buffer)
                )],
                sections: vec![BinarySection {
                    name: ".raw".to_string(),
                    virtual_address: 0,
                    file_offset: 0,
                    size: buffer.len() as u64,
                    entropy: compute_shannon_entropy(buffer),
                    is_executable: true,
                }],
                imported_symbols: Vec::new(),
                exported_symbols: Vec::new(),
                disassembly: disassemble_slice(CpuArch::X86_64, buffer, 0, 80),
                strings_sample,
                hex_preview,
            }),

            _ => Ok(BinaryTriageResult {
                sample_path,
                file_size: buffer.len(),
                sha256,
                file_class: "Other Container Format".to_string(),
                arch: CpuArch::Unknown,
                is_64_bit: true,
                is_little_endian: true,
                entry_point: 0,
                security_flags: Vec::new(),
                sections: Vec::new(),
                imported_symbols: Vec::new(),
                exported_symbols: Vec::new(),
                disassembly: Vec::new(),
                strings_sample,
                hex_preview,
            }),
        }
    }
}

/// Computes Shannon entropy in bits per byte (`0.0` to `8.0`).
/// High entropy (`> 7.0`) is a primary indicator of packed or encrypted malware sections.
pub fn compute_shannon_entropy(data: &[u8]) -> f64 {
    if data.is_empty() {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    for &b in data {
        counts[b as usize] += 1;
    }
    let len = data.len() as f64;
    let mut entropy = 0.0;
    for &c in &counts {
        if c > 0 {
            let p = (c as f64) / len;
            entropy -= p * p.log2();
        }
    }
    entropy
}

/// Disassembles up to `max_instructions` from a code slice using Capstone.
pub fn disassemble_slice(
    arch: CpuArch,
    code: &[u8],
    base_addr: u64,
    max_instructions: usize,
) -> Vec<DisassembledInstruction> {
    let slice_len = code.len().min(4096);
    let code_window = &code[..slice_len];

    let cs_result = match arch {
        CpuArch::X86_64 => Capstone::new()
            .x86()
            .mode(arch::x86::ArchMode::Mode64)
            .syntax(arch::x86::ArchSyntax::Intel)
            .detail(false)
            .build(),
        CpuArch::X86_32 => Capstone::new()
            .x86()
            .mode(arch::x86::ArchMode::Mode32)
            .syntax(arch::x86::ArchSyntax::Intel)
            .detail(false)
            .build(),
        CpuArch::Arm64 => Capstone::new()
            .arm64()
            .mode(arch::arm64::ArchMode::Arm)
            .detail(false)
            .build(),
        CpuArch::Arm32 => Capstone::new()
            .arm()
            .mode(arch::arm::ArchMode::Arm)
            .detail(false)
            .build(),
        CpuArch::Unknown => return Vec::new(),
    };

    let Ok(cs) = cs_result else {
        return Vec::new();
    };

    let Ok(insns) = cs.disasm_count(code_window, base_addr, max_instructions) else {
        return Vec::new();
    };

    insns
        .as_ref()
        .iter()
        .map(|i| {
            let bytes_hex = i
                .bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            DisassembledInstruction {
                address: i.address(),
                bytes_hex,
                mnemonic: i.mnemonic().unwrap_or("???").to_string(),
                operands: i.op_str().unwrap_or("").to_string(),
            }
        })
        .collect()
}

/// Formats a slice of bytes into 16-byte Hex + ASCII rows.
pub fn build_hex_rows(data: &[u8], start_offset: usize, max_rows: usize) -> Vec<HexRow> {
    let mut rows = Vec::new();
    let slice = if start_offset < data.len() {
        &data[start_offset..]
    } else {
        return rows;
    };

    for (row_idx, chunk) in slice.chunks(16).take(max_rows).enumerate() {
        let offset = start_offset + row_idx * 16;
        let mut hex_parts = Vec::with_capacity(16);
        let mut ascii = String::with_capacity(16);

        for &b in chunk {
            hex_parts.push(format!("{b:02x}"));
            if b.is_ascii_graphic() || b == b' ' {
                ascii.push(b as char);
            } else {
                ascii.push('.');
            }
        }
        while hex_parts.len() < 16 {
            hex_parts.push("  ".to_string());
        }

        rows.push(HexRow {
            offset,
            hex_bytes: format!("{}  {}", hex_parts[..8].join(" "), hex_parts[8..].join(" ")),
            ascii,
        });
    }

    rows
}

/// Extracts printable ASCII strings of at least `min_len` bytes.
pub fn extract_printable_strings(data: &[u8], min_len: usize, max_count: usize) -> Vec<String> {
    let mut results = Vec::new();
    let mut current = String::new();

    for &b in data {
        if b.is_ascii_graphic() || b == b' ' {
            current.push(b as char);
        } else {
            if current.len() >= min_len {
                results.push(current.clone());
                if results.len() >= max_count {
                    return results;
                }
            }
            current.clear();
        }
    }

    if current.len() >= min_len && results.len() < max_count {
        results.push(current);
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_analyze_self_elf_binary() {
        let exe_path = std::env::current_exe().expect("Failed to locate current test executable");
        let report = BinaryTriageResult::analyze_sample(&exe_path)
            .expect("Should parse current ELF executable cleanly");

        assert!(report.file_class.contains("ELF"));
        assert!(report.is_64_bit);
        assert!(!report.sha256.is_empty());
        assert!(!report.sections.is_empty());
        assert!(!report.disassembly.is_empty());
        assert!(!report.hex_preview.is_empty());
    }

    #[test]
    fn test_shannon_entropy_bounds() {
        let zeros = vec![0u8; 1024];
        assert_eq!(compute_shannon_entropy(&zeros), 0.0);

        let uniform: Vec<u8> = (0..=255).cycle().take(2560).collect();
        let ent = compute_shannon_entropy(&uniform);
        assert!((ent - 8.0).abs() < 1e-6);
    }
}
