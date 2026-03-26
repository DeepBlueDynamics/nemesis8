pub mod cli;
pub mod config;
pub mod docker;
pub mod gateway;
pub mod pokeball;
pub mod remote;
pub mod scheduler;
pub mod session;
pub mod ui;

/// Resolve the nemisis8 project directory (Dockerfile, MCP/, Cargo.toml, src/)
pub fn project_dir_fn() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("NEMISIS8_PROJECT_DIR") {
        return std::path::PathBuf::from(dir);
    }
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}
