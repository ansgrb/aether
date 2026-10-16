use aether_bridge::arrow::ffi::{FFI_ArrowArray, FFI_ArrowSchema};
use aether_bridge::{
    import_array_from_c_ptrs, import_record_batch_from_ffi_ptrs, register_shared_record_batch,
    validate_identifier,
};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyCapsule, PyTuple};
use tracing::info;

/// Python magic function `__share_to_rust__(name: str, df: Any) -> tuple[int, int]`
/// injected into the embedded Python runtime by the orchestrator (README Section 4.1).
///
/// Extracts `FFI_ArrowArray` and `FFI_ArrowSchema` pointers from Polars DataFrames,
/// PyArrow Tables/RecordBatches, or any object implementing `__arrow_c_array__` / `_export_to_c`,
/// reconstructs an `arrow_array::RecordBatch` in zero-copy memory, and stores its
/// `(FFI_ArrowArray*, FFI_ArrowSchema*)` raw addresses in the orchestrator's global registry.
#[pyfunction]
pub fn __share_to_rust__(
    _py: Python<'_>,
    name: &str,
    df: &Bound<'_, PyAny>,
) -> PyResult<(usize, usize)> {
    validate_identifier(name).map_err(|e| PyValueError::new_err(e.to_string()))?;

    // Resolve the input dataframe/table into an Arrow object exposing either `_export_to_c`
    // or `__arrow_c_array__`.
    let arrow_obj = resolve_to_arrow_record_batch_obj(df)?;

    let batch = if arrow_obj.hasattr("_export_to_c")? {
        let mut ffi_array = FFI_ArrowArray::empty();
        let mut ffi_schema = FFI_ArrowSchema::empty();
        let array_addr = (&raw mut ffi_array) as usize;
        let schema_addr = (&raw mut ffi_schema) as usize;

        arrow_obj.call_method1("_export_to_c", (array_addr, schema_addr))?;

        unsafe {
            import_record_batch_from_ffi_ptrs(array_addr, schema_addr)
                .map_err(|e| PyRuntimeError::new_err(format!("Arrow FFI import failed: {e}")))?
        }
    } else if arrow_obj.hasattr("__arrow_c_array__")? {
        let tuple_any = arrow_obj.call_method0("__arrow_c_array__")?;
        let tuple = tuple_any.downcast::<PyTuple>()?;
        if tuple.len() != 2 {
            return Err(PyRuntimeError::new_err(
                "__arrow_c_array__ must return a 2-tuple of (schema_capsule, array_capsule)",
            ));
        }
        let schema_item = tuple.get_item(0)?;
        let array_item = tuple.get_item(1)?;
        let schema_capsule = schema_item.downcast::<PyCapsule>()?;
        let array_capsule = array_item.downcast::<PyCapsule>()?;

        let schema_ptr = schema_capsule.pointer() as *const FFI_ArrowSchema;
        let array_ptr = array_capsule.pointer() as *mut FFI_ArrowArray;

        unsafe {
            import_array_from_c_ptrs(array_ptr, schema_ptr).map_err(|e| {
                PyRuntimeError::new_err(format!("Arrow PyCapsule import failed: {e}"))
            })?
        }
    } else {
        return Err(PyValueError::new_err(
            "object does not support Arrow C-Data Interface (.to_arrow(), _export_to_c, or __arrow_c_array__)",
        ));
    };

    let rows = batch.num_rows();
    let cols = batch.num_columns();
    let entry = register_shared_record_batch(name, batch)
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

    info!(
        table = %name,
        rows = rows,
        cols = cols,
        ffi_array_ptr = entry.latest_ffi_ptrs.0,
        ffi_schema_ptr = entry.latest_ffi_ptrs.1,
        "Registered zero-copy Arrow table from Python -> Rust"
    );

    Ok(entry.latest_ffi_ptrs)
}

fn resolve_to_arrow_record_batch_obj<'py>(
    df: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    // 1. Direct PyArrow RecordBatch or object that already exposes `__arrow_c_array__`
    if df.hasattr("__arrow_c_array__")? {
        return Ok(df.clone());
    }

    // 2. Polars DataFrame (`df.rechunk().to_arrow()`) or Pandas DataFrame (`pa.RecordBatch.from_pandas(df)`)
    let maybe_table = if df.hasattr("to_arrow")? {
        let contiguous = if df.hasattr("rechunk")? {
            df.call_method0("rechunk")?
        } else {
            df.clone()
        };
        contiguous.call_method0("to_arrow")?
    } else {
        df.clone()
    };

    if maybe_table.hasattr("__arrow_c_array__")? {
        return Ok(maybe_table);
    }

    // 3. PyArrow Table -> combine_chunks() -> to_batches()[0] (or to_struct_array() if 0 batches)
    if maybe_table.hasattr("to_batches")? {
        let combined = if maybe_table.hasattr("combine_chunks")? {
            maybe_table.call_method0("combine_chunks")?
        } else {
            maybe_table.clone()
        };
        let batches = combined.call_method0("to_batches")?;
        let len: usize = batches.len()?;
        if len > 0 {
            return batches.get_item(0);
        }
    }

    // 4. Direct `_export_to_c`
    if maybe_table.hasattr("_export_to_c")? {
        return Ok(maybe_table);
    }

    Ok(df.clone())
}
