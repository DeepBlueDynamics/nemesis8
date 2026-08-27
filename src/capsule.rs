//! Capsule emitter: turns a `CapsuleSpec` into a Repo One–submittable bundle —
//! pinned source, vendored deps, a hardened Dockerfile rebased on an Iron Bank
//! base, a `hardening_manifest.yaml`, and (optionally) an exported image tarball
//! validated with a fully offline `docker build --network none`.
//!
//! nemesis8 is the low-side *factory*: it emits the hardened artifact but never
//! runs it and is never itself hardened. See docs/plans/iterative-baking-nebula.

use crate::capsule_def::CapsuleSpec;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

const DOCKERFILE_TMPL: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/capsules/templates/Dockerfile.tmpl"
));
const MANIFEST_TMPL: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/capsules/templates/hardening_manifest.yaml.tmpl"
));

/// Knobs for a single emit run.
pub struct EmitOptions {
    /// Bundle output dir (also the build context + Repo One submission dir).
    pub out: PathBuf,
    /// Local source checkout to build from instead of cloning (dev/offline).
    pub source: Option<PathBuf>,
    /// Override the capsule's runtime base image (stand-in vs real Iron Bank).
    pub base_image: Option<String>,
    /// Override the capsule's builder base image.
    pub builder_image: Option<String>,
    /// Run the offline `docker build` + `docker save` after emitting the bundle.
    pub build: bool,
    /// Container engine binary (docker / podman).
    pub runtime: String,
}

/// Emit the bundle for `spec` into `opts.out`. Returns the bundle dir.
pub fn emit(spec: &CapsuleSpec, opts: &EmitOptions) -> Result<PathBuf> {
    spec.validate().map_err(|e| anyhow::anyhow!(e))?;
    let out = &opts.out;
    std::fs::create_dir_all(out)
        .with_context(|| format!("creating output dir {}", out.display()))?;

    // 1. Resolve + copy the source into the bundle (which IS the build context).
    let source = resolve_source(spec, opts)?;
    println!("[capsule] source: {}", source.display());
    copy_tree(&source, out, &[".git", "target", "workspace"])
        .with_context(|| "copying source tree into the bundle")?;

    // 2. Vendor crates so the image build needs no network.
    println!("[capsule] vendoring crates (cargo vendor)…");
    vendor_deps(out)?;

    // 3. Render the hardened Dockerfile + hardening manifest.
    let (base_repo, base_tag) =
        split_image(opts.base_image.as_deref().unwrap_or(&spec.base_image));
    let builder = opts
        .builder_image
        .clone()
        .unwrap_or_else(|| spec.builder_image.clone());
    write_file(&out.join("Dockerfile"), &render_dockerfile(spec, &base_repo, &base_tag, &builder))?;
    write_file(
        &out.join("hardening_manifest.yaml"),
        &render_manifest(spec, &base_repo, &base_tag)?,
    )?;
    println!("[capsule] wrote Dockerfile + hardening_manifest.yaml");

    // 4. Optional: prove the offline build, then export the image.
    if opts.build {
        let tag = format!("{}-capsule:latest", spec.name);
        println!("[capsule] building offline ({} build --network none)…", opts.runtime);
        docker_build_offline(&opts.runtime, out, &tag)?;
        let image_tar = out.join("image.tar");
        println!("[capsule] exporting {} → {}", tag, image_tar.display());
        docker_save(&opts.runtime, &tag, &image_tar)?;
    }
    Ok(out.clone())
}

fn resolve_source(spec: &CapsuleSpec, opts: &EmitOptions) -> Result<PathBuf> {
    if let Some(p) = &opts.source {
        if !p.is_dir() {
            bail!("--source path does not exist: {}", p.display());
        }
        return Ok(p.clone());
    }
    if let Some(p) = &spec.source.path {
        let pb = PathBuf::from(p);
        if !pb.is_dir() {
            bail!("capsule source.path does not exist: {}", pb.display());
        }
        return Ok(pb);
    }
    let git = spec
        .source
        .git
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("capsule has no source (git/path)"))?;
    let rev = spec.source.rev.as_deref().unwrap_or("HEAD");
    let tmp = std::env::temp_dir().join(format!("n8-capsule-{}-src", spec.name));
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp).ok();
    }
    let tmp_str = tmp.display().to_string();
    println!("[capsule] cloning {git} @ {rev}");
    run("git", &["clone", git, &tmp_str], None)?;
    run("git", &["-C", &tmp_str, "checkout", rev], None)?;
    Ok(tmp)
}

