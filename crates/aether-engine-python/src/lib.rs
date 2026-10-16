pub mod magic_bridge;
pub mod runtime;
pub mod stdio;

pub use runtime::{PythonEngine, PythonEngineError, PythonExecutionOutput};
pub use stdio::IoPubStreamWriter;

#[cfg(test)]
mod tests {
    use super::*;
    use aether_bridge::load_shared_record_batch;
    use std::collections::HashMap;

    #[test]
    fn test_python_state_persistence_and_stdout_capture() {
        let engine = PythonEngine::new().expect("failed to init PythonEngine");
        let env = HashMap::new();

        let out1 = engine
            .execute_cell("x = 21\nprint(f'hello {x * 2}')", &env, None, None)
            .expect("cell 1 should succeed");
        assert_eq!(out1.stdout_collected, "hello 42\n");
        assert!(out1.data.is_empty());

        let out2 = engine
            .execute_cell("x + 100", &env, None, None)
            .expect("cell 2 should succeed");
        assert_eq!(
            out2.data.get("text/plain").and_then(|v| v.as_str()),
            Some("121")
        );
    }

    #[test]
    fn test_python_polars_to_rust_arrow_bridge() {
        let engine = PythonEngine::new().expect("failed to init PythonEngine");
        let env = HashMap::new();

        let code = r#"
import polars as pl
df = pl.DataFrame({"a": [10, 20, 30], "b": [1.5, 2.5, 3.5]})
__share_to_rust__("shared_from_py", df)
"#;
        engine
            .execute_cell(code, &env, None, None)
            .expect("sharing polars DataFrame should succeed");

        let batch = load_shared_record_batch("shared_from_py")
            .expect("should load shared RecordBatch in Rust");
        assert_eq!(batch.num_rows(), 3);
        assert_eq!(batch.num_columns(), 2);
    }
}
