use std::path::PathBuf;

use clap::Parser;

/// Configuration for the CodeForge REST server.
#[derive(Debug, Clone, Parser)]
#[command(name = "codeforge-server", about = "CodeForge REST API server")]
pub struct ServerConfig {
    /// Host address to bind.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// Port to listen on.
    #[arg(long, default_value = "3000")]
    pub port: u16,

    /// Path to the project root (must have a `.codeforge/` index).
    #[arg(default_value = ".")]
    pub root_path: PathBuf,
}
