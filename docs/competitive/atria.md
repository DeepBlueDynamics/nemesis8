# atria

A Go TUI **control center** for AI coding agents. Unlike nemesis8 and awman,
atria doesn't *run* agents in a box it owns — it **observes agents already
running in your existing terminals** (tmux, iTerm2, Kitty, WezTerm) and gives
you one pane to watch status, send prompts, and switch between them.
*Observer, not owner.*

## What it is
- Discovers agents across **tmux / iTerm2 / Kitty / WezTerm**, plus a built-in
  **PTY backend** to launch agents itself (with an embedded terminal,
  `Ctrl+\` to exit).
- Supports Claude Code, Codex, OpenCode, GitHub Copilot.
- **Status detection by screen-scraping**: it periodically reads the terminal
  screen and pattern-matches known agent UI signatures to infer
  **working / idle / needs input / error** — no agent API.
- Talks to each multiplexer via its native control channel: iTerm2
  protobuf-over-WebSocket (Python API), Kitty `kitten @` unix socket
  (`allow_remote_control yes`), `tmux`, `wezterm cli`.
- Config `~/.config/atria/config.toml`: `watch_dirs`, `default_agent`, `theme`,
  `integrations`. Keys: `j/k` nav, `n` new, `f` focus, `Enter` send, `B` batch,
  `I` settings, `?` help.

## Stack
- Go (~99.7%), Makefile, goreleaser. macOS/Linux. `brew install
  sethdeckard/tap/atria` or `go install …@latest`.

## Core contrast with nemesis8

| | atria | nemesis8 |
|---|---|---|
| Where agents run | Your **real host terminals** (no sandbox) | **Containers** it controls |
| Discovery | **Screen-scrape** multiplexers/emulators | Docker **labels** + `docker ps` reconcile |
| Status source | Pattern-match terminal output | **Structured telemetry** (in-container monitor → JSONL/HTTP) |
| Reach | Single host, any terminal | Single host now; cross-host fleet planned |
| Ethos | Non-invasive overlay | Sandbox-and-own |

atria's screen-scraping is **universal but fuzzy** (works for any agent in any
terminal, infers status from pixels). n8's monitor is **precise but narrow**
(real events, only for n8-launched containers).

## Convergent design (validates n8's control room)
Both landed on nearly the same TUI grammar: `n`=new, `f`/attach=focus,
`Enter`=act, `?`=help, `I`=settings, `j/k` nav, batch ops. n8's `controlroom.rs`
(Running/Sessions tabs, `n` modal, `a` attach, detail overlay) shares these
instincts — a good signal the control-room UX is on the right track.

## What's worth borrowing
1. **A "needs input" status (highest value).** atria's best idea. n8's registry
   states are Starting/Running/Idle/Exited/Killed — none answer the fleet
   question that matters: *which agents are blocked waiting on me?* The
   in-container monitor already sees agent output, so n8 can detect this cheaply
   and surface it in the Running tab + as a fleet rollup.
2. **Batch prompt / operations (`B`).** Send one prompt to N agents. Natural fit
   for the gateway/control-plane (`/agents` already enumerates the fleet).
3. **Screen-scrape as a _fallback_ detector.** For discovered-but-not-n8-launched
   containers (the plan adopts hand-started ones), a light pattern-match on a
   `docker logs` tail could fill in status where there's no monitor telemetry.

## Where n8 shouldn't follow
atria's multiplexer-integration model is the opposite of n8's
containerize-everything thesis. The portable lesson is the **status taxonomy and
fleet-attention UX**, *not* the host-terminal discovery mechanism.

## Candidate n8 work items (open later)
- [ ] **"Needs input" agent status** + surface it in the control room / fleet. → [#40](https://github.com/DeepBlueDynamics/nemesis8/issues/40)
- [ ] **Batch prompt** across selected agents. → [#43](https://github.com/DeepBlueDynamics/nemesis8/issues/43)
- [ ] (optional) **log-tail status fallback** for adopted/discovered containers.

## Attribution
- **Repo:** https://github.com/sethdeckard/atria
- **Author:** Seth Deckard (`sethdeckard`)
- **License:** MIT — *idea reuse is unrestricted; copying code requires
  preserving the copyright + permission notice (see [README](README.md) §3).*
- **Examined:** 2026-06-08 (README only; no local clone)
