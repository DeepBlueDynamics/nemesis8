# Podman 6 adoption — boxes without Docker as first-class

Podman 6.0.0 (2026-07-01) finalized the modern stack (Netavark/Pasta/nftables,
Docker compat API v1.44, CDI GPU, libkrun default on macOS, quadlets matured).
nemesis8 already runs on podman via the compat socket
(`runtime.rs::detect_container_socket`); this plan closes the remaining gaps so
a podman-only box (the Mac, fresh Linux installs) is not a second-class citizen.

Written against **v0.18.11**. Verify line numbers before editing — they drift.

## Already shipped (context, don't redo)

- **0.18.10**: Linux `--add-host=host.docker.internal:172.17.0.1` fallback
  gated to `runtime_binary == "docker"` (podman's bridge is 10.88.0.1);
  `host.containers.internal` added as a probe candidate in
  `entry.rs::host_gateway_alias` + `probe_hyperia`.
- **0.18.7**: `host.docker.internal ↔ localhost` auto-toggle at config-write —
  podman 6 formalizing `host.containers.internal = 127.0.0.1` under
  `--net=host` matches this design exactly.
- Confirmed non-issues: we never parse CLI label output (`{{json .Labels}}`
  format change) and never run `volume prune` (semantics change).

---

## Phase 1 — `n8 doctor` podman-6 readiness (cheap, do first)

The 5→6 upgrade has real foot-guns; doctor should catch them before a user
hits a cryptic failure. In the doctor path (`src/main.rs`, `Command::Doctor`,
~line 219/664):

- Detect podman + version (`podman --version`).
- **v6-specific warnings**:
  - Linux `podman machine` VMs created under v5 must be **recreated** (volume
    mounts broke); detect via `podman machine list --format json` age/provider
    and print the recreate commands.
  - cgroups v1 host → unsupported; say so explicitly (Linux:
    `/sys/fs/cgroup/cgroup.controllers` missing = v1).
  - Intel Mac / Windows 10 → unsupported by podman 6; suggest staying on 5.x.
- **v5 hint**: if podman 5.x, note 6 is out + the machine-recreation caveat
  (upgrade deliberately, not accidentally).
- Mention `podman machine os update` as the maintenance path (6+ only).

Tests: pure fns `parse_podman_version(&str)` + `podman6_warnings(...) -> Vec<String>`
unit-tested; doctor output is best-effort strings.

## Phase 2 — GPU on podman (CDI / AMD)

Podman 6: `--gpus` covers AMD, CDI devices reliable through the compat API,
`podman info` reports CDI specs.

- `src/docker.rs:~1914` (bollard path) hardcodes
  `DeviceRequest { driver: "nvidia", .. }`. On podman, prefer **CDI**: when
  `runtime_binary == "podman"`, emit `HostConfig.Devices` CDI entries
  (`nvidia.com/gpu=all` / `amd.com/gpu=all`) instead of the nvidia
  DeviceRequest. Which vendor: probe `podman info --format json`
  → `.host.cdiSpecDirs` + discovered devices; fall back to nvidia.
- `src/docker.rs:~2196` (CLI `--gpus=all` run path) already works as-is on
  podman 6 (incl. AMD) — no change.
- Image-side CUDA layers stay NVIDIA-only for now; AMD boxes get device
  passthrough without the CUDA runtime layer (document in doctor output).

Acceptance: `n8 --gpu` on a podman box passes devices without the
"could not select device driver" error; CPU fallback path unchanged.

## Phase 3 — Quadlets: services survive reboots on podman boxes

Today `services/*.toml` (ferricula, transcription, chisel, future aggregator)
are started by nemesis8 and die with the daemon/boot. Podman 6 quadlets are
systemd-native and matured (subdir layout, `.volume` UID/GID/Options, REST
API).

- New: `n8 services quadlet <name> [--install]` — render a `ServiceSpec`
  (`src/service_def.rs:15`) to a `.container` unit:
  - `Image=`/`Exec=` (from `command`), `Network=` (gnosis-network as a
    `.network` unit), `PublishPort=`, `Volume=`, `Restart=` (map our restart
    policy), `Label=nemesis8.service=…` (so `n8 services status` still sees
    them).
  - `--install` writes to `~/.config/containers/systemd/nemesis8/` +
    `systemctl --user daemon-reload` + `enable --now`; bare form prints to
    stdout.
- `ensure_service` grows an awareness check: if a quadlet-managed unit exists
  for the name, don't double-start — report "managed by systemd".
- Docker boxes: command errors with "quadlets are podman+systemd only; use
  `restart: always` instead" (our restart policy already covers docker).

File: new `src/quadlet.rs` (pure `ServiceSpec -> String` render, unit-tested
against golden files) + CLI wiring in `cli.rs`/`main.rs`.

## Phase 4 — charon isolation: blackhole routes

Charon's consumer network relies on `internal: true` (bollard
`CreateNetworkOptions`, `src/charon.rs:20`). Netavark 2 adds
`--route <cidr>,blackhole|unreachable|prohibit` at network create.

- On podman boxes, add explicit blackhole routes for RFC1918 +
  host-gateway ranges to the per-session network so even a
  misconfigured/priv container can't route past the proxy — defense in depth
  on top of `internal`.
- Bollard may not expose route options → shell out to
  `podman network create` for this path (charon already knows
  `runtime_binary`).
- No-op on docker (option doesn't exist); `internal` remains the baseline.

## Phase 5 — OOM-killed agents surfaced

Podman 6 `died` events carry `OOMKilled`; docker exposes the same via
`inspect .State.OOMKilled`. The monitor (`src/monitor.rs` / `collectors.rs`)
currently derives everything from /proc — it never notices *why* an agent
died.

- On agent-container exit (the control room's ~2s refresher in `main.rs`
  already sees containers vanish), inspect the exited container once:
  `State.OOMKilled`, `ExitCode`. Emit an `agent_died` event
  (`{agent_id, exit_code, oom}`) into `events.jsonl` via the existing
  `JsonlSink`.
- LOGPANE: `agent_died` events render with the OOM flag highlighted — "your
  agent didn't crash, it was OOM-killed" is a one-glance answer.

## Ordering & channels

| Phase | Size | Channel |
|---|---|---|
| 1 doctor | S | A (host binary) |
| 2 GPU/CDI | M | A |
| 3 quadlets | M | A |
| 4 charon routes | S-M | A |
| 5 OOM events | S | A (+ LOGPANE view) |

All host-binary work (Channel A); nothing here touches the container image.
Phases are independent — land in any order; 1 first (protects the Mac
upgrade), then 2 (GPU boxes), 3–5 as wanted.

## Out of scope

- Switching default rootless forwarder to Pesto (podman marks it
  experimental; revisit when they flip the default).
- AMD ROCm layers in the image (CUDA-equivalent for AMD) — separate build
  epic.
- Quadlet REST API integration (we render files; the API adds nothing for us
  yet).
