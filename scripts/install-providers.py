#!/usr/bin/env python3
"""Install provider CLIs based on data from providers/*.toml.

Usage:
  install-providers.py <providers>            # csv of provider names
  install-providers.py codex,gemini,antigravity

Each providers/<name>.toml supplies its own install recipe via
[provider.install]:

  kind = "npm"   -> npm install -g <package>
  kind = "bun"   -> bun install -g <package>. Bun (the runtime omp needs) is
                    installed on demand FIRST, so it lands in the image ONLY
                    when a bun-kind provider is actually being installed.
  kind = "curl"  -> curl -fsSL <url> | bash, then locate <binary_name>
                    and symlink to /usr/local/bin/<binary_name>
  kind = "host"  -> runs on the host; nothing to install in the container
  kind = "none"  -> explicit no-op (same as host but documents intent)

The script fails LOUDLY if any requested provider can't be installed —
producing a "successful" image that's secretly missing a binary is the
exact problem this refactor is solving.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tomllib
from pathlib import Path

PROVIDERS_DIR = Path("/opt/defaults/providers")
TARGET_BIN_DIR = Path("/usr/local/bin")


def load_spec(name: str) -> dict | None:
    path = PROVIDERS_DIR / f"{name}.toml"
    if not path.is_file():
        return None
    with open(path, "rb") as f:
        return tomllib.load(f)


def install_npm(name: str, install_cfg: dict) -> None:
    package = install_cfg.get("package")
    if not package:
        raise RuntimeError(f"{name}: install.kind='npm' but no install.package set")
    pkg_at_latest = f"{package}@latest"
    args = ["npm", "install", "-g"]
    if install_cfg.get("ignore_scripts"):
        args.append("--ignore-scripts")
    if install_cfg.get("npm_flags"):
        flags = install_cfg.get("npm_flags")
        if isinstance(flags, list):
            args.extend(flags)
        else:
            args.extend(flags.split())
    args.append(pkg_at_latest)
    print(f"[install-providers] {' '.join(args)}")
    subprocess.run(args, check=True)


# Bun's global-install prefix. `BUN_INSTALL=/usr/local` makes `bun install -g`
# drop binaries in /usr/local/bin (already on PATH at build AND runtime), so an
# omp bin with a `#!/usr/bin/env bun` entrypoint resolves both when we smoke-test
# it here and when the agent launches later.
BUN_INSTALL_PREFIX = "/usr/local"


def ensure_bun() -> None:
    """Install Bun on demand — only called by install_bun, so Bun lands in the
    image ONLY when a bun-kind provider (omp) is actually being installed. Uses
    the image's existing npm (bun is published on npm and pulls its own native
    binary) to avoid dragging in curl|unzip just for the Bun installer."""
    if shutil.which("bun"):
        print("[install-providers] bun already present")
        return
    print("[install-providers] installing bun (required by a bun-kind provider)")
    subprocess.run(["npm", "install", "-g", "bun"], check=True)
    subprocess.run(["bun", "--version"], check=True)


def install_bun(name: str, spec: dict, install_cfg: dict) -> None:
    package = install_cfg.get("package")
    if not package:
        raise RuntimeError(f"{name}: install.kind='bun' but no install.package set")
    ensure_bun()
    env = os.environ.copy()
    env["BUN_INSTALL"] = BUN_INSTALL_PREFIX
    args = ["bun", "install", "-g", package]
    print(f"[install-providers] BUN_INSTALL={BUN_INSTALL_PREFIX} {' '.join(args)}")
    subprocess.run(args, check=True, env=env)
    # Smoke test — surface a broken install (or a missing bun runtime) at build
    # time, not at session-launch time.
    binary = install_cfg.get("binary_name") or spec["provider"].get("binary")
    if binary:
        subprocess.run([binary, "--version"], check=True, env=env)


def install_curl(name: str, spec: dict, install_cfg: dict) -> None:
    url = install_cfg.get("url")
    if not url:
        raise RuntimeError(f"{name}: install.kind='curl' but no install.url set")
    binary_name = install_cfg.get("binary_name") or spec["provider"].get("binary")
    if not binary_name:
        raise RuntimeError(f"{name}: cannot determine binary_name (set install.binary_name or provider.binary)")

    # Optional args passed through to the installer script (bash -s -- <args>).
    # e.g. hermes's install.sh takes --skip-browser to skip its default ~290 MB
    # Playwright/Chromium download (browser tools off by default; keep builds lean).
    import shlex
    installer_args = install_cfg.get("args") or []
    args_str = " ".join(shlex.quote(str(a)) for a in installer_args)
    pipe_tail = f" | bash -s -- {args_str}" if args_str else " | bash"
    print(f"[install-providers] curl {url}{pipe_tail}")
    # set -eo pipefail so curl failures surface as a non-zero exit instead
    # of bash silently succeeding on empty input.
    subprocess.run(
        f"set -eo pipefail; curl -fsSL {url}{pipe_tail}",
        shell=True, executable="/bin/bash", check=True,
    )

    # Locate the binary anywhere on the filesystem (single device, skips
    # proc/sys/etc). Include symlinks: installers often ship a versioned file
    # and only symlink the bare name (grok -> ../downloads/grok-linux-x86_64),
    # so a -type f search comes back empty even after a successful install.
    find = subprocess.run(
        ["find", "/", "-xdev", "-name", binary_name,
         "(", "-type", "f", "-o", "-type", "l", ")"],
        capture_output=True, text=True,
    )
    paths = [p for p in find.stdout.splitlines() if p.strip()]
    if not paths:
        raise RuntimeError(
            f"{name}: installer ran but '{binary_name}' not found on filesystem. "
            f"HOME=/root, find stderr: {find.stderr!r}"
        )

    dst = TARGET_BIN_DIR / binary_name
    if str(dst) in paths:
        # The installer already put it on PATH — never re-link dst onto itself.
        print(f"[install-providers] {binary_name} already on PATH at {dst}")
    else:
        src = sorted(paths, key=len)[0]
        # Make sure executable (chmod follows symlinks), then symlink onto PATH.
        Path(src).chmod(0o755)
        if dst.is_symlink() or dst.exists():
            dst.unlink()
        dst.symlink_to(src)
        print(f"[install-providers] linked {src} -> {dst}")

    # Smoke test — surface a broken install at build time, not at session-launch time.
    subprocess.run([str(dst), "--version"], check=True)


def install_tarball(name: str, spec: dict, install_cfg: dict) -> None:
    """Install a provider from a version-PINNED tarball.

    Bypasses 'latest-only' upstream installers (e.g. antigravity's install.sh) so
    we can lock a known-good version. Resolves the per-version manifest
    (<manifest_base>/<version>/manifest.json -> platforms[<plat>].{url,sha512}),
    downloads + sha512-verifies the tarball, extracts the binary, and links it onto
    PATH as binary_name (plus the archive's own name, in case the CLI keys off argv0).
    """
    import json
    import hashlib
    import tarfile
    import tempfile
    import urllib.request
    import platform as _platform

    base = install_cfg.get("manifest_base")
    version = install_cfg.get("version")
    if not base or not version:
        raise RuntimeError(f"{name}: install.kind='tarball' needs install.manifest_base and install.version")
    binary_name = install_cfg.get("binary_name") or spec["provider"].get("binary")
    if not binary_name:
        raise RuntimeError(f"{name}: cannot determine binary_name (set install.binary_name or provider.binary)")

    # Map the container arch to the manifest's platform key.
    arch_key = install_cfg.get("platform_key")
    if not arch_key:
        machine = _platform.machine().lower()
        arch_key = "linux-arm" if machine in ("aarch64", "arm64") else "linux-x64"

    manifest_url = f"{base}/{version}/manifest.json"
    print(f"[install-providers] {name}: pinning to {version} ({arch_key}) via {manifest_url}")
    with urllib.request.urlopen(manifest_url) as r:  # noqa: S310 (trusted URL from provider TOML)
        manifest = json.load(r)
    plat = manifest.get("platforms", {}).get(arch_key, {})
    url, want_sha = plat.get("url"), plat.get("sha512", "")
    if not url:
        raise RuntimeError(f"{name}: manifest for {version} has no platform '{arch_key}'")

    with tempfile.TemporaryDirectory() as td:
        tgz = Path(td) / "payload.tar.gz"
        print(f"[install-providers] {name}: downloading {url}")
        urllib.request.urlretrieve(url, tgz)  # noqa: S310
        if want_sha:
            got = hashlib.sha512(tgz.read_bytes()).hexdigest()
            if got != want_sha:
                raise RuntimeError(f"{name}: sha512 mismatch for {version} (want {want_sha[:16]}…, got {got[:16]}…)")
        with tarfile.open(tgz) as tf:
            tf.extractall(td)
        candidates = [p for p in Path(td).rglob("*") if p.is_file() and p.suffix != ".gz"]
        member = install_cfg.get("archive_member")
        src = None
        if member:
            src = next((p for p in candidates if p.name == member), None)
        if src is None:
            src = next((p for p in candidates if p.name in (binary_name, "antigravity")), None)
        if src is None and len(candidates) == 1:
            src = candidates[0]
        if src is None:
            raise RuntimeError(f"{name}: no binary found in tarball: {[p.name for p in candidates]}")

        dst = TARGET_BIN_DIR / binary_name
        shutil.copy2(src, dst)
        dst.chmod(0o755)
        print(f"[install-providers] {name}: installed {version} -> {dst}")
        # Alias under the archive's own name too (some CLIs branch on argv0).
        if member and member != binary_name:
            alias = TARGET_BIN_DIR / member
            if alias != dst:
                alias.unlink(missing_ok=True)
                alias.symlink_to(dst)


def install_archive(name: str, spec: dict, install_cfg: dict) -> None:
    """Install from a version-PINNED release archive with an arch-templated URL.

    For CLIs that ship raw release tarballs with no installer script and no
    manifest (hax: hax-<ver>-linux-<arch>.tar.gz on GitHub releases).
    Template vars: {version}, {arch} (machine arch normalized to
    x86_64/aarch64, overridable via install.arch_map). Optional
    install.sha512_<arch> pins the archive hash.
    """
    import hashlib
    import shutil
    import tarfile
    import tempfile
    import urllib.request
    import platform as _platform

    template = install_cfg.get("url_template")
    version = install_cfg.get("version")
    if not template or not version:
        raise RuntimeError(f"{name}: install.kind='archive' needs install.url_template and install.version")
    binary_name = install_cfg.get("binary_name") or spec["provider"].get("binary")
    if not binary_name:
        raise RuntimeError(f"{name}: cannot determine binary_name (set install.binary_name or provider.binary)")

    machine = _platform.machine().lower()
    arch = {"amd64": "x86_64", "arm64": "aarch64"}.get(machine, machine)
    arch = (install_cfg.get("arch_map") or {}).get(arch, arch)
    url = template.format(version=version, arch=arch)
    print(f"[install-providers] fetching {url}")

    with tempfile.TemporaryDirectory() as td:
        archive_path = Path(td) / "pkg.archive"
        urllib.request.urlretrieve(url, archive_path)
        expected = install_cfg.get(f"sha512_{arch}")
        if expected:
            actual = hashlib.sha512(archive_path.read_bytes()).hexdigest()
            if actual != expected:
                raise RuntimeError(f"{name}: sha512 mismatch for {url}\n  expected {expected}\n  got      {actual}")
        with tarfile.open(archive_path) as tf:
            tf.extractall(td)
        hits = sorted(
            (p for p in Path(td).rglob(binary_name) if p.is_file()),
            key=lambda p: len(str(p)),
        )
        if not hits:
            raise RuntimeError(f"{name}: '{binary_name}' not found inside archive {url}")
        dst = TARGET_BIN_DIR / binary_name
        if dst.is_symlink() or dst.exists():
            dst.unlink()
        shutil.copy2(hits[0], dst)
        dst.chmod(0o755)
        print(f"[install-providers] installed {hits[0].name} -> {dst}")

    # Smoke test — surface a broken install at build time, not session-launch time.
    subprocess.run([str(dst), "--version"], check=True)


def install_one(name: str) -> None:
    spec = load_spec(name)
    if spec is None:
        print(f"[install-providers] unknown provider '{name}', skipping", file=sys.stderr)
        return

    install_cfg = spec.get("provider", {}).get("install", {})
    kind = install_cfg.get("kind")

    # Back-compat: if no [provider.install] block is set but install_package
    # is set, treat it as an npm install. New providers should define the
    # block explicitly.
    if not kind:
        legacy = spec.get("provider", {}).get("install_package")
        if legacy:
            kind = "npm"
            install_cfg = {"kind": "npm", "package": legacy}
            print(f"[install-providers] {name}: no [provider.install] block; falling back to install_package='{legacy}'")
        else:
            kind = "host"

    if kind == "npm":
        install_npm(name, install_cfg)
    elif kind == "bun":
        install_bun(name, spec, install_cfg)
    elif kind == "curl":
        install_curl(name, spec, install_cfg)
    elif kind == "tarball":
        install_tarball(name, spec, install_cfg)
    elif kind == "archive":
        install_archive(name, spec, install_cfg)
    elif kind in ("host", "none"):
        print(f"[install-providers] {name}: kind={kind}, nothing to install in container")
    else:
        raise RuntimeError(f"{name}: unknown install.kind '{kind}'")


def main() -> int:
    if len(sys.argv) < 2 or not sys.argv[1].strip():
        print("usage: install-providers.py <csv-of-provider-names>", file=sys.stderr)
        return 1

    providers = [p.strip() for p in sys.argv[1].split(",") if p.strip()]
    print(f"[install-providers] requested: {providers}")

    for name in providers:
        install_one(name)

    # Record the exact selected set so the host UI offers PRECISELY these
    # providers. A binary probe can't do this: host-kind providers reuse another
    # CLI's binary (sakana -> codex), so `command -v` can't tell an unchecked
    # sakana from an installed codex. gather_installed_providers reads this file;
    # a missing manifest (image built before this existed) falls back to probing.
    manifest = PROVIDERS_DIR.parent / "providers.selected"
    try:
        manifest.write_text("\n".join(providers) + "\n", encoding="utf-8")
        print(f"[install-providers] wrote {manifest} ({len(providers)} providers)")
    except OSError as e:
        print(f"[install-providers] WARN: could not write {manifest}: {e}", file=sys.stderr)

    # Clean npm cache once at the end if npm was used.
    if shutil.which("npm"):
        subprocess.run(["npm", "cache", "clean", "--force"], check=False)

    return 0


if __name__ == "__main__":
    sys.exit(main())
