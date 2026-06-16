#!/usr/bin/env python3
"""Install provider CLIs based on data from providers/*.toml.

Usage:
  install-providers.py <providers>            # csv of provider names
  install-providers.py codex,gemini,antigravity

Each providers/<name>.toml supplies its own install recipe via
[provider.install]:

  kind = "npm"   -> npm install -g <package>
  kind = "curl"  -> curl -fsSL <url> | bash, then locate <binary_name>
                    and symlink to /usr/local/bin/<binary_name>
  kind = "host"  -> runs on the host; nothing to install in the container
  kind = "none"  -> explicit no-op (same as host but documents intent)

The script fails LOUDLY if any requested provider can't be installed —
producing a "successful" image that's secretly missing a binary is the
exact problem this refactor is solving.
"""

from __future__ import annotations

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


def install_curl(name: str, spec: dict, install_cfg: dict) -> None:
    url = install_cfg.get("url")
    if not url:
        raise RuntimeError(f"{name}: install.kind='curl' but no install.url set")
    binary_name = install_cfg.get("binary_name") or spec["provider"].get("binary")
    if not binary_name:
        raise RuntimeError(f"{name}: cannot determine binary_name (set install.binary_name or provider.binary)")

    print(f"[install-providers] curl {url} | bash")
    # set -eo pipefail so curl failures surface as a non-zero exit instead
    # of bash silently succeeding on empty input.
    subprocess.run(
        f"set -eo pipefail; curl -fsSL {url} | bash",
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
    elif kind == "curl":
        install_curl(name, spec, install_cfg)
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

    # Clean npm cache once at the end if npm was used.
    if shutil.which("npm"):
        subprocess.run(["npm", "cache", "clean", "--force"], check=False)

    return 0


if __name__ == "__main__":
    sys.exit(main())
