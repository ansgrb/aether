pub mod client;
pub mod ffi_import;
pub mod shm_store;

pub use arrow;
pub use arrow_array;
pub use arrow_buffer;
pub use arrow_data;
pub use arrow_schema;

pub use client::{
    generate_evcxr_preamble_for_table, load_shared_record_batch,
    load_shared_record_batch_from_path,
};
pub use ffi_import::{
    export_record_batch_to_ffi_ptrs, import_array_from_c_ptrs, import_record_batch_from_ffi_ptrs,
    reconstruct_record_batch_from_boxed_ffi, validate_identifier, BridgeError,
};
pub use shm_store::{
    active_bridge_namespace, ensure_shm_file_for, export_fresh_ffi_ptrs_for,
    get_global_ffi_pointer_map, list_shared_tables, read_batch_from_shm_file_zero_copy,
    register_shared_record_batch, shm_base_dir, shm_path_for, write_batch_to_shm_file,
    SharedTableEntry,
};

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{Float64Array, Int64Array, StringArray};
    use arrow_schema::{DataType, Field, Schema};
    use std::sync::Arc;

    fn sample_batch() -> arrow_array::RecordBatch {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("score", DataType::Float64, true),
            Field::new("label", DataType::Utf8, false),
        ]));
        let ids = Int64Array::from(vec![10, 20, 30, 40]);
        let scores = Float64Array::from(vec![Some(1.5), None, Some(3.25), Some(9.0)]);
        let labels = StringArray::from(vec!["alpha", "beta", "gamma", "delta"]);

        arrow_array::RecordBatch::try_new(
            schema,
            vec![Arc::new(ids), Arc::new(scores), Arc::new(labels)],
        )
        .unwrap()
    }

    #[test]
    fn test_identifier_validation() {
        assert!(validate_identifier("my_df").is_ok());
        assert!(validate_identifier("_df_2").is_ok());
        assert!(validate_identifier("123bad").is_err());
        assert!(validate_identifier("fn").is_err());
        assert!(validate_identifier("my_df; std::process::exit(1);").is_err());
    }

    #[test]
    fn test_in_process_ffi_c_data_interface_roundtrip() {
        let batch = sample_batch();
        let (array_ptr, schema_ptr) = export_record_batch_to_ffi_ptrs(&batch).unwrap();
        assert_ne!(array_ptr, 0);
        assert_ne!(schema_ptr, 0);

        let reconstructed =
            unsafe { reconstruct_record_batch_from_boxed_ffi(array_ptr, schema_ptr).unwrap() };
        assert_eq!(reconstructed.num_rows(), 4);
        assert_eq!(reconstructed.num_columns(), 3);
        assert_eq!(reconstructed, batch);
    }

    #[test]
    fn test_register_and_zero_copy_shm_roundtrip_multiple_reads() {
        let batch = sample_batch();
        let entry = register_shared_record_batch("test_df_multi", batch.clone()).unwrap();

        // Read 1 via in-process FFI pointers
        let loaded_1 = load_shared_record_batch("test_df_multi").unwrap();
        assert_eq!(loaded_1, batch);

        // Read 2 via in-process FFI pointers (proving no Use-After-Free on repeated reads!)
        let loaded_2 = load_shared_record_batch("test_df_multi").unwrap();
        assert_eq!(loaded_2, batch);

        // Read 3 via cross-process zero-copy mmap file
        let shm_path = ensure_shm_file_for("test_df_multi").unwrap();
        let loaded_shm = load_shared_record_batch_from_path(&shm_path).unwrap();
        assert_eq!(loaded_shm, batch);

        let _ = std::fs::remove_file(&entry.shm_path);
    }
}
