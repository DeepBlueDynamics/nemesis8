# Releasing nemesis8 — the runbook

nemesis8 ships through **four separate channels**, and *what you changed*
decides which one(s) you touch. Most changes need only one. Use the table,
then jump to that section.

> Touched MCP tools, registry defs, provider TOMLs, or the config pipeline?
> Run **`n8 mcp test`** before shipping — see [PROVIDER-TESTING.md](PROVIDER-TESTING.md).

## 0. Which channel do I need?

| What you changed | Channel | What to run | Tag? |
|---|---|---|---|
| **Host CLI** — `src/main.rs`, `cli.rs`, `docker.rs`, `picker.rs`, `launcher.rs`, `names.rs`, `search.rs`, `gateway.rs`, `daemon.rs`, … (the `nemesis8` binary you type as `n8`) | **A. GitHub Release (binaries)** | bump version → push `main` → push tag | **Yes** |
| **Container internals** — `MCP/*.py`, `providers/*.toml`, `Dockerfile` (thin), `src/entry.rs`, `src/worker.rs`, `src/monitor_main.rs` (things baked into the *agent container*) | **C. Container image** | push `main` → `n8 build` | No |
| **Base-image deps** — `requirements.txt`, `Dockerfile.base` (Python/MCP runtime, system packages) | **B. Docker Hub base** | push `main` → push tag (auto-builds base) → `n8 build` | **Yes** |
| **Installer / landing page** — `nuts.services/nemesis8-site/` (`install.ps1`, `install.sh`, `index.html`) | **D. Site (Cloud Run)** | `bash deploy.sh` | No |

> Rule of thumb: **a tag (`vX.Y.Z`) is only for things that live in a tagged
> artifact** — the host binary (A) and the base image (B). `MCP/`, `providers/`,
> the thin `Dockerfile`, and the in-container Rust binaries (`entry`/`worker`/
> `monitor`) are **not** in any tagged artifact — they reach users when someone
> runs `n8 build`, which pulls the latest `main`. Tagging them does nothing.

A single tag push triggers **both** A and B at once (see below) — that's normal
and fine.

## Version numbers — NEVER hand-edit; use the script

**Do not hand-edit `version = ` in `Cargo.toml`.** Bumping the version by hand
(typing the next number into a `sed`) is exactly how the minor number kept
getting bumped by reflex — the number gets *decided in the moment*, and the
decision is biased toward "this feels like a feature." The version is computed
for you instead:

```bash
scripts/bump.sh            # PATCH (default) — use this for everything iterative
scripts/bump.sh minor      # MINOR — ONLY when the user explicitly calls it a milestone
```

**Default is always PATCH** (`0.13.0 → 0.13.1`). Fixes, tweaks, columns, a
modal, a pulldown, polish — all PATCH. A change is NOT a minor just because it
adds a "feature." **Bump MINOR only when the user says so** (or it's an
unmistakable new subsystem — the first control plane, the first control room).
When unsure: patch. (MAJOR stays 0 pre-1.0.)

---

## A. GitHub Release — the host binary (`n8`)

For changes to the host CLI. Produces signed binaries for Linux (x64/arm64),
macOS (Intel/Apple Silicon), and Windows, attached to a GitHub Release.

**The one rule: bump `Cargo.toml` version BEFORE pushing the tag — they must match.**

```bash
# 1. Bump the version — NEVER hand-edit Cargo.toml. Default is patch.
NEW=$(scripts/bump.sh)                 # -> "bumped: 0.13.0 -> 0.13.1 (patch)"
#   scripts/bump.sh minor              # ONLY if the user called it a milestone
#   The script edits Cargo.toml + refreshes Cargo.lock and prints the new x.y.z.

# 2. Commit + push main  (use the x.y.z the script printed)
git add -A
git commit -m "fix: <what changed>"
git push origin main

# 3. Tag + push  → triggers the Release workflow (.github/workflows/release.yml)
#    The tag MUST equal the bumped version.
git tag vX.Y.Z
git push origin vX.Y.Z
```

- Workflow: `.github/workflows/release.yml`, triggers on tags matching `v*`.
- Windows binaries are code-signed via **Azure Trusted Signing** (account
  `nuts-services`, profile `hyperia-signing`) — needs the Azure secrets in repo
  settings.
- Output: a GitHub Release `vX.Y.Z` with `nemesis8-vX.Y.Z-<target>.tar.gz` / `.zip`.
- `n8 -V` MUST equal the tag. If they differ, the release is broken (you tagged
  before bumping).

**How users get it:** `n8 update`, or re-run the installer (Channel D URL).

### Nightly builds (don't tag every change)

Push fixes to `main` freely; you do **not** need to tag a release for every
change. `.github/workflows/nightly.yml` builds `main` once a day (and on demand)
into a single rolling **`nightly` prerelease**:

```bash
gh workflow run nightly.yml      # build a nightly RIGHT NOW from main
gh run watch $(gh run list --workflow=nightly.yml -L1 --json databaseId -q '.[0].databaseId')
```

