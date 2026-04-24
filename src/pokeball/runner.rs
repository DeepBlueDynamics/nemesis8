use anyhow::Result;
use std::process::Command;

/// Available container runtime
#[derive(Debug)]
pub enum ContainerRuntime {
    Docker {
        version: String,
        compose: bool,
    },
    Podman {
        version: String,
        /// true when a podman machine is running (macOS); false on Linux (native)
        machine_running: bool,
    },
    Wsl2Docker {
        distro: String,
        version: String,
    },
    Colima {
        version: String,
    },
}

/// Result of probing for container runtimes
#[derive(Debug)]
pub struct RuntimeProbe {
    pub available: Vec<ContainerRuntime>,
    pub recommended: Option<String>,
    pub errors: Vec<String>,
}

/// Detect available container runtimes
pub fn detect_runtime() -> RuntimeProbe {
    let mut probe = RuntimeProbe {
        available: Vec::new(),
        recommended: None,
        errors: Vec::new(),
    };

    // Try Docker
    match probe_docker() {
        Ok(rt) => {
            probe.recommended = Some("docker".to_string());
            probe.available.push(rt);
        }
        Err(e) => probe.errors.push(format!("Docker: {e}")),
    }

    // Try Podman (all platforms)
    match probe_podman() {
        Ok(rt) => {
            if probe.recommended.is_none() {
                probe.recommended = Some("podman".to_string());
            }
            probe.available.push(rt);
        }
        Err(e) => probe.errors.push(format!("Podman: {e}")),
    }

    // On Windows, try WSL2 Docker
    #[cfg(target_os = "windows")]
    if probe.available.is_empty() {
        match probe_wsl2_docker() {
            Ok(rt) => {
                if probe.recommended.is_none() {
                    probe.recommended = Some("wsl2".to_string());
                }
                probe.available.push(rt);
            }
            Err(e) => probe.errors.push(format!("WSL2: {e}")),
        }
    }

    // On macOS, try Colima
    #[cfg(target_os = "macos")]
    if probe.available.is_empty() {
        match probe_colima() {
            Ok(rt) => {
                if probe.recommended.is_none() {
                    probe.recommended = Some("colima".to_string());
                }
                probe.available.push(rt);
            }
            Err(e) => probe.errors.push(format!("Colima: {e}")),
        }
    }

    probe
}

/// Probe for local Docker
fn probe_docker() -> Result<ContainerRuntime> {
    let output = Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .output()
        .map_err(|e| anyhow::anyhow!("docker not found: {e}"))?;

    if !output.status.success() {
        anyhow::bail!(
            "docker version failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // Check for compose
    let compose = Command::new("docker")
        .args(["compose", "version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    Ok(ContainerRuntime::Docker { version, compose })
}

/// Probe for Podman
fn probe_podman() -> Result<ContainerRuntime> {
    let output = Command::new("podman")
        .args(["version", "--format", "{{.Client.Version}}"])
        .output()
        .map_err(|e| anyhow::anyhow!("podman not found: {e}"))?;

    if !output.status.success() {
        anyhow::bail!(
            "podman version failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // On macOS, check if a machine is running (Linux runs natively, no machine needed)
    #[cfg(target_os = "macos")]
    let machine_running = {
        Command::new("podman")
            .args(["machine", "list", "--format", "{{.Running}}"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("true"))
            .unwrap_or(false)
    };
    #[cfg(not(target_os = "macos"))]
    let machine_running = false; // Linux/Windows: no machine concept (native or WSL)

    Ok(ContainerRuntime::Podman { version, machine_running })
}

/// Probe for Docker inside WSL2
#[cfg(target_os = "windows")]
fn probe_wsl2_docker() -> Result<ContainerRuntime> {
    // List WSL distros
    let output = Command::new("wsl")
        .args(["--list", "--quiet"])
        .output()
        .map_err(|e| anyhow::anyhow!("wsl not found: {e}"))?;

    if !output.status.success() {
        anyhow::bail!("wsl --list failed");
    }

    let distros = String::from_utf8_lossy(&output.stdout);
    let distro = distros
        .lines()
        .map(|l| l.trim().trim_matches('\0'))
        .find(|l| !l.is_empty())
        .ok_or_else(|| anyhow::anyhow!("no WSL distros found"))?
        .to_string();

    // Try docker inside WSL
    let output = Command::new("wsl")
        .args(["-d", &distro, "--", "docker", "version", "--format", "{{.Server.Version}}"])
        .output()
        .map_err(|e| anyhow::anyhow!("docker in WSL failed: {e}"))?;

    if !output.status.success() {
        anyhow::bail!("docker not available in WSL distro '{distro}'");
    }

    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();

    Ok(ContainerRuntime::Wsl2Docker { distro, version })
}

/// Probe for Colima
#[cfg(target_os = "macos")]
fn probe_colima() -> Result<ContainerRuntime> {
    let output = Command::new("colima")
        .args(["version"])
        .output()
        .map_err(|e| anyhow::anyhow!("colima not found: {e}"))?;

    if !output.status.success() {
        anyhow::bail!("colima version failed");
    }

    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // Check if running
    let status = Command::new("colima")
        .args(["status"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !status {
        anyhow::bail!("colima is not running — run 'colima start'");
    }

    Ok(ContainerRuntime::Colima { version })
}

/// Print doctor report
pub fn doctor() {
    let probe = detect_runtime();

    println!("nemisis8 doctor");
    println!("===============");
    println!();

    if probe.available.is_empty() {
        println!("No container runtime found!");
        println!();
        println!("Install one of:");
        println!("  Docker Desktop: https://docs.docker.com/desktop/");
        println!("  Podman (free):");
        println!("    macOS:   brew install podman && podman machine start");
        println!("    Linux:   sudo apt install podman");
        println!("    Windows: https://podman-desktop.io");
        println!();
        println!("Run 'nemesis8 init' to auto-detect or install a runtime.");
    } else {
        println!("Container runtimes:");
        for rt in &probe.available {
            match rt {
                ContainerRuntime::Docker { version, compose } => {
                    let extras = if *compose { " (with compose)" } else { "" };
                    println!("  [OK] Docker v{version}{extras}");
                }
                ContainerRuntime::Podman { version, machine_running } => {
                    let extras = if *machine_running { " (machine running)" } else { " (native)" };
                    println!("  [OK] Podman v{version}{extras}");
                }
                ContainerRuntime::Wsl2Docker { distro, version } => {
                    println!("  [OK] WSL2 Docker v{version} (distro: {distro})");
                }
                ContainerRuntime::Colima { version } => {
                    println!("  [OK] Colima {version}");
                }
            }
        }
    }

    // macOS: report Homebrew availability
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = Command::new("brew").arg("--version").output() {
            if out.status.success() {
                let v = String::from_utf8_lossy(&out.stdout);
                let line = v.lines().next().unwrap_or("").trim();
                println!("  [OK] {line}");
            }
        }
    }

    if !probe.errors.is_empty() {
        println!();
        println!("Probing notes:");
        for err in &probe.errors {
            println!("  - {err}");
        }
    }

    println!();
    if let Some(rec) = &probe.recommended {
        println!("Recommended runtime: {rec}");
    }

    // Check system resources
    println!();
    println!("System:");
    println!("  Platform: {}", std::env::consts::OS);
    println!("  Arch: {}", std::env::consts::ARCH);
}
