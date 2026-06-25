// Build-time codegen + Windows metadata.

use std::path::Path;

/// Generate the embedded provider list from providers/*.toml so adding a
/// provider never requires editing provider_registry.rs (issue #34). Emits
/// OUT_DIR/embedded_providers.rs containing `const EMBEDDED: &[&str]`.
fn generate_embedded_providers() {
    println!("cargo:rerun-if-changed=providers");
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let providers_dir = Path::new(&manifest).join("providers");

    let mut paths: Vec<String> = std::fs::read_dir(&providers_dir)
        .expect("reading providers/ dir")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .map(|p| {
            // rerun when any individual TOML changes (the dir-level line only
            // catches file adds/removes).
            println!("cargo:rerun-if-changed={}", p.display());
            p.display().to_string().replace('\\', "/")
        })
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no provider TOMLs found in providers/");

    let body: String = paths
        .iter()
        .map(|p| format!("    include_str!(r\"{p}\"),\n"))
        .collect();
    let out = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("embedded_providers.rs");
    std::fs::write(&out, format!("const EMBEDDED: &[&str] = &[\n{body}];\n"))
        .expect("writing embedded_providers.rs");
}

/// Generate the embedded service list from services/*.toml (mirrors providers).
/// Emits OUT_DIR/embedded_services.rs with `const EMBEDDED_SERVICES: &[&str]`.
fn generate_embedded_services() {
    println!("cargo:rerun-if-changed=services");
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let services_dir = Path::new(&manifest).join("services");

    let mut paths: Vec<String> = std::fs::read_dir(&services_dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "toml"))
                .map(|p| {
                    println!("cargo:rerun-if-changed={}", p.display());
                    p.display().to_string().replace('\\', "/")
                })
                .collect()
        })
        .unwrap_or_default();
    paths.sort();

    let body: String = paths
        .iter()
        .map(|p| format!("    include_str!(r\"{p}\"),\n"))
        .collect();
    let out = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("embedded_services.rs");
    std::fs::write(
        &out,
        format!("const EMBEDDED_SERVICES: &[&str] = &[\n{body}];\n"),
    )
    .expect("writing embedded_services.rs");
}

/// Generate the embedded socket-MCP list from mcp-servers/*.toml (mirrors
/// services). Dir is `mcp-servers`, not `mcp`, to avoid colliding with the
/// case-insensitive `MCP/` python-tools dir on Windows.
/// Emits OUT_DIR/embedded_mcp_servers.rs with `const EMBEDDED_MCP_SERVERS`.
fn generate_embedded_mcp_servers() {
    println!("cargo:rerun-if-changed=mcp-servers");
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let mcp_dir = Path::new(&manifest).join("mcp-servers");

    let mut paths: Vec<String> = std::fs::read_dir(&mcp_dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "toml"))
                .map(|p| {
                    println!("cargo:rerun-if-changed={}", p.display());
                    p.display().to_string().replace('\\', "/")
                })
                .collect()
        })
        .unwrap_or_default();
    paths.sort();

    let body: String = paths
        .iter()
        .map(|p| format!("    include_str!(r\"{p}\"),\n"))
        .collect();
    let out = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("embedded_mcp_servers.rs");
    std::fs::write(
        &out,
        format!("const EMBEDDED_MCP_SERVERS: &[&str] = &[\n{body}];\n"),
    )
    .expect("writing embedded_mcp_servers.rs");
}

/// Generate the embedded app list from apps/*.toml (mirrors services).
/// Emits OUT_DIR/embedded_apps.rs with `const EMBEDDED_APPS: &[&str]`.
fn generate_embedded_apps() {
    println!("cargo:rerun-if-changed=apps");
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let apps_dir = Path::new(&manifest).join("apps");

    let mut paths: Vec<String> = std::fs::read_dir(&apps_dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "toml"))
                .map(|p| {
                    println!("cargo:rerun-if-changed={}", p.display());
                    p.display().to_string().replace('\\', "/")
                })
                .collect()
        })
        .unwrap_or_default();
    paths.sort();

    let body: String = paths
        .iter()
        .map(|p| format!("    include_str!(r\"{p}\"),\n"))
        .collect();
    let out = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("embedded_apps.rs");
    std::fs::write(&out, format!("const EMBEDDED_APPS: &[&str] = &[\n{body}];\n"))
        .expect("writing embedded_apps.rs");
}

fn main() {
    generate_embedded_providers();
    generate_embedded_services();
    generate_embedded_mcp_servers();
    generate_embedded_apps();

    // Embed Windows PE VERSIONINFO so the firewall and Properties dialogs
    // show "DeepBlue Dynamics LLC" instead of "Unknown publisher". FileVersion
    // and ProductVersion default to CARGO_PKG_VERSION automatically.
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set("CompanyName", "DeepBlue Dynamics LLC")
            .set("FileDescription", "nemesis8 — run AI agents in Docker")
            .set("ProductName", "nemesis8")
            .set("OriginalFilename", "nemesis8.exe")
            .set("LegalCopyright", "\u{00A9} DeepBlue Dynamics LLC");
        res.compile().expect("failed to embed Windows VERSIONINFO");
    }
}