fn vendor_deps(dir: &Path) -> Result<()> {
    let output = std::process::Command::new("cargo")
        .args(["vendor", "--locked", "vendor"])
        .current_dir(dir)
        .output()
        .context("running `cargo vendor` (is cargo installed on the host?)")?;
    if !output.status.success() {
        bail!(
            "cargo vendor failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    // cargo vendor prints the `[source.crates-io] replace-with` config to stdout.
    let config = String::from_utf8_lossy(&output.stdout);
    let cargo_dir = dir.join(".cargo");
    std::fs::create_dir_all(&cargo_dir)?;
    write_file(&cargo_dir.join("config.toml"), &config)?;
    Ok(())
}

fn render_dockerfile(spec: &CapsuleSpec, base_repo: &str, base_tag: &str, builder: &str) -> String {
    let features_arg = if spec.build_features.is_empty() {
        String::new()
    } else {
        format!(" --features {}", spec.build_features.join(","))
    };
    let labels_block: String = spec
        .labels
        .iter()
        .map(|(k, v)| format!("LABEL \"{}\"=\"{}\"\n", k, v.replace('"', "\\\"")))
        .collect();
    let env_block: String = spec.env.iter().map(|e| format!("ENV {e}\n")).collect();
    render(
        DOCKERFILE_TMPL,
        &[
            ("name", spec.name.clone()),
            ("builder_image", builder.to_string()),
            ("base_image_repo", base_repo.to_string()),
            ("base_image_tag", base_tag.to_string()),
            ("features_arg", features_arg),
            ("binary", spec.binary.clone()),
            ("run_user", spec.run_user.clone()),
            ("labels_block", labels_block),
            ("env_block", env_block),
        ],
    )
}

fn render_manifest(spec: &CapsuleSpec, base_repo: &str, base_tag: &str) -> Result<String> {
    // Flow-style (JSON is valid YAML) keeps the collections robust — no
    // indentation to get wrong, empty renders as `[]` / `{}`.
    let tags = serde_json::to_string(&spec.tags)?;
    let labels = serde_json::to_string(&spec.labels)?;
    let maintainers = serde_json::to_string(
        &spec
            .maintainers
            .iter()
            .map(|m| {
                serde_json::json!({"name": m.name, "email": m.email, "username": m.username})
            })
            .collect::<Vec<_>>(),
    )?;
    Ok(render(
        MANIFEST_TMPL,
        &[
            ("name", spec.name.clone()),
            ("tags", tags),
            ("base_image_repo", base_repo.to_string()),
            ("base_image_tag", base_tag.to_string()),
            ("labels", labels),
            ("maintainers", maintainers),
        ],
    ))
}

fn render(template: &str, vars: &[(&str, String)]) -> String {
    let mut out = template.to_string();
    for (key, value) in vars {
        out = out.replace(&format!("{{{{{key}}}}}"), value);
    }
    out
}

fn docker_build_offline(runtime: &str, context: &Path, tag: &str) -> Result<()> {
    let status = std::process::Command::new(runtime)
        .args(["build", "--network", "none", "-t", tag, "."])
        .current_dir(context)
        .status()
        .with_context(|| format!("running `{runtime} build`"))?;
    if !status.success() {
        bail!(
            "offline `{runtime} build` failed — are the base + builder images pulled locally? \
             (a disconnected build can't fetch them)"
        );
    }
    Ok(())
}

fn docker_save(runtime: &str, tag: &str, out: &Path) -> Result<()> {
    let status = std::process::Command::new(runtime)
        .args(["save", "-o", &out.display().to_string(), tag])
        .status()
        .with_context(|| format!("running `{runtime} save`"))?;
    if !status.success() {
        bail!("`{runtime} save` failed for {tag}");
    }
    Ok(())
}

/// Split an image ref into (repo, tag). `debian:bookworm-slim` → (debian,
/// bookworm-slim); a bare `host:5000/img` (port, no tag) → (…, latest).
fn split_image(image: &str) -> (String, String) {
    if let Some(idx) = image.rfind(':') {
        let (repo, tag) = image.split_at(idx);
        let tag = &tag[1..];
        if !tag.contains('/') {
            return (repo.to_string(), tag.to_string());
        }
    }
    (image.to_string(), "latest".to_string())
}

fn copy_tree(src: &Path, dst: &Path, exclude: &[&str]) -> Result<()> {
    for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if exclude.iter().any(|e| *e == name_str.as_ref()) {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if from.is_dir() {
            std::fs::create_dir_all(&to)?;
            copy_tree(&from, &to, exclude)?;
        } else {
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&from, &to)
                .with_context(|| format!("copying {}", from.display()))?;
        }
    }
    Ok(())
}

fn write_file(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

fn run(cmd: &str, args: &[&str], cwd: Option<&Path>) -> Result<()> {
    let mut command = std::process::Command::new(cmd);
    command.args(args);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    let status = command
        .status()
        .with_context(|| format!("running `{cmd}` (is it installed?)"))?;
    if !status.success() {
        bail!("`{cmd} {}` failed", args.join(" "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_image_handles_tag_and_port() {
        assert_eq!(
            split_image("debian:bookworm-slim"),
            ("debian".into(), "bookworm-slim".into())
        );
        assert_eq!(
            split_image("registry1.dso.mil/ironbank/redhat/ubi/ubi9-minimal:9.4"),
            (
                "registry1.dso.mil/ironbank/redhat/ubi/ubi9-minimal".into(),
                "9.4".into()
            )
        );
        // port with no tag → latest
        assert_eq!(
            split_image("localhost:5000/img"),
            ("localhost:5000/img".into(), "latest".into())
        );
    }

    #[test]
    fn render_substitutes_all_placeholders() {
        let out = render("a={{x}} b={{y}}", &[("x", "1".into()), ("y", "2".into())]);
        assert_eq!(out, "a=1 b=2");
    }
}
