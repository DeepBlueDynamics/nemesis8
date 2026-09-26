//! What can this image run, and how do I launch it? — the machine-readable
//! provider catalog behind `n8 providers --json` and `GET /providers`.
//!
//! A host UI (Hyperia's "new agent" menu) needs two things n8 already knows
//! but never exposed: which providers are baked into the current image, and
//! the exact `n8 …` line that starts each one. Installed-ness comes from the
//! image itself, cheapest source first: the `nemesis8.providers` label stamped
//! at build (one `docker inspect`), then the `/opt/defaults/providers.selected`
//! manifest the installer wrote (one `docker run --rm`). Images built before
//! either existed report `installed: null` and the caller may probe.

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LaunchLines {
    /// `n8 --provider <name> interactive`
    pub interactive: String,
    /// `n8 --danger --provider <name> interactive` — what Hyperia panes run.
    pub interactive_danger: String,
    /// `n8 --provider <name> run "<prompt>"` with a placeholder prompt.
    pub run: String,
    /// The interactive line as an argv array, for spawning without a shell.
    pub argv: Vec<String>,
    /// Same, danger mode.
    pub argv_danger: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProviderEntry {
    pub name: String,
    /// The CLI binary inside the container (`codex`, `claude`, `agy`, …).
    pub binary: String,
    /// The provider's default model, when its TOML declares one.
    pub default_model: Option<String>,
    /// Whether the current image has this provider. `None` = unknown (an
    /// image built before the label/manifest existed).
    pub installed: Option<bool>,
    /// True for the providers `n8 build` installs by default.
    pub default: bool,
    pub launch: LaunchLines,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderCatalog {
    pub image: String,
    /// Where `installed` came from: "label", "manifest", or "unknown".
    pub source: &'static str,
    pub providers: Vec<ProviderEntry>,
}

pub fn launch_lines(name: &str) -> LaunchLines {
    let argv = vec!["n8".to_string(), "--provider".to_string(), name.to_string(), "interactive".to_string()];
    let argv_danger = vec![
        "n8".to_string(),
        "--danger".to_string(),
        "--provider".to_string(),
        name.to_string(),
        "interactive".to_string(),
    ];
    LaunchLines {
        interactive: argv.join(" "),
        interactive_danger: argv_danger.join(" "),
        run: format!("n8 --provider {name} run \"<prompt>\""),
        argv,
        argv_danger,
    }
}

/// Parse the comma-separated provider list from the image label or manifest.
pub fn parse_provider_list(raw: &str) -> Vec<String> {
    raw.split([',', '\n'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// The set of providers baked into `image`, and where we learned it. Tries the
/// `nemesis8.providers` label first (no container start), then the installer's
/// manifest. `None` when neither exists.
pub fn installed_providers(runtime: &str, image: &str) -> Option<(Vec<String>, &'static str)> {
    if let Ok(o) = std::process::Command::new(runtime)
        .args(["inspect", "-f", "{{index .Config.Labels \"nemesis8.providers\"}}", image])
        .output()
    {
        if o.status.success() {
            let list = parse_provider_list(&String::from_utf8_lossy(&o.stdout));
            if !list.is_empty() {
                return Some((list, "label"));
            }
        }
    }
    if let Ok(o) = std::process::Command::new(runtime)
        .args([
            "run",
            "--rm",
            "--entrypoint",
            "sh",
            image,
            "-c",
            "cat /opt/defaults/providers.selected 2>/dev/null",
        ])
        .output()
    {
        let list = parse_provider_list(&String::from_utf8_lossy(&o.stdout));
        if !list.is_empty() {
            return Some((list, "manifest"));
        }
    }
    None
}

/// Build the catalog for `image`. `installed` is `Some(true/false)` when the
/// image says, `None` otherwise. Registry order is kept.
pub fn catalog(runtime: &str, image: &str) -> ProviderCatalog {
    let registry = crate::provider_registry::ProviderRegistry::load();
    let (installed, source) = match installed_providers(runtime, image) {
        Some((list, source)) => (Some(list), source),
        None => (None, "unknown"),
    };
    let providers = registry
        .all()
        .map(|def| {
            let p = &def.provider;
            ProviderEntry {
                name: p.name.clone(),
                binary: p.binary.clone(),
                default_model: p.model.default.clone().filter(|m| !m.is_empty()),
                installed: installed.as_ref().map(|set| set.iter().any(|n| n == &p.name)),
                default: p.install.default_build,
                launch: launch_lines(&p.name),
            }
        })
        .collect();
    ProviderCatalog {
        image: image.to_string(),
        source,
        providers,
    }
}

/// Human table for `n8 providers` without `--json`.
pub fn render_table(cat: &ProviderCatalog) -> String {
    let mut out = format!("image {}  (installed set from: {})\n", cat.image, cat.source);
    out.push_str(&format!("{:<13} {:<10} {:<9} {}\n", "PROVIDER", "INSTALLED", "DEFAULT", "LAUNCH"));
    for p in &cat.providers {
        let inst = match p.installed {
            Some(true) => "yes",
            Some(false) => "no",
            None => "?",
        };
        out.push_str(&format!(
            "{:<13} {:<10} {:<9} {}\n",
            p.name,
            inst,
            if p.default { "yes" } else { "" },
            p.launch.interactive_danger
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_lines_are_exact() {
        let l = launch_lines("grok");
        assert_eq!(l.interactive, "n8 --provider grok interactive");
        assert_eq!(l.interactive_danger, "n8 --danger --provider grok interactive");
        assert_eq!(l.run, "n8 --provider grok run \"<prompt>\"");
        assert_eq!(l.argv_danger, vec!["n8", "--danger", "--provider", "grok", "interactive"]);
    }

    #[test]
    fn provider_lists_parse_from_label_and_manifest_forms() {
        assert_eq!(parse_provider_list("codex,claude, grok"), vec!["codex", "claude", "grok"]);
        assert_eq!(parse_provider_list("codex\nclaude\n\n"), vec!["codex", "claude"]);
        assert!(parse_provider_list("  ").is_empty());
    }

    #[test]
    fn catalog_covers_every_registered_provider_with_unknown_install_state() {
        // A runtime that doesn't exist → neither label nor manifest → installed: None.
        let cat = catalog("definitely-not-a-container-runtime", "nemesis8:latest");
        assert_eq!(cat.source, "unknown");
        assert!(cat.providers.iter().any(|p| p.name == "codex"));
        assert!(cat.providers.iter().all(|p| p.installed.is_none()));
        let grok = cat.providers.iter().find(|p| p.name == "grok").expect("grok in registry");
        assert_eq!(grok.launch.interactive_danger, "n8 --danger --provider grok interactive");
        let table = render_table(&cat);
        assert!(table.contains("n8 --danger --provider codex interactive"));
    }
}
