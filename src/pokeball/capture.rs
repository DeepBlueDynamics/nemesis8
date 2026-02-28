use anyhow::{Context, Result};
use std::path::Path;

use super::git::{self, InputKind};
use super::spec::{
    ComposeInfo, ComposeService, EnvVar, MonorepoInfo, PokeballSpec, ProjectMeta, Source,
};
use super::store::PokeballStore;

/// Detected project runtime info
#[derive(Debug)]
struct DetectedRuntime {
    language: String,
    base_image: String,
    package_manager: Option<String>,
    node_version: Option<String>,
    install_cmd: Option<String>,
    build_cmd: Option<String>,
    system_packages: Vec<String>,
    exclude: Vec<String>,
}

/// Capture from a string that may be a local path or git URL.
/// Returns `(spec, name)`.
pub fn capture_from_string(input: &str) -> Result<(PokeballSpec, String)> {
    match git::classify_input(input) {
        InputKind::GitUrl { url, name } => {
            let store = PokeballStore::open()?;
            let clone_dir = store.source_dir(&name);
            git::clone(&url, &clone_dir)?;

            let mut spec = capture(&clone_dir)?;
            // Override the source to be a git source
            spec.source = Source::new_git(&url, clone_dir.to_string_lossy().as_ref());
            spec.metadata.name = name.clone();
            Ok((spec, name))
        }
        InputKind::LocalPath(path) => {
            let p = Path::new(&path);
            let spec = capture(p)?;
            let name = spec.metadata.name.clone();
            Ok((spec, name))
        }
    }
}

/// Scan a project directory and generate a PokeballSpec
pub fn capture(project_path: &Path) -> Result<PokeballSpec> {
    let project_path = project_path
        .canonicalize()
        .with_context(|| format!("resolving path: {}", project_path.display()))?;

    let name = project_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string());

    let runtime = detect_runtime(&project_path)?;

    let mut spec = PokeballSpec::default_for(&name, &project_path.to_string_lossy());
    spec.runtime.language = runtime.language;
    spec.runtime.base_image = runtime.base_image;
    spec.runtime.package_manager = runtime.package_manager;
    spec.runtime.node_version = runtime.node_version;
    spec.build.install_cmd = runtime.install_cmd;
    spec.build.build_cmd = runtime.build_cmd;
    spec.build.system_packages = runtime.system_packages;
    spec.build.exclude = runtime.exclude;

    // Check for existing Dockerfile
    if project_path.join("Dockerfile").is_file() {
        spec.runtime.existing_dockerfile = Some("Dockerfile".to_string());
    }

    // Enhanced project meta detection
    spec.meta = detect_project_meta(&project_path);

    Ok(spec)
}

// ── Enhanced project meta detection ──

/// Detect all project metadata: env vars, compose, monorepo, dockerfiles
fn detect_project_meta(path: &Path) -> ProjectMeta {
    ProjectMeta {
        env_vars: detect_env_files(path),
        compose: detect_compose(path),
        monorepo: detect_monorepo(path),
        dockerfiles: detect_all_dockerfiles(path),
    }
}

/// Parse .env.example, .env.sample, .env.template for env var declarations
fn detect_env_files(path: &Path) -> Vec<EnvVar> {
    let candidates = [".env.example", ".env.sample", ".env.template"];
    let mut vars = Vec::new();

    for name in &candidates {
        let file = path.join(name);
        if let Ok(content) = std::fs::read_to_string(&file) {
            for line in content.lines() {
                let line = line.trim();
                // Skip comments and empty lines
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((key, value)) = line.split_once('=') {
                    let key = key.trim().to_string();
                    let value = value.trim().to_string();
                    let default = if value.is_empty() { None } else { Some(value.clone()) };
                    let required = value.is_empty();
                    // Avoid duplicates
                    if !vars.iter().any(|v: &EnvVar| v.name == key) {
                        vars.push(EnvVar {
                            name: key,
                            default,
                            required,
                        });
                    }
                }
            }
            break; // Only use the first env file found
        }
    }

    vars
}

