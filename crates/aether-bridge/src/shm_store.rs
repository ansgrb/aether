use crate::ffi_import::{
    export_record_batch_to_ffi_ptrs, reconstruct_record_batch_from_boxed_ffi, validate_identifier,
    BridgeError,
};
use arrow_array::{make_array, Array, RecordBatch};
use arrow_buffer::Buffer;
use arrow_data::ArrayData;
use arrow_schema::{DataType, Schema};
use memmap2::Mmap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::{Arc, LazyLock, RwLock};

const SHM_MAGIC: &[u8; 8] = b"AETHARW1";
const ALIGNMENT: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BufferSliceMeta {
    offset: usize,
    len: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ColumnLayoutMeta {
    data_type: DataType,
    len: usize,
    offset: usize,
    null_count: usize,
    null_buffer: Option<BufferSliceMeta>,
    buffers: Vec<BufferSliceMeta>,
    children: Vec<ColumnLayoutMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ShmHeaderMeta {
    schema: Schema,
    num_rows: usize,
    columns: Vec<ColumnLayoutMeta>,
}

/// Entry stored in the orchestrator's global Arrow bridge registry.
#[derive(Debug, Clone)]
pub struct SharedTableEntry {
    pub name: String,
    pub batch: RecordBatch,
    /// Raw `(FFI_ArrowArray*, FFI_ArrowSchema*)` pointer pair as specified in README Section 4.1.
    pub latest_ffi_ptrs: (usize, usize),
    /// Path to the zero-copy POSIX shared-memory backing file (`/dev/shm/...`) for cross-process access.
    pub shm_path: PathBuf,
}

static GLOBAL_ARROW_REGISTRY: LazyLock<RwLock<HashMap<String, SharedTableEntry>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Returns the directory used for zero-copy POSIX shared memory segments (`/dev/shm` on Linux).
pub fn shm_base_dir() -> PathBuf {
    let dev_shm = Path::new("/dev/shm");
    if dev_shm.is_dir() {
        dev_shm.to_path_buf()
    } else {
        std::env::temp_dir()
    }
}

/// Computes the deterministic shared-memory path for a given session namespace and dataframe name.
pub fn shm_path_for(namespace: &str, name: &str) -> PathBuf {
    shm_base_dir().join(format!("aether_bridge_{}_{}.arrowshm", namespace, name))
}

/// Returns the active bridge namespace (defaults to the orchestrator's PID, or `AETHER_BRIDGE_NS` env var).
pub fn active_bridge_namespace() -> String {
    std::env::var("AETHER_BRIDGE_NS").unwrap_or_else(|_| std::process::id().to_string())
}

/// Registers a `RecordBatch` in the orchestrator's zero-copy bridge under `name`.
///
/// 1. Validates `name` as a safe identifier.
/// 2. Exports `(FFI_ArrowArray*, FFI_ArrowSchema*)` raw pointers `(usize, usize)` in O(1) time
///    with zero buffer copying.
/// 3. Computes the deterministic `/dev/shm` path for on-demand cross-process `mmap` export.
pub fn register_shared_record_batch(
    name: &str,
    batch: RecordBatch,
) -> Result<SharedTableEntry, BridgeError> {
    validate_identifier(name)?;

    let latest_ffi_ptrs = export_record_batch_to_ffi_ptrs(&batch)?;
    let ns = active_bridge_namespace();
    let shm_path = shm_path_for(&ns, name);

    let entry = SharedTableEntry {
        name: name.to_string(),
        batch,
        latest_ffi_ptrs,
        shm_path,
    };

    let mut guard = GLOBAL_ARROW_REGISTRY
        .write()
        .expect("GLOBAL_ARROW_REGISTRY poisoned");
    // If replacing an existing entry, reclaim its unconsumed boxed FFI pointers first
    if let Some(prev) = guard.insert(name.to_string(), entry.clone()) {
        unsafe {
            let _ = reconstruct_record_batch_from_boxed_ffi(
                prev.latest_ffi_ptrs.0,
                prev.latest_ffi_ptrs.1,
            );
        }
    }

    Ok(entry)
}

/// Ensures that the shared-memory file in `/dev/shm` is materialized for `name` when an
/// external child process (such as `evcxr`) needs to `mmap` the table across address spaces.
pub fn ensure_shm_file_for(name: &str) -> Result<PathBuf, BridgeError> {
    let (batch, shm_path) = {
        let guard = GLOBAL_ARROW_REGISTRY
            .read()
            .expect("GLOBAL_ARROW_REGISTRY poisoned");
        let entry = guard
            .get(name)
            .ok_or_else(|| BridgeError::NotFound(name.to_string()))?;
        (entry.batch.clone(), entry.shm_path.clone())
    };
    write_batch_to_shm_file(&batch, &shm_path)?;
    Ok(shm_path)
}

/// Returns the `(FFI_ArrowArray*, FFI_ArrowSchema*)` pointer map matching README Section 4.1
/// (`HashMap<String, (usize, usize)>`).
///
/// Each call exports a fresh, reference-counted FFI pointer pair `(usize, usize)` for `name`
/// so that consuming the pointers via `arrow::ffi::from_ffi` never triggers a Use-After-Free
/// on subsequent cell executions.
pub fn export_fresh_ffi_ptrs_for(name: &str) -> Result<(usize, usize), BridgeError> {
    let guard = GLOBAL_ARROW_REGISTRY
        .read()
        .expect("GLOBAL_ARROW_REGISTRY poisoned");
    let entry = guard
        .get(name)
        .ok_or_else(|| BridgeError::NotFound(name.to_string()))?;
    export_record_batch_to_ffi_ptrs(&entry.batch)
}

/// Returns a snapshot of the global `HashMap<String, (usize, usize)>` storing raw
/// `(FFI_ArrowArray, FFI_ArrowSchema)` memory addresses as specified in README Section 4.1.
pub fn get_global_ffi_pointer_map() -> HashMap<String, (usize, usize)> {
    let guard = GLOBAL_ARROW_REGISTRY
        .read()
        .expect("GLOBAL_ARROW_REGISTRY poisoned");
    guard
        .iter()
        .map(|(k, v)| (k.clone(), v.latest_ffi_ptrs))
        .collect()
}

/// Snapshot of all registered table names and their current `(FFI_ArrowArray, FFI_ArrowSchema)`
/// raw memory addresses `(usize, usize)` plus shared-memory file paths.
pub fn list_shared_tables() -> Vec<(String, (usize, usize), PathBuf)> {
    let guard = GLOBAL_ARROW_REGISTRY
        .read()
        .expect("GLOBAL_ARROW_REGISTRY poisoned");
    guard
        .values()
        .map(|e| (e.name.clone(), e.latest_ffi_ptrs, e.shm_path.clone()))
        .collect()
}

/// Writes a `RecordBatch` into a 64-byte-aligned POSIX shared-memory file so any process can
/// reconstruct the `RecordBatch` directly over an `Arc<Mmap>` via `Buffer::from_custom_allocation`.
pub fn write_batch_to_shm_file(batch: &RecordBatch, path: &Path) -> Result<(), BridgeError> {
    let mut raw_buffers: Vec<Buffer> = Vec::new();
    let mut running_offset: usize = 0;

    fn collect_array_data(
        data: &ArrayData,
        raw_buffers: &mut Vec<Buffer>,
        running_offset: &mut usize,
    ) -> ColumnLayoutMeta {
        let mut push_buf = |buf: Buffer| -> BufferSliceMeta {
            let pad = (ALIGNMENT - (*running_offset % ALIGNMENT)) % ALIGNMENT;
            *running_offset += pad;
            let offset = *running_offset;
            let len = buf.len();
            *running_offset += len;
            raw_buffers.push(buf);
            BufferSliceMeta { offset, len }
        };

        let null_buffer = data.nulls().map(|n| push_buf(n.inner().inner().clone()));
        let buffers = data
            .buffers()
            .iter()
            .map(|b| push_buf(b.clone()))
            .collect::<Vec<_>>();
        let children = data
            .child_data()
            .iter()
            .map(|c| collect_array_data(c, raw_buffers, running_offset))
            .collect::<Vec<_>>();

        ColumnLayoutMeta {
            data_type: data.data_type().clone(),
            len: data.len(),
            offset: data.offset(),
            null_count: data.null_count(),
            null_buffer,
            buffers,
            children,
        }
    }

    let columns_meta = batch
        .columns()
        .iter()
        .map(|col| collect_array_data(&col.to_data(), &mut raw_buffers, &mut running_offset))
        .collect::<Vec<_>>();

    let header = ShmHeaderMeta {
        schema: (*batch.schema()).clone(),
        num_rows: batch.num_rows(),
        columns: columns_meta,
    };
    let header_bytes = serde_json::to_vec(&header)?;

    // Header layout:
    // [0..8]:   SHM_MAGIC ("AETHARW1")
    // [8..16]:  u64 little-endian header_bytes.len()
    // [16..16+header_len]: header_bytes
    // [padding to 64-byte boundary]: 0u8
    // [data_base..]: 64-byte aligned buffer slices
    let prefix_len = 16 + header_bytes.len();
    let data_base = (prefix_len + (ALIGNMENT - 1)) & !(ALIGNMENT - 1);

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    }

    file.write_all(SHM_MAGIC)?;
    file.write_all(&(header_bytes.len() as u64).to_le_bytes())?;
    file.write_all(&header_bytes)?;
    if data_base > prefix_len {
        let zeros = vec![0u8; data_base - prefix_len];
        file.write_all(&zeros)?;
    }

    let mut written_data_offset = 0usize;
    let zeros_64 = [0u8; ALIGNMENT];
    for buf in raw_buffers {
        let slice: &[u8] = buf.as_slice();
        let pad = (ALIGNMENT - (written_data_offset % ALIGNMENT)) % ALIGNMENT;
        if pad > 0 {
            file.write_all(&zeros_64[..pad])?;
            written_data_offset += pad;
        }
        file.write_all(slice)?;
        written_data_offset += slice.len();
    }
    file.flush()?;
    Ok(())
}

/// Memory-maps a POSIX shared-memory file produced by [`write_batch_to_shm_file`] and constructs
/// an `arrow_array::RecordBatch` backed directly by the `Arc<Mmap>` via
/// `Buffer::from_custom_allocation` (**zero memory copy, O(1) latency regardless of table size**).
pub fn read_batch_from_shm_file_zero_copy(path: &Path) -> Result<RecordBatch, BridgeError> {
    let file = File::open(path)?;
    let mmap = Arc::new(unsafe { Mmap::map(&file)? });

    if mmap.len() < 16 || &mmap[0..8] != SHM_MAGIC {
        return Err(BridgeError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid Aether shared-memory Arrow magic header",
        )));
    }

    let header_len = u64::from_le_bytes(mmap[8..16].try_into().unwrap()) as usize;
    let prefix_len = 16 + header_len;
    if mmap.len() < prefix_len {
        return Err(BridgeError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "truncated Aether shared-memory Arrow header",
        )));
    }

    let header: ShmHeaderMeta = serde_json::from_slice(&mmap[16..prefix_len])?;
    let data_base = (prefix_len + (ALIGNMENT - 1)) & !(ALIGNMENT - 1);

    fn slice_to_zero_copy_buffer(
        mmap: &Arc<Mmap>,
        data_base: usize,
        meta: &BufferSliceMeta,
    ) -> Result<Buffer, BridgeError> {
        let start = data_base + meta.offset;
        let end = start + meta.len;
        if end > mmap.len() {
            return Err(BridgeError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "shared-memory buffer slice out of bounds",
            )));
        }
        let ptr = if meta.len == 0 {
            NonNull::dangling()
        } else {
            unsafe { NonNull::new_unchecked(mmap.as_ptr().add(start) as *mut u8) }
        };
        let buf =
            unsafe { Buffer::from_custom_allocation(ptr, meta.len, Arc::clone(mmap) as Arc<_>) };
        Ok(buf)
    }

    fn build_array_data(
        mmap: &Arc<Mmap>,
        data_base: usize,
        col: &ColumnLayoutMeta,
    ) -> Result<ArrayData, BridgeError> {
        let null_bit_buffer = match &col.null_buffer {
            Some(nb) => Some(slice_to_zero_copy_buffer(mmap, data_base, nb)?),
            None => None,
        };
        let buffers = col
            .buffers
            .iter()
            .map(|b| slice_to_zero_copy_buffer(mmap, data_base, b))
            .collect::<Result<Vec<_>, _>>()?;
        let child_data = col
            .children
            .iter()
            .map(|c| build_array_data(mmap, data_base, c))
            .collect::<Result<Vec<_>, _>>()?;

        let data = unsafe {
            ArrayData::new_unchecked(
                col.data_type.clone(),
                col.len,
                Some(col.null_count),
                null_bit_buffer,
                col.offset,
                buffers,
                child_data,
            )
        };
        Ok(data)
    }

    let columns = header
        .columns
        .iter()
        .map(|col_meta| {
            let data = build_array_data(&mmap, data_base, col_meta)?;
            Ok(make_array(data))
        })
        .collect::<Result<Vec<Arc<dyn Array>>, BridgeError>>()?;

    let schema = Arc::new(header.schema);
    if columns.is_empty() {
        let options =
            arrow_array::RecordBatchOptions::new().with_row_count(Some(header.num_rows));
        Ok(RecordBatch::try_new_with_options(schema, columns, &options)?)
    } else {
        Ok(RecordBatch::try_new(schema, columns)?)
    }
}
