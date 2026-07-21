pub mod app_def;
pub mod app_registry;
pub mod charon;
pub mod cli;
pub mod config;
pub mod controlroom;
pub mod daemon;
pub mod docker;
pub mod gateway;
pub mod mcp_def;
pub mod mcp_registry;
pub mod monitor;
pub mod names;
pub mod paths;
pub mod picker;
pub mod provider_def;
pub mod provider_registry;
pub mod registry;
pub mod remote;
pub mod runtime;
pub mod scheduler;
pub mod search;
pub mod service_def;
pub mod service_registry;
pub mod session;
pub mod theme;
pub mod tunnel;
pub mod ui;
pub mod activity;
pub mod pulse;
pub mod collectors;
pub mod event_index;
pub mod event_store;
pub mod logpane;
pub mod tool_events;
pub mod trainer_api;
pub mod transcript;



/// Heuristic: is this the nemesis8 agent-image Dockerfile, and not some
/// unrelated project's Dockerfile that merely shares the filename? The nemesis8
/// Dockerfile builds `nemesis8-entry` FROM `nemesis8-base`; no foreign project
/// carries those tokens. Guards `project_dir_fn` against tagging a stranger's
/// image as `nemesis8:latest` just because the cwd happens to have a Dockerfile.
fn is_nemesis8_dockerfile(path: &std::path::Path) -> bool {
    match std::fs::read_to_string(path) {
        Ok(contents) => contents.contains("nemesis8-entry") || contents.contains("nemesis8-base"),
        Err(_) => false,
    }
}

/// Resolve the nemesis8 project directory (Dockerfile, MCP/, etc.)
/// Downloads build files from GitHub on first run if not found locally.
pub fn project_dir_fn() -> std::path::PathBuf {
    // 1. Explicit env var
    if let Ok(dir) = std::env::var("NEMESIS8_PROJECT_DIR") {
        return std::path::PathBuf::from(dir);
    }

    // 2. Current working directory — but ONLY if its Dockerfile is actually
    // nemesis8's. A bare "cwd has a Dockerfile" check is a footgun: running
    // `n8 build` (or triggering an auto-build) from any OTHER repo that happens
    // to ship a Dockerfile (e.g. sibling project `skiff`) would build THAT
    // project and stamp the result `nemesis8:latest` (DEFAULT_IMAGE is fixed),
    // silently clobbering the real agent image. Verify the Dockerfile carries a
    // nemesis8 build marker before trusting cwd; warn (don't silently build the
    // wrong thing) when a foreign Dockerfile is present.
    if let Ok(cwd) = std::env::current_dir() {
        let df = cwd.join("Dockerfile");
        if df.is_file() {
            if is_nemesis8_dockerfile(&df) {
                return cwd;
            }
            eprintln!(
                "[nemesis8] ignoring Dockerfile in {} — not the nemesis8 build \
                 (no nemesis8-base/nemesis8-entry marker). Run from the nemesis8 \
                 repo or set NEMESIS8_PROJECT_DIR to avoid clobbering nemesis8:latest.",
                cwd.display()
            );
        }
    }

    // 3. Compile-time path (works for local/dev builds)
    let compile_time = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if compile_time.join("Dockerfile").is_file() {
        return compile_time;
    }

    // 4. ~/.nemesis8/project — the build context fetched for INSTALLED users
    // (binary only, no repo/Dockerfile). We pin the fetch to THIS binary's
    // version so `n8 build` always produces a container that matches the binary
    // that built it — never a floating `main` that has drifted ahead.
    let version = env!("CARGO_PKG_VERSION");
    let home_dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".nemesis8")
        .join("project");
    let stamp = home_dir.join(".nemesis8-version");

    // Reuse a cached context only if it's complete AND matches our version.
    let cached_version = std::fs::read_to_string(&stamp).ok();
    let cache_valid = home_dir.join("Dockerfile").is_file()
        && cached_version.as_deref().map(str::trim) == Some(version);
    if cache_valid {
        return home_dir;
    }

    // Missing or version-mismatched → fetch the tag matching this binary.
    eprintln!("[nemesis8] fetching v{version} build context (installed binary, no local repo)...");
    if let Err(e) = download_build_files(&home_dir, version) {
        eprintln!("[nemesis8] warning: failed to fetch build context: {e}");
        eprintln!("[nemesis8] Set NEMESIS8_PROJECT_DIR to a nemesis8 source checkout to build offline.");
        return compile_time;
    }
    let _ = std::fs::write(&stamp, version);

    home_dir
}

/// Fetch the nemesis8 build context for tag `v{version}` into `dest`.
///
/// Primary path is a plain HTTPS download of the GitHub source tarball + local
/// unpack — NO `git` required, since installed users usually don't have it. The
/// archive's top-level `nemesis8-{version}/` directory is stripped so `dest`
/// holds the repo contents directly. Falls back to a shallow `git clone` of the
/// tag only if the HTTPS path fails and `git` happens to be available.
fn download_build_files(
    dest: &std::path::Path,
    version: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // Clean slate — a half-extracted or stale context must not linger.
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    std::fs::create_dir_all(dest)?;

    let tag = format!("v{version}");
    match fetch_tarball(dest, &tag) {
        Ok(()) => {
            eprintln!("[nemesis8] build context v{version} unpacked to {}", dest.display());
            Ok(())
        }
        Err(e) => {
            eprintln!("[nemesis8] tarball fetch failed ({e}); trying git clone of {tag}...");
            let status = std::process::Command::new("git")
                .args([
                    "clone", "--depth", "1", "--branch", &tag,
                    "https://github.com/DeepBlueDynamics/nemesis8.git",
                    &dest.display().to_string(),
                ])
                .status()
                .map_err(|ge| format!("git not available ({ge}) after tarball error: {e}"))?;
            if !status.success() {
                return Err(format!("git clone of {tag} failed (tarball error: {e})").into());
            }
            eprintln!("[nemesis8] build context v{version} cloned to {}", dest.display());
            Ok(())
        }
    }
}

/// Download `archive/refs/tags/{tag}.tar.gz` over HTTPS and extract into `dest`,
/// stripping the single `nemesis8-*/` top-level directory GitHub wraps it in.
fn fetch_tarball(dest: &std::path::Path, tag: &str) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "https://github.com/DeepBlueDynamics/nemesis8/archive/refs/tags/{tag}.tar.gz"
    );
    let resp = reqwest::blocking::Client::builder()
        .user_agent(concat!("nemesis8/", env!("CARGO_PKG_VERSION")))
        .build()?
        .get(&url)
        .send()?
        .error_for_status()?;
    let bytes = resp.bytes()?;

    let gz = flate2::read::GzDecoder::new(std::io::Cursor::new(bytes));
    let mut archive = tar::Archive::new(gz);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        // Strip the leading `nemesis8-<tag>/` component so files land at dest root.
        let stripped: std::path::PathBuf = path.components().skip(1).collect();
        if stripped.as_os_str().is_empty() {
            continue;
        }
        let out = dest.join(stripped);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        entry.unpack(&out)?;
    }
    Ok(())
}

pub mod telemetry;

pub mod telemetry_web;
