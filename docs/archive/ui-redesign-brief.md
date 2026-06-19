# Design brief: nemesis8 terminal UI v3

**Audience:** a research/design agent. Your job: study this brief (and the code it
cites), research prior art, and produce a full UI design for nemesis8's terminal
surfaces. You are designing, not implementing — deliverables at the bottom.

## What nemesis8 is

A single Rust binary (`n8`) that runs AI coding agents (codex, claude, gemini,
antigravity, grok, ollama, …) inside Docker/Podman containers. Users launch agents,
attach to running ones, resume past sessions, search session transcripts, build the
agent image, and (soon) supervise a cross-host fleet. The TUI is the cockpit for all
of that.

## Current surfaces (study these)

| Surface | Entry | Code |
|---|---|---|
| **Control room** (home) | bare `n8` / `n8 --danger` | `src/controlroom.rs` |
| Resume/attach picker (older, overlaps control room) | `n8 resume`, `n8 attach` | `src/picker.rs` |
| Build progress TUI | `n8 build` | `src/ui.rs` (+ `run_build_cli` in docker.rs) |
| Plain-stdout commands | `n8 sessions`, `n8 ps`, `n8 doctor`, `n8 agents` | various |

### Control room today (v0.13.x)
- **Menu bar** (Microsoft `edit`-style): `Session` (New session, Find) and `Help`
  (Keys, About). Alt+S / Alt+H or click. By design, menus contain ONLY actions with
  an in-pane representation (see Laws below).
- **Top action bar** (cheat sheet) + **bottom nav bar**, both generated from a single
  `KEYS` table that also feeds Help▸Keys: top = `n` new · `⏎` open · `a`
  attach/resume · `.` resume here · `/` find · `Tab` switch tab · `q` quit;
  bottom = `↑↓/jk` move · `PgUp/PgDn` page · `Home/End` ends. Bottom bar is
  navigation-only, colorized.
- **Two tabs**: `Running` (live containers: NAME, PROV, SESSION ID, UPTIME,
  WORKSPACE) and `Sessions` (saved: SESSION ID, PROV, MODIFIED, WORKSPACE).
- **Detail overlay** (Enter): key/value metadata + last log lines for the selected
  row.
- **New-session modal** (`n`): provider pulldown (mouse + keys), model (free-text —
  a models pulldown is planned, fed by a `/models` endpoint), danger checkbox,
  Launch/Cancel.
- **Find** (`/`): filters the visible tab live. Distinct from **Search** (BM25
  full-text over all saved transcripts — exists as `n8 sessions <query>` CLI, has no
  in-pane UI yet).
- **Mouse**: click menus/tabs/rows/modal fields, wheel scroll.
- Full keyboard parity. Workspace paths render in host-native form (`C:\…` on
  Windows).

## The owner's established laws (hard requirements — learned through iteration)

1. **Everything done should be GUI-visible.** If an action can't be shown in the main
   pane, it doesn't belong in a menu. Menus are not a junk drawer.
2. **No duplicate hints**: each key hint lives in exactly one bar (top = actions,
   bottom = navigation only). The cheat sheet, bars, and Help must come from a single
   source of truth in code, so they can't drift.
3. **Find ≠ Search.** Find filters what's on screen. Search ranks all saved session
   content. Both must exist and be visibly distinct.
4. **Modals are real modals** — centered, focus-trapped, fully mouse-driven
   (pulldowns clickable; an off-by-one that made the top pulldown row unclickable was
   a real reported bug — hit-testing precision matters).
5. **Resume must drop the user in the session's original workspace** (and ideally
   leave the shell there on exit).
6. **Paths display host-native.**
7. Provider lists, models, etc. are **data-driven** — never hardcode provider names
   in UI code.

## Known pain points / explicit owner asks (the backlog to design for)

- **Detail overlay is cramped**: wants last ~20 log lines, scrollable, in the detail
  view of a running agent.
- **Lifecycle verbs missing**: kill (`k`), view logs (`l`), refresh (`r`) and a ~2s
  auto-refresh of the Running tab.
- **Model pulldown** in the new-session modal (fed per-provider from a models API;
  must degrade to free-text when offline).
- **In-pane views that don't exist yet**: Fleet (cross-host agents), Container/image
  status, Tools (MCP list per session), Config viewer, full-text Search results.
  These were *removed from menus* because they had no pane — the redesign should
  give them panes (or consciously cut them).
- **"Needs input" agent status** (borrowed idea from atria — see
  `docs/competitive/atria.md`): the fleet question that matters is *which agents are
  blocked waiting on me?* Status taxonomy today is only running/exited.
- **Batch operations**: send one prompt to N selected agents (atria's `B`).
- **Danger mode visibility**: `n8 --danger` looks identical to safe mode. The owner
  passes `--danger` deliberately; the UI should make the armed state unmistakable
  (think: colored frame/badge) without being annoying.
- The **picker** (`src/picker.rs`) and control room overlap ~80% — the redesign
  should unify them into one component.
- Attach/detach ergonomics: detach chord is remapped to `ctrl-^`; sessions had a
  history of "half-disconnect" confusion — status of *am I attached, detached, did
  the agent exit?* should be legible.

## Constraints

- **Stack**: Rust, ratatui + crossterm. Keep it — no GUI frameworks, no web views.
- **Terminals**: Windows Terminal, macOS Terminal/iTerm, Linux, and Hyperia (the
  owner's own terminal, which adds mouse + pane tooling). Must degrade gracefully
  over plain SSH. 16-color fallback; assume 256-color/truecolor when available.
- **Mouse + keyboard parity** for every action.
- Latency: list data comes from `docker ps` + filesystem scans; design for async
  refresh (stale-while-revalidate), never a frozen UI.
- Single binary; no config required for the UI to be useful.
- Respect the existing single-source-of-truth pattern for keymaps (`KEYS` const).

## Research the design agent should do

- Survey TUI control-plane prior art: lazydocker, k9s, lazygit, atria
  (`docs/competitive/atria.md`), btop, Microsoft `edit`, opencode/claude-code TUIs.
  What navigation/status idioms transfer to "fleet of agents" supervision?
- Status-at-a-glance patterns for N concurrent agents (needs-input, working, idle,
  exited) — color, glyphs, grouping, notification affordances in a terminal.
- Information architecture: tabs vs. a sidebar vs. a k9s-style resource switcher,
  given the view inventory above (Running, Sessions, Fleet, Tools, Config, Search,
  Build).
- Modal vs. inline editing for the new-session flow (provider/model/danger/workspace).

## Deliverables (what you must output)

1. **Information architecture**: full view inventory, navigation model, how every
   backlog view fits (or an argued cut list).
2. **ASCII wireframes** of every screen/state: home, each view, detail, new-session
   flow, search results, danger-armed state, empty states, error states.
3. **Interaction spec**: complete keymap (extending the single-source `KEYS` model),
   mouse map, focus rules, modal behavior, auto-refresh policy.
4. **Status taxonomy**: agent states incl. "needs input", their glyphs/colors, and
   how they roll up at fleet level.
5. **Component inventory** mapped to ratatui widgets, with notes on what
   `controlroom.rs`/`picker.rs` keep vs. replace, and a migration order (the owner
   ships small patches — design must land incrementally, not big-bang).
6. **Open questions** you couldn't resolve, each with your recommended default.

Constraints on the output: plain markdown, ASCII diagrams (no images), concrete over
abstract — every recommendation tied to a law, pain point, or researched precedent
above.
