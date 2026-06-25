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

/// A runtime whose CLI is installed but whose daemon/VM isn't responding —
/// i.e. it could work if started. Carries how to start it.
#[derive(Debug, Clone)]
pub struct DownRuntime {
    /// Runtime key: "docker" | "podman" | "colima".
    pub name: String,
    /// Human label, e.g. "Docker Desktop" / "Docker daemon" / "Podman machine".
    pub label: String,
    /// Manual command to show if auto-start is declined or fails.
    pub start_hint: String,
    /// Whether n8 can start it automatically on this platform.
    pub can_autostart: bool,
}

/// Result of probing for container runtimes
#[derive(Debug)]
pub struct RuntimeProbe {
    pub available: Vec<ContainerRuntime>,
    pub recommended: Option<String>,
    pub errors: Vec<String>,
    /// Runtimes installed but not currently running (offer to start these).
    pub installed_down: Vec<DownRuntime>,
}

/// True if `<bin> --version` runs — i.e. the CLI is on PATH (works even when the
/// daemon is down, since --version is a client-only call).
fn bin_present(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn runtime_key(rt: &ContainerRuntime) -> &'static str {
    match rt {
        ContainerRuntime::Docker { .. } => "docker",
        ContainerRuntime::Podman { .. } => "podman",
        ContainerRuntime::Wsl2Docker { .. } => "wsl2",
        ContainerRuntime::Colima { .. } => "colima",
    }
}

fn docker_down() -> DownRuntime {
    #[cfg(target_os = "windows")]
    let (label, hint) = ("Docker Desktop", "start Docker Desktop and wait until it reports 'running'");
    #[cfg(target_os = "macos")]
    let (label, hint) = ("Docker Desktop", "open -a Docker");
    #[cfg(target_os = "linux")]
    let (label, hint) = ("Docker daemon", "sudo systemctl start docker");
    DownRuntime {
        name: "docker".to_string(),
        label: label.to_string(),
        start_hint: hint.to_string(),
        can_autostart: true,
    }
}

fn podman_down() -> DownRuntime {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    let (label, hint) = ("Podman machine", "podman machine start  (first run: podman machine init && podman machine start)");
    #[cfg(target_os = "linux")]
    let (label, hint) = ("Podman socket", "systemctl --user start podman.socket");
    DownRuntime {
        name: "podman".to_string(),
        label: label.to_string(),
        start_hint: hint.to_string(),
        can_autostart: true,
    }
}

#[cfg(target_os = "macos")]
fn colima_down() -> DownRuntime {
    DownRuntime {
        name: "colima".to_string(),
        label: "Colima".to_string(),
        start_hint: "colima start".to_string(),
        can_autostart: true,
    }
}

/// Detect available container runtimes
pub fn detect_runtime() -> RuntimeProbe {
    let mut probe = RuntimeProbe {
        available: Vec::new(),
        recommended: None,
        errors: Vec::new(),
        installed_down: Vec::new(),
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

    // Classify installed-but-down: a CLI on PATH whose daemon/VM didn't answer.
    // These are the runtimes n8 can offer to START (vs. install). Skip any that
    // are already up (their probe succeeded and they're in `available`).
    let up = |k: &str| probe.available.iter().any(|r| runtime_key(r) == k);
    if !up("docker") && bin_present("docker") {
        probe.installed_down.push(docker_down());
    }
    if !up("podman") && bin_present("podman") {
        probe.installed_down.push(podman_down());
    }
    #[cfg(target_os = "macos")]
    if !up("colima") && bin_present("colima") {
        probe.installed_down.push(colima_down());
    }

    probe
}

/// Start an installed-but-down runtime and wait until it's ready (~60s).
/// Returns Ok once its probe succeeds, Err on failure/timeout.
pub fn start_runtime(name: &str) -> Result<()> {
    match name {
        "docker" => start_docker()?,
        "podman" => start_podman()?,
        #[cfg(target_os = "macos")]
        "colima" => {
            run_start("colima", &["start"])?;
        }
        _ => anyhow::bail!("don't know how to start '{name}' automatically"),
    }
    wait_ready(name)
}

/// Spawn a start command, surfacing a clear error if it can't be launched.
fn run_start(bin: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(bin)
        .args(args)
        .status()
        .map_err(|e| anyhow::anyhow!("could not run `{bin} {}`: {e}", args.join(" ")))?;
    if !status.success() {
        anyhow::bail!("`{bin} {}` exited with {}", args.join(" "), status);
    }
    Ok(())
}

fn start_docker() -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        // Docker Desktop is a GUI app — launch the exe and let the daemon come
        // up; wait_ready polls for it. Try the standard install paths.
        let candidates = [
            std::env::var("ProgramFiles")
                .map(|p| format!("{p}\\Docker\\Docker\\Docker Desktop.exe"))
                .unwrap_or_default(),
            std::env::var("ProgramW6432")
                .map(|p| format!("{p}\\Docker\\Docker\\Docker Desktop.exe"))
                .unwrap_or_default(),
        ];
        for path in candidates.iter().filter(|p| !p.is_empty()) {
            if std::path::Path::new(path).exists() {
                Command::new(path)
                    .spawn()
                    .map_err(|e| anyhow::anyhow!("could not launch Docker Desktop: {e}"))?;
                return Ok(());
            }
        }
        anyhow::bail!("Docker Desktop.exe not found in Program Files — start it from the Start menu");
    }
    #[cfg(target_os = "macos")]
    {
        run_start("open", &["-a", "Docker"])
    }
    #[cfg(target_os = "linux")]
    {
        run_start("sudo", &["systemctl", "start", "docker"])
    }
}

