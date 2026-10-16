use aether_kernel::{run_kernel_server, PolyglotOrchestrator};
use aether_protocol::KernelConnectionInfo;
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "rust-notebook-core",
    version,
    about = "Aether (RustyNote) — Polyglot Rust & Python Jupyter Kernel with Zero-Copy Arrow Bridge"
)]
struct CliArgs {
    /// Path to the Jupyter kernel connection file (`kernel.json`)
    #[arg(short = 'f', long = "connection-file", value_name = "FILE")]
    connection_file: PathBuf,
}

fn main() -> anyhow::Result<()> {
    // CRITICAL: Must be called at the very start of `main()` before starting Tokio or PyO3
    // so `evcxr` child worker subprocesses enter the `evcxr` runtime loop cleanly.
    aether_engine_rust::runtime_hook();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("warn,aether_kernel=info,aether_protocol=info,aether_bridge=info,aether_engine_rust=info,aether_engine_python=info,salsa=off")),
        )
        .init();

    let args = CliArgs::parse();
    let conn_info = KernelConnectionInfo::from_file(&args.connection_file)?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    rt.block_on(async move {
        let orchestrator = Arc::new(PolyglotOrchestrator::new()?);
        run_kernel_server(conn_info, orchestrator).await
    })
}