/// Detect docker-compose.yml / compose.yaml and extract services
fn detect_compose(path: &Path) -> Option<ComposeInfo> {
    let candidates = [
        "docker-compose.yml",
        "docker-compose.yaml",
        "compose.yml",
        "compose.yaml",
    ];

    for name in &candidates {
        let file = path.join(name);
        if let Ok(content) = std::fs::read_to_string(&file) {
            let doc: serde_yaml::Value = serde_yaml::from_str(&content).ok()?;
            let services_map = doc.get("services")?.as_mapping()?;

            let mut services = Vec::new();
            for (svc_name, svc_def) in services_map {
                let name = svc_name.as_str().unwrap_or("unknown").to_string();

                let ports: Vec<String> = svc_def
                    .get("ports")
                    .and_then(|p| p.as_sequence())
                    .map(|seq| {
                        seq.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();

                let volumes: Vec<String> = svc_def
                    .get("volumes")
                    .and_then(|v| v.as_sequence())
                    .map(|seq| {
                        seq.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();

                services.push(ComposeService {
                    name,
                    ports,
                    volumes,
                });
            }

            return Some(ComposeInfo {
                file: name.to_string(),
                services,
            });
        }
    }

    None
}

/// Detect monorepo structure (npm/pnpm/yarn workspaces)
fn detect_monorepo(path: &Path) -> Option<MonorepoInfo> {
    // Check pnpm-workspace.yaml first
    let pnpm_ws = path.join("pnpm-workspace.yaml");
    if let Ok(content) = std::fs::read_to_string(&pnpm_ws) {
        let doc: serde_yaml::Value = serde_yaml::from_str(&content).ok()?;
        let packages: Vec<String> = doc
            .get("packages")
            .and_then(|p| p.as_sequence())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        if !packages.is_empty() {
            return Some(MonorepoInfo {
                tool: "pnpm-workspaces".to_string(),
                packages,
            });
        }
    }

    // Check package.json workspaces
    let pkg_path = path.join("package.json");
    if let Ok(content) = std::fs::read_to_string(&pkg_path) {
        if let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(ws) = pkg.get("workspaces") {
                let packages: Vec<String> = if let Some(arr) = ws.as_array() {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                } else if let Some(obj) = ws.as_object() {
                    // yarn workspaces with packages field
                    obj.get("packages")
                        .and_then(|p| p.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };

                if !packages.is_empty() {
                    // Determine which tool (check lock files)
                    let tool = if path.join("yarn.lock").is_file() {
                        "yarn-workspaces"
                    } else {
                        "npm-workspaces"
                    };
                    return Some(MonorepoInfo {
                        tool: tool.to_string(),
                        packages,
                    });
                }
            }
        }
    }

    None
}

/// Find all Dockerfile* files recursively (up to 3 levels deep)
fn detect_all_dockerfiles(path: &Path) -> Vec<String> {
    let mut results = Vec::new();
    walk_for_dockerfiles(path, path, 0, 3, &mut results);
    results.sort();
    results
}

fn walk_for_dockerfiles(
    base: &Path,
    current: &Path,
    depth: usize,
    max_depth: usize,
    results: &mut Vec<String>,
) {
    if depth > max_depth {
        return;
    }
    let entries = match std::fs::read_dir(current) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        // Skip hidden dirs and node_modules
        if name.starts_with('.') || name == "node_modules" || name == "target" || name == ".git" {
            continue;
        }

        if path.is_file() && name.starts_with("Dockerfile") {
            if let Ok(rel) = path.strip_prefix(base) {
                results.push(rel.to_string_lossy().to_string());
            }
        } else if path.is_dir() {
            walk_for_dockerfiles(base, &path, depth + 1, max_depth, results);
        }
    }
}

// ── Runtime detection (unchanged) ──

/// Detect project runtime from files in the directory
fn detect_runtime(path: &Path) -> Result<DetectedRuntime> {
    // Try each detector in order of specificity
    if let Some(rt) = detect_node(path)? {
        return Ok(rt);
    }
    if let Some(rt) = detect_python(path) {
        return Ok(rt);
    }
    if let Some(rt) = detect_rust(path) {
        return Ok(rt);
    }
    if let Some(rt) = detect_go(path) {
        return Ok(rt);
    }

    // Fallback: generic
    Ok(DetectedRuntime {
        language: "unknown".to_string(),
        base_image: "debian:bookworm-slim".to_string(),
        package_manager: None,
        node_version: None,
        install_cmd: None,
        build_cmd: None,
        system_packages: vec!["git".to_string(), "curl".to_string(), "tini".to_string()],
        exclude: vec![".git".to_string(), "*.log".to_string(), ".env*".to_string()],
    })
}

/// Detect Node.js / TypeScript projects
fn detect_node(path: &Path) -> Result<Option<DetectedRuntime>> {
    let pkg_path = path.join("package.json");
    if !pkg_path.is_file() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(&pkg_path).context("reading package.json")?;
    let pkg: serde_json::Value =
        serde_json::from_str(&content).context("parsing package.json")?;

    // Detect language: TypeScript if tsconfig exists or typescript in deps
    let has_typescript = path.join("tsconfig.json").is_file()
        || pkg
            .pointer("/devDependencies/typescript")
            .is_some()
        || pkg.pointer("/dependencies/typescript").is_some();

    let language = if has_typescript {
        "typescript"
    } else {
        "javascript"
    };

    // Detect package manager
    let package_manager = if path.join("pnpm-lock.yaml").is_file() {
        "pnpm"
    } else if path.join("yarn.lock").is_file() {
        "yarn"
    } else if path.join("bun.lockb").is_file() || path.join("bun.lock").is_file() {
        "bun"
    } else {
        "npm"
    };

    // Detect node version from engines field
    let node_version = pkg
        .pointer("/engines/node")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Build install command
    let install_cmd = match package_manager {
        "pnpm" => "pnpm install --frozen-lockfile".to_string(),
        "yarn" => "yarn install --frozen-lockfile".to_string(),
        "bun" => "bun install --frozen-lockfile".to_string(),
        _ => "npm ci".to_string(),
    };

    // Detect build command from scripts
    let build_cmd = detect_build_scripts(&pkg, package_manager);

    // Base image
    let base_image = "node:22-bookworm-slim".to_string();

    // System packages — include package manager if not npm
    let system_packages = vec!["git".to_string(), "curl".to_string(), "tini".to_string()];
    if package_manager == "pnpm" {
        // pnpm is installed via corepack
    }

    let exclude = vec![
        "node_modules".to_string(),
        ".git".to_string(),
        "*.log".to_string(),
        ".env*".to_string(),
        ".next".to_string(),
        "dist".to_string(),
        "build".to_string(),
    ];

    Ok(Some(DetectedRuntime {
        language: language.to_string(),
        base_image,
        package_manager: Some(package_manager.to_string()),
        node_version,
        install_cmd: Some(install_cmd),
        build_cmd,
        system_packages,
        exclude,
    }))
}

/// Detect build scripts from package.json
fn detect_build_scripts(pkg: &serde_json::Value, pm: &str) -> Option<String> {
    let scripts = pkg.get("scripts")?;
    let run_prefix = match pm {
        "pnpm" => "pnpm",
        "yarn" => "yarn",
        "bun" => "bun run",
        _ => "npm run",
    };

    let mut cmds = Vec::new();

    // Primary build
    if scripts.get("build").is_some() {
        cmds.push(format!("{run_prefix} build"));
    }

    // UI build (monorepo patterns)
    for script_name in &["ui:build", "client:build", "frontend:build"] {
        if scripts.get(*script_name).is_some() {
            cmds.push(format!("{run_prefix} {script_name}"));
        }
    }

    if cmds.is_empty() {
        None
    } else {
        Some(cmds.join(" && "))
    }
}

/// Detect Python projects
fn detect_python(path: &Path) -> Option<DetectedRuntime> {
    let has_pyproject = path.join("pyproject.toml").is_file();
    let has_requirements = path.join("requirements.txt").is_file();
    let has_setup_py = path.join("setup.py").is_file();

    if !has_pyproject && !has_requirements && !has_setup_py {
        return None;
    }

    let package_manager = if has_pyproject {
        // Check for poetry
        if let Ok(content) = std::fs::read_to_string(path.join("pyproject.toml")) {
            if content.contains("[tool.poetry]") {
                "poetry"
            } else if content.contains("[tool.uv]") || path.join("uv.lock").is_file() {
                "uv"
            } else {
                "pip"
            }
        } else {
            "pip"
        }
    } else {
        "pip"
    };

    let install_cmd = match package_manager {
        "poetry" => "poetry install --no-interaction".to_string(),
        "uv" => "uv sync".to_string(),
        _ => "pip install -r requirements.txt".to_string(),
    };

    Some(DetectedRuntime {
        language: "python".to_string(),
        base_image: "python:3.12-slim-bookworm".to_string(),
        package_manager: Some(package_manager.to_string()),
        node_version: None,
        install_cmd: Some(install_cmd),
        build_cmd: None,
        system_packages: vec!["git".to_string(), "curl".to_string(), "tini".to_string()],
        exclude: vec![
            ".git".to_string(),
            "*.log".to_string(),
            ".env*".to_string(),
            "__pycache__".to_string(),
            ".venv".to_string(),
            "*.pyc".to_string(),
        ],
    })
}

/// Detect Rust projects
fn detect_rust(path: &Path) -> Option<DetectedRuntime> {
    if !path.join("Cargo.toml").is_file() {
        return None;
    }

    Some(DetectedRuntime {
        language: "rust".to_string(),
        base_image: "rust:1-bookworm".to_string(),
        package_manager: Some("cargo".to_string()),
        node_version: None,
        install_cmd: Some("cargo fetch".to_string()),
        build_cmd: Some("cargo build --release".to_string()),
        system_packages: vec!["git".to_string(), "curl".to_string(), "tini".to_string()],
        exclude: vec![
            ".git".to_string(),
            "*.log".to_string(),
            ".env*".to_string(),
            "target".to_string(),
        ],
    })
}

/// Detect Go projects
fn detect_go(path: &Path) -> Option<DetectedRuntime> {
    if !path.join("go.mod").is_file() {
        return None;
    }

    Some(DetectedRuntime {
        language: "go".to_string(),
        base_image: "golang:1.23-bookworm".to_string(),
        package_manager: Some("go".to_string()),
        node_version: None,
        install_cmd: Some("go mod download".to_string()),
        build_cmd: Some("go build -o /work/app .".to_string()),
        system_packages: vec!["git".to_string(), "curl".to_string(), "tini".to_string()],
        exclude: vec![
            ".git".to_string(),
            "*.log".to_string(),
            ".env*".to_string(),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_node_npm() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"test","scripts":{"build":"tsc"}}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("package-lock.json"), "{}").unwrap();

        let spec = capture(dir.path()).unwrap();
        assert_eq!(spec.runtime.language, "javascript");
        assert_eq!(spec.runtime.package_manager.as_deref(), Some("npm"));
        assert_eq!(spec.build.install_cmd.as_deref(), Some("npm ci"));
    }

    #[test]
    fn test_detect_node_pnpm_typescript() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"test","devDependencies":{"typescript":"^5"},"scripts":{"build":"tsc","ui:build":"vite build"}}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("pnpm-lock.yaml"), "").unwrap();
        std::fs::write(dir.path().join("tsconfig.json"), "{}").unwrap();

        let spec = capture(dir.path()).unwrap();
        assert_eq!(spec.runtime.language, "typescript");
        assert_eq!(spec.runtime.package_manager.as_deref(), Some("pnpm"));
        assert_eq!(
            spec.build.install_cmd.as_deref(),
            Some("pnpm install --frozen-lockfile")
        );
        assert!(spec.build.build_cmd.as_deref().unwrap().contains("pnpm build"));
        assert!(spec.build.build_cmd.as_deref().unwrap().contains("ui:build"));
    }

    #[test]
    fn test_detect_python() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("requirements.txt"), "flask\n").unwrap();

        let spec = capture(dir.path()).unwrap();
        assert_eq!(spec.runtime.language, "python");
        assert_eq!(spec.runtime.base_image, "python:3.12-slim-bookworm");
    }

    #[test]
    fn test_detect_rust() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"test\"\nversion = \"0.1.0\"",
        )
        .unwrap();

        let spec = capture(dir.path()).unwrap();
        assert_eq!(spec.runtime.language, "rust");
        assert_eq!(spec.build.build_cmd.as_deref(), Some("cargo build --release"));
    }

    #[test]
    fn test_detect_go() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("go.mod"), "module example.com/test\n").unwrap();

        let spec = capture(dir.path()).unwrap();
        assert_eq!(spec.runtime.language, "go");
    }

    #[test]
    fn test_detect_unknown() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "# Hello").unwrap();

        let spec = capture(dir.path()).unwrap();
        assert_eq!(spec.runtime.language, "unknown");
    }

    #[test]
    fn test_existing_dockerfile_detected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Dockerfile"), "FROM node:22").unwrap();
        std::fs::write(dir.path().join("package.json"), r#"{"name":"test"}"#).unwrap();

        let spec = capture(dir.path()).unwrap();
        assert_eq!(
            spec.runtime.existing_dockerfile.as_deref(),
            Some("Dockerfile")
        );
    }

    #[test]
    fn test_detect_env_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".env.example"),
            "# Database\nDB_HOST=localhost\nDB_PASSWORD=\nAPI_KEY=\n",
        )
        .unwrap();

        let vars = detect_env_files(dir.path());
        assert_eq!(vars.len(), 3);
        assert_eq!(vars[0].name, "DB_HOST");
        assert_eq!(vars[0].default.as_deref(), Some("localhost"));
        assert!(!vars[0].required);
        assert_eq!(vars[1].name, "DB_PASSWORD");
        assert!(vars[1].required);
    }

    #[test]
    fn test_detect_compose() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("docker-compose.yml"),
            "services:\n  web:\n    ports:\n      - \"3000:3000\"\n  db:\n    volumes:\n      - \"./data:/var/lib/mysql\"\n",
        )
        .unwrap();

        let info = detect_compose(dir.path()).unwrap();
        assert_eq!(info.file, "docker-compose.yml");
        assert_eq!(info.services.len(), 2);
        assert_eq!(info.services[0].name, "web");
        assert_eq!(info.services[0].ports, vec!["3000:3000"]);
    }

    #[test]
    fn test_detect_monorepo_pnpm() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n  - 'apps/*'\n",
        )
        .unwrap();

        let info = detect_monorepo(dir.path()).unwrap();
        assert_eq!(info.tool, "pnpm-workspaces");
        assert_eq!(info.packages, vec!["packages/*", "apps/*"]);
    }

    #[test]
    fn test_detect_monorepo_npm() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"root","workspaces":["packages/*","apps/*"]}"#,
        )
        .unwrap();

        let info = detect_monorepo(dir.path()).unwrap();
        assert_eq!(info.tool, "npm-workspaces");
        assert_eq!(info.packages, vec!["packages/*", "apps/*"]);
    }

    #[test]
    fn test_detect_all_dockerfiles() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Dockerfile"), "FROM node:22").unwrap();
        std::fs::write(dir.path().join("Dockerfile.dev"), "FROM node:22").unwrap();
        let sub = dir.path().join("services").join("api");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("Dockerfile"), "FROM python:3.12").unwrap();

        let files = detect_all_dockerfiles(dir.path());
        assert!(files.contains(&"Dockerfile".to_string()));
        assert!(files.contains(&"Dockerfile.dev".to_string()));
        // The nested one should be found too (within 3 levels)
        assert!(files.iter().any(|f| f.contains("api")));
    }

    #[test]
    fn test_capture_from_string_local() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), r#"{"name":"test"}"#).unwrap();

        let (spec, name) = capture_from_string(&dir.path().to_string_lossy()).unwrap();
        assert_eq!(spec.runtime.language, "javascript");
        assert!(!name.is_empty());
    }

    #[test]
    fn test_project_meta_populated() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), r#"{"name":"test"}"#).unwrap();
        std::fs::write(dir.path().join("Dockerfile"), "FROM node:22").unwrap();
        std::fs::write(
            dir.path().join(".env.example"),
            "PORT=3000\nSECRET=\n",
        )
        .unwrap();

        let spec = capture(dir.path()).unwrap();
        assert!(!spec.meta.is_empty());
        assert_eq!(spec.meta.env_vars.len(), 2);
        assert!(spec.meta.dockerfiles.contains(&"Dockerfile".to_string()));
    }
}