- **Unsigned** (no Azure signing) and marked **prerelease**, so it never becomes
  "latest" — the installer and `n8 update` keep pulling real tagged releases.
- Stable asset URLs, e.g. (Apple Silicon):
  ```bash
  curl -fsSL https://github.com/DeepBlueDynamics/nemesis8/releases/download/nightly/nemesis8-nightly-aarch64-apple-darwin.tar.gz | tar xz
  ```
  Other targets: `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`,
  `aarch64-unknown-linux-gnu`, `x86_64-pc-windows-msvc` (`.zip`).
- Cut a real tagged release (Channel A, via `scripts/bump.sh`) only when you want
  a stable, signed version.

---

## B. Docker Hub — the base image (`nemesis8-base`)

For changes to `requirements.txt` or `Dockerfile.base` (the heavy Python/MCP
runtime layer the thin image builds on).

**It piggybacks on the same tag as Channel A** — `.github/workflows/docker-base.yml`
triggers on any `v*` tag push (tag pushes ignore the `paths:` filter, so it runs
on *every* tag, even when only `requirements.txt` changed). So:

```bash
# Same tag push as Channel A also builds + pushes the base image. Nothing extra.
git push origin vX.Y.Z
```

Or build it on demand without a release (e.g. you only changed deps):

```bash
gh workflow run docker-base.yml          # manual trigger (workflow_dispatch)
gh run watch $(gh run list --workflow=docker-base.yml -L1 --json databaseId -q '.[0].databaseId')
```

- Pushes `deepbluedynamics/nemesis8-base:X.Y.Z` **and** `:latest`, multi-arch
  (`linux/amd64,linux/arm64`).
- Needs repo secrets `DOCKER_USERNAME` + `DOCKER_TOKEN`.
- Takes ~10–12 min. **Watch it** — the common failure is `uv` failing to resolve
  `requirements.txt`.

**How users get it:** the thin image is `FROM nemesis8-base:${NEMESIS8_BASE_TAG}`
(default `latest`), so the next `n8 build` pulls the new base. (Pin a specific
base with `NEMESIS8_BASE_TAG=X.Y.Z n8 build` if needed.)

---

## C. Container image — MCP tools, providers, entry binary

For `MCP/*.py`, `providers/*.toml`, the thin `Dockerfile`, or the in-container
Rust (`entry.rs` / `worker.rs` / `monitor_main.rs`). **No tag.**

```bash
# 1. Commit + push main
git add <your changed files>
git commit -m "fix: <what changed>"
git push origin main

# 2. Rebuild the local agent image (pulls latest main, COPYs MCP/, rebuilds entry)
n8 build                 # add --json-progress for non-TUI / scripted output
```

- `n8 build` runs `git pull` on its project dir (`~/.nemesis8/project`, a clone of
  `main`) first, so it always builds from what you just pushed.
- Tags the local image `nemesis8:latest`.
- **You must start a NEW session to use it** — `n8 interactive` / `n8` → "+ New
  session". MCP files are baked at build time and copied in at container start,
  so **attaching to an already-running container won't have the change.**

---

## D. Installer + landing page — `nemesis8.nuts.services`

For `nuts.services/nemesis8-site/` (the `install.ps1` / `install.sh` served at
`nemesis8.nuts.services`, plus the landing page). This is a **Google Cloud Run**
service, deployed manually. **The repo lives outside nemesis8** (it's its own
repo under the `nuts.services` orchestration workspace).

```bash
cd C:/Users/kordl/Code/DeepBlueDynamics/nuts.services/nemesis8-site
bash deploy.sh
```

- Needs `gcloud` authenticated to project `gnosis-459403`
  (`gcloud auth login` if not — run it yourself in a terminal; it's interactive).
- `deploy.sh` runs `gcloud builds submit` + `gcloud run deploy`
  (service `nemesis8-site`, region `us-central1`, domain `nemesis8.nuts.services`).
- nginx serves the scripts with `Cache-Control: no-cache`, so the fix is **live
  immediately** after the deploy — no propagation wait.
- ⚠️ This deploy is **manual and easy to forget** — if you change an installer
  but don't run `deploy.sh`, the live URL keeps serving the old one.

---

## Verifying a release

```bash
gh run list -L 5                                   # recent CI runs
gh release view vX.Y.Z                             # the GitHub release + assets
docker pull deepbluedynamics/nemesis8-base:latest  # base image landed
curl -fsSL https://nemesis8.nuts.services/install.sh | head   # live installer
```

## Common gotchas

- **Tagged but binary version doesn't match** → you tagged before the version-bump
  commit was pushed. Re-bump, re-commit, delete + re-push the tag.
- **`n8 build` didn't pick up my MCP change** → it pulls `~/.nemesis8/project`; make
  sure you pushed to `main` first (it builds from the pull, not your dev tree —
  unless `NEMESIS8_PROJECT_DIR` points at your checkout).
- **Installer still broken after I fixed it** → you didn't run Channel D
  (`deploy.sh`). The repo fix isn't live until Cloud Run redeploys.
- **Base build red** → almost always `uv` can't resolve `requirements.txt`. Read
  the `docker-base.yml` run log.
</content>
