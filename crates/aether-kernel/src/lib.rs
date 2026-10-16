pub mod env_manager;
pub mod multiplexer;
pub mod orchestrator;

pub use env_manager::{EnvManager, EnvParseError};
pub use multiplexer::{parse_cell_magic, CellTarget};
pub use orchestrator::{run_kernel_server, CellOutcome, PolyglotOrchestrator};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cell_magic_routing() {
        assert_eq!(
            parse_cell_magic("let x = 10;\nx + 5"),
            CellTarget::Rust("let x = 10;\nx + 5".to_string())
        );

        assert_eq!(
            parse_cell_magic("%%rust\nlet y = 20;"),
            CellTarget::Rust("let y = 20;".to_string())
        );

        assert_eq!(
            parse_cell_magic("%%python\nimport polars as pl\nprint(1)"),
            CellTarget::Python("import polars as pl\nprint(1)".to_string())
        );

        assert_eq!(
            parse_cell_magic("%%env\nDATABASE_URL = \"postgres://localhost/db\""),
            CellTarget::Env("DATABASE_URL = \"postgres://localhost/db\"".to_string())
        );
    }

    #[test]
    fn test_env_manager_toml_and_json_with_redaction() {
        let mgr = EnvManager::new();
        let summary = mgr
            .apply_env_block(
                r#"
                APP_MODE = "production"
                DATABASE_URL = "postgres://admin:secret@localhost:5432/aether"
                [redis]
                HOST = "127.0.0.1"
                AUTH_TOKEN = "tok_12345"
                "#,
            )
            .expect("TOML env block should parse");

        let map: std::collections::HashMap<_, _> = summary.into_iter().collect();
        assert_eq!(map.get("APP_MODE").map(|s| s.as_str()), Some("production"));
        assert_eq!(
            map.get("DATABASE_URL").map(|s| s.as_str()),
            Some("[REDACTED]")
        );
        assert_eq!(
            map.get("redis_AUTH_TOKEN").map(|s| s.as_str()),
            Some("[REDACTED]")
        );

        let snap = mgr.snapshot();
        assert_eq!(
            snap.get("DATABASE_URL").map(|s| s.as_str()),
            Some("postgres://admin:secret@localhost:5432/aether")
        );
        assert_eq!(
            snap.get("redis_HOST").map(|s| s.as_str()),
            Some("127.0.0.1")
        );
    }
}
