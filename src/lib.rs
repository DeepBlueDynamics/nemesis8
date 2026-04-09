pub mod cli;
pub mod config;
pub mod docker;
pub mod gateway;
pub mod pokeball;
pub mod provider_def;
pub mod provider_registry;
pub mod remote;
pub mod scheduler;
pub mod session;
pub mod ui;

const GITHUB_RAW: &str = "https://raw.githubusercontent.com/DeepBlueDynamics/nemesis8/main";

/// Files needed to build the Docker image
const BUILD_FILES: &[&str] = &[
    "Dockerfile",
    "requirements.txt",
    "scripts/codex_login.sh",
];

/// Resolve the nemesis8 project directory (Dockerfile, MCP/, etc.)
/// Downloads build files from GitHub on first run if not found locally.
pub fn project_dir_fn() -> std::path::PathBuf {
    // 1. Explicit env var
    if let Ok(dir) = std::env::var("NEMISIS8_PROJECT_DIR") {
        return std::path::PathBuf::from(dir);
    }

    // 2. Compile-time path (works for local/dev builds)
    let compile_time = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if compile_time.join("Dockerfile").is_file() {
        return compile_time;
    }

    // 3. ~/.nemesis8/project — downloaded build files
    let home_dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".nemesis8")
        .join("project");

    if home_dir.join("Dockerfile").is_file() {
        // Always pull latest so the image stays in sync with the release
        eprintln!("[nemesis8] Updating project files from GitHub...");
        let _ = std::process::Command::new("git")
            .args(["-C", &home_dir.display().to_string(), "pull", "--ff-only", "--quiet"])
            .status();
        return home_dir;
    }

    // 4. Download build files from GitHub
    eprintln!("[nemesis8] Downloading build files on first run...");
    if let Err(e) = download_build_files(&home_dir) {
        eprintln!("[nemesis8] warning: failed to download build files: {e}");
        eprintln!("[nemesis8] Set NEMISIS8_PROJECT_DIR to the nemesis8 source directory.");
        return compile_time;
    }

    home_dir
}

/// Download the project by shallow-cloning the repo
fn download_build_files(dest: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    // Remove dest if it exists (clean slate)
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }

    let status = std::process::Command::new("git")
        .args([
            "clone", "--depth", "1",
            "https://github.com/DeepBlueDynamics/nemesis8.git",
            &dest.display().to_string(),
        ])
        .status()?;

    if !status.success() {
        return Err("git clone failed".into());
    }

    eprintln!("[nemesis8] Project downloaded to {}", dest.display());
    Ok(())
}
