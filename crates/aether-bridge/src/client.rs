use crate::ffi_import::{reconstruct_record_batch_from_boxed_ffi, BridgeError};
use crate::shm_store::{
    active_bridge_namespace, export_fresh_ffi_ptrs_for, read_batch_from_shm_file_zero_copy,
    shm_path_for,
};
use arrow_array::RecordBatch;
use std::path::Path;

/// High-level zero-copy loader usable both in-process (via `FFI_ArrowArray` / `FFI_ArrowSchema`
/// pointers) and across processes (such as inside `evcxr` subprocesses via `mmap` over `/dev/shm`).
pub fn load_shared_record_batch(name: &str) -> Result<RecordBatch, BridgeError> {
    // Fast path 1: If running inside the same process as the orchestrator registry,
    // export a fresh pair of (FFI_ArrowArray*, FFI_ArrowSchema*) raw pointers and reconstruct
    // via `arrow::ffi::from_ffi`.
    if let Ok((array_ptr, schema_ptr)) = export_fresh_ffi_ptrs_for(name) {
        return unsafe { reconstruct_record_batch_from_boxed_ffi(array_ptr, schema_ptr) };
    }

    // Fast path 2: If running in an `evcxr` child worker subprocess, zero-copy memory-map
    // the shared memory file from `/dev/shm`.
    let ns = active_bridge_namespace();
    let shm_path = shm_path_for(&ns, name);
    if shm_path.exists() {
        return read_batch_from_shm_file_zero_copy(&shm_path);
    }

    Err(BridgeError::NotFound(name.to_string()))
}

/// Loads a shared `RecordBatch` directly from an explicit shared-memory file path.
pub fn load_shared_record_batch_from_path(path: impl AsRef<Path>) -> Result<RecordBatch, BridgeError> {
    read_batch_from_shm_file_zero_copy(path.as_ref())
}

/// Generates the Rust preamble code injected into `evcxr` when shared tables are available.
///
/// The preamble binds a lazy/zero-copy loader function `__aether_load_batch("<name>")` and
/// provides the exact `FFI_ArrowArray` / `FFI_ArrowSchema` pointer metadata in comments/constants
/// for transparency and debugging.
pub fn generate_evcxr_preamble_for_table(
    name: &str,
    ffi_ptrs: (usize, usize),
    shm_path: &Path,
) -> String {
    let escaped_path = shm_path.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        r#"const __AETHER_FFI_PTRS_{upper}: (usize, usize) = ({arr_ptr:#x}, {schema_ptr:#x});
let {name}: aether_bridge::arrow_array::RecordBatch = aether_bridge::load_shared_record_batch_from_path("{path}").expect("failed to zero-copy map shared Arrow table '{name}'");"#,
        upper = name.to_ascii_uppercase(),
        arr_ptr = ffi_ptrs.0,
        schema_ptr = ffi_ptrs.1,
        name = name,
        path = escaped_path,
    )
}
