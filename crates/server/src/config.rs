use std::path::PathBuf;

use clap::Parser;

/// Configuration for the Codixing REST server.
#[derive(Debug, Clone, Parser)]
#[command(name = "codixing-server", about = "Codixing REST API server")]
pub struct ServerConfig {
    /// Host address to bind.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// Port to listen on.
    #[arg(long, default_value = "3000")]
    pub port: u16,

    /// Path to the project root (must have a `.codixing/` index).
    #[arg(default_value = ".")]
    pub root_path: PathBuf,
}