fn start_podman() -> Result<()> {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        run_start("podman", &["machine", "start"])
    }
    #[cfg(target_os = "linux")]
    {
        run_start("systemctl", &["--user", "start", "podman.socket"])
    }
}

/// Poll the runtime's probe until it reports ready, up to ~60s.
fn wait_ready(name: &str) -> Result<()> {
    use std::io::Write;
    print!("Waiting for {name} to come up");
    let _ = std::io::stdout().flush();
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_secs(3));
        let ready = match name {
            "docker" => probe_docker().is_ok(),
            "podman" => probe_podman().is_ok(),
            #[cfg(target_os = "macos")]
            "colima" => probe_colima().is_ok(),
            _ => false,
        };
        if ready {
            println!(" ready.");
            return Ok(());
        }
        print!(".");
        let _ = std::io::stdout().flush();
    }
    println!();
    anyhow::bail!("{name} did not become ready within ~60s")
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

/// Decode the output of `wsl.exe`, which is UTF-16LE. Reading it as UTF-8 leaves
/// a NUL byte between every ASCII char; those interior NULs then crash the next
/// `Command`/CString call with "nul byte found in provided data". So decode as
/// UTF-16LE — but fall back to UTF-8 for the rare distro/setup that emits it.
#[cfg(target_os = "windows")]
fn decode_wsl_output(bytes: &[u8]) -> String {
    // If it's already clean (NUL-free) UTF-8, keep it.
    if let Ok(s) = std::str::from_utf8(bytes) {
        if !s.contains('\0') {
            return s.to_string();
        }
    }
    // Otherwise decode as UTF-16LE, stripping a BOM if present.
    let b = bytes.strip_prefix(&[0xFF, 0xFE]).unwrap_or(bytes);
    let units: Vec<u16> = b
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

/// Probe for Docker inside WSL2
#[cfg(target_os = "windows")]
fn probe_wsl2_docker() -> Result<ContainerRuntime> {
    // List WSL distros (wsl.exe emits UTF-16LE — decode properly or the distro
    // name carries interior NULs that crash the `wsl -d <distro>` call below).
    let output = Command::new("wsl")
        .args(["--list", "--quiet"])
        .output()
        .map_err(|e| anyhow::anyhow!("wsl not found: {e}"))?;

    if !output.status.success() {
        anyhow::bail!("wsl --list failed");
    }

    let distros = decode_wsl_output(&output.stdout);
    let distro = distros
        .lines()
        .map(|l| l.trim())
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

    let version = decode_wsl_output(&output.stdout).trim().to_string();

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

    println!("nemesis8 doctor");
    println!("===============");
    println!();

    if probe.available.is_empty() {
        if probe.installed_down.is_empty() {
            println!("No container runtime found!");
            println!();
            println!("Install one of:");
            println!("  Docker Desktop: https://docs.docker.com/desktop/");
            println!("  Podman (free):");
            println!("    macOS:   brew install podman && podman machine start");
            println!("    Linux:   sudo apt install podman");
            println!("    Windows: https://podman-desktop.io");
        } else {
            println!("A container runtime is installed but not running:");
            for d in &probe.installed_down {
                println!("  [DOWN] {} — start: {}", d.label, d.start_hint);
            }
            println!();
            println!("'nemesis8 init' (or any build/run) will offer to start it for you.");
        }
        println!();
        println!("Run 'nemesis8 init' to auto-detect, start, or install a runtime.");
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
