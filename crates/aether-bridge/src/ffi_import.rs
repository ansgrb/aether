use arrow::ffi::{from_ffi, FFI_ArrowArray, FFI_ArrowSchema};
use arrow_array::{make_array, Array, RecordBatch, StructArray};
use arrow_data::ArrayData;
use arrow_schema::{DataType, Field, Schema};
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BridgeError {
    #[error("invalid dataframe identifier '{0}': must be a valid Rust identifier ([a-zA-Z_][a-zA-Z0-9_]*)")]
    InvalidIdentifier(String),
    #[error("null FFI pointer provided for Arrow C-Data Interface")]
    NullPointer,
    #[error("Arrow error: {0}")]
    Arrow(#[from] arrow_schema::ArrowError),
    #[error("I/O error in shared memory bridge: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error in bridge metadata: {0}")]
    Json(#[from] serde_json::Error),
    #[error("shared dataframe '{0}' not found in bridge registry")]
    NotFound(String),
    #[error("imported Arrow array is not a StructArray (RecordBatch)")]
    NotStructArray,
}

/// Validates that a shared dataframe name is a safe Rust/Python variable identifier.
/// Prevents arbitrary code injection when generating the `evcxr` Rust preamble.
pub fn validate_identifier(name: &str) -> Result<(), BridgeError> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(BridgeError::InvalidIdentifier(name.to_string()));
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(BridgeError::InvalidIdentifier(name.to_string()));
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(BridgeError::InvalidIdentifier(name.to_string()));
    }
    // Reject Rust reserved keywords that cannot be used as `let <ident>` bindings
    const KEYWORDS: &[&str] = &[
        "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn",
        "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
        "return", "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe",
        "use", "where", "while", "async", "await", "dyn",
    ];
    if KEYWORDS.contains(&name) {
        return Err(BridgeError::InvalidIdentifier(name.to_string()));
    }
    Ok(())
}

/// Imports an `arrow_array::RecordBatch` from raw `FFI_ArrowArray` and `FFI_ArrowSchema`
/// pointers allocated via the Apache Arrow C-Data Interface.
///
/// # Safety
/// `array_ptr` and `schema_ptr` must be valid, non-null pointers to initialized
/// `FFI_ArrowArray` and `FFI_ArrowSchema` structures following the Arrow C-Data Interface
/// specification. Ownership of the `FFI_ArrowArray` is moved into the returned `RecordBatch`.
pub unsafe fn import_record_batch_from_ffi_ptrs(
    array_ptr: usize,
    schema_ptr: usize,
) -> Result<RecordBatch, BridgeError> {
    if array_ptr == 0 || schema_ptr == 0 {
        return Err(BridgeError::NullPointer);
    }

    let ffi_array_ptr = array_ptr as *mut FFI_ArrowArray;
    let ffi_schema_ptr = schema_ptr as *const FFI_ArrowSchema;

    // Take ownership of the FFI_ArrowArray by replacing it with an empty struct so drop
    // doesn't double-release if the caller frees the outer container.
    let ffi_array = std::ptr::replace(ffi_array_ptr, FFI_ArrowArray::empty());
    let ffi_schema = &*ffi_schema_ptr;

    let array_data: ArrayData = unsafe { from_ffi(ffi_array, ffi_schema)? };
    let struct_array = StructArray::from(array_data);
    Ok(RecordBatch::from(&struct_array))
}

/// Imports an `arrow_array::RecordBatch` from heap-allocated `Box<FFI_ArrowArray>` and
/// `Box<FFI_ArrowSchema>` raw pointers (`(usize, usize)`), freeing the `Box` wrappers after
/// transferring ownership to Arrow's `ArrayData`.
///
/// # Safety
/// Both `array_box_ptr` and `schema_box_ptr` must have been created by [`export_record_batch_to_ffi_ptrs`].
pub unsafe fn reconstruct_record_batch_from_boxed_ffi(
    array_box_ptr: usize,
    schema_box_ptr: usize,
) -> Result<RecordBatch, BridgeError> {
    if array_box_ptr == 0 || schema_box_ptr == 0 {
        return Err(BridgeError::NullPointer);
    }
    let ffi_array = *Box::from_raw(array_box_ptr as *mut FFI_ArrowArray);
    let ffi_schema = *Box::from_raw(schema_box_ptr as *mut FFI_ArrowSchema);

    let array_data: ArrayData = unsafe { from_ffi(ffi_array, &ffi_schema)? };
    let struct_array = StructArray::from(array_data);
    Ok(RecordBatch::from(&struct_array))
}

/// Exports an existing `RecordBatch` into a fresh pair of heap-allocated
/// `(FFI_ArrowArray*, FFI_ArrowSchema*)` raw addresses `(usize, usize)`.
///
/// Because `RecordBatch` buffers are reference-counted (`Arc`), this operation is **O(1)
/// zero-copy** and produces an independent FFI handle pair each time it is called, eliminating
/// Use-After-Free when the same dataframe is consumed multiple times!
pub fn export_record_batch_to_ffi_ptrs(
    batch: &RecordBatch,
) -> Result<(usize, usize), BridgeError> {
    let struct_array: StructArray = batch.clone().into();
    let data: ArrayData = struct_array.into_data();
    let fields = batch.schema().fields().clone();
    let struct_type = DataType::Struct(fields);

    let ffi_array = FFI_ArrowArray::new(&data);
    let ffi_schema = FFI_ArrowSchema::try_from(&struct_type)?;

    let array_ptr = Box::into_raw(Box::new(ffi_array)) as usize;
    let schema_ptr = Box::into_raw(Box::new(ffi_schema)) as usize;

    Ok((array_ptr, schema_ptr))
}

/// Exports a single column or arbitrary `ArrayData` + `Field` from C-Data FFI pointers.
/// Used when importing via `PyCapsule` (`__arrow_c_array__`).
///
/// # Safety
/// `array_ptr` and `schema_ptr` must point to valid `FFI_ArrowArray` and `FFI_ArrowSchema`.
pub unsafe fn import_array_from_c_ptrs(
    array_ptr: *mut FFI_ArrowArray,
    schema_ptr: *const FFI_ArrowSchema,
) -> Result<RecordBatch, BridgeError> {
    if array_ptr.is_null() || schema_ptr.is_null() {
        return Err(BridgeError::NullPointer);
    }
    let ffi_array = std::ptr::replace(array_ptr, FFI_ArrowArray::empty());
    let ffi_schema = &*schema_ptr;

    let field = Field::try_from(ffi_schema)?;
    let array_data = from_ffi(ffi_array, ffi_schema)?;
    let array = make_array(array_data);

    if let Some(struct_arr) = array.as_any().downcast_ref::<StructArray>() {
        // Preserve top-level field metadata if present
        if let DataType::Struct(fields) = field.data_type() {
            let schema = Arc::new(Schema::new_with_metadata(
                fields.clone(),
                field.metadata().clone(),
            ));
            let columns = struct_arr.columns().to_vec();
            return Ok(RecordBatch::try_new(schema, columns)?);
        }
        return Ok(RecordBatch::from(struct_arr));
    }

    Err(BridgeError::NotStructArray)
}
