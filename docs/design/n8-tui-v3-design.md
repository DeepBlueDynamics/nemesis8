# nemesis8 terminal UI v3 — design

Design for the `n8` TUI surfaces, per the v3 brief. Stack stays ratatui + crossterm.
Every recommendation is tagged with its justification: **[Law N]** (owner's laws),
**[Pain: …]** (backlog item), or **[Prec: …]** (researched precedent).

**Inputs and assumptions.** This design is based on the brief only; `controlroom.rs`,
`picker.rs`, `ui.rs`, and `docs/competitive/atria.md` were cited but not available,
so where the code would have settled a detail I state an assumption and flag it in
§6. Prior art surveyed: k9s, lazygit, lazydocker, btop, Microsoft `edit`,
opencode/Claude Code-style agent TUIs, plus current TUI design guidance (80×24
minimum, constraint layouts, context-sensitive footers, `/`-to-filter as the
universal pattern).

---

## 1. Information architecture

### 1.1 The core consolidation: five views, not nine

The backlog names nine candidate surfaces (Running, Sessions, Fleet, Tools, Config,
Container/image status, Search, Build, picker). Nine top-level views in a tab bar
violates the spirit of Law 1 — a tab bar that scrolls off-screen is no longer
GUI-visible. The design consolidates to **five top-level views** by merging things
that answer the same user question:

```
[1 Agents]  [2 Sessions]  [3 Search]  [4 Build]  [5 System]
```

| View | Absorbs | Question it answers |
|---|---|---|
| **1 Agents** | Running tab, Fleet, picker's attach half | "What is alive, where, and what needs me?" |
| **2 Sessions** | Sessions tab, picker's resume half | "What can I resume?" |
| **3 Search** | `n8 sessions <query>` (BM25) | "Where did an agent say X?" |
| **4 Build** | build TUI (`src/ui.rs`) | "Is my image ready?" |
| **5 System** | Container/image status, Config viewer, doctor | "Is the machine healthy / what is my config?" |

Argued merges and cuts:

- **Fleet is not a separate view — it is the Agents view with a HOST column.**
  [Pain: Fleet] [Prec: k9s, which shows one resource list and scopes it by context
  rather than duplicating views per cluster]. On a single host the HOST column is
  hidden and the view is byte-identical to today's Running tab; when remote hosts
  are configured the column appears and the status rollup (§2.4) aggregates across
  hosts. This means fleet support lands as a *column + data source*, not a new
  screen — which matches the owner's incremental-patch shipping style (§5.3).
- **Tools (MCP list) is not top-level — it is a section of the detail view.**
  Tools are per-session data; a top-level Tools view would always need a "for which
  session?" selector, i.e. it has no in-pane representation of its own [Law 1].
  Detail gets three sections: Meta / Tools / Logs (§3.4).
- **Config + container/image status + doctor merge into one System view.** All
  three are read-only "is my environment sane" panels with no row selection; alone,
  each is too thin to justify a tab. [Pain: in-pane views that don't exist yet]
- **The picker dies.** `n8 resume` and `n8 attach` open the same control-room
  component in **pick mode**: same tables, same keys, but Enter performs the
  attach/resume directly and the app exits into the agent. The brief says overlap
  is ~80% [Pain: picker overlap]; the remaining 20% (Enter semantics, no menu bar
  needed) becomes a `Mode::Pick` flag, and `picker.rs` is deleted at parity (§5.3).
- **Build stays a separate entry path short-term** (`n8 build` launches straight
  into view 4) but is restyled onto the shared theme and chrome so it *looks* like
  tab 4 of the same app. Full embedding (kicking off builds from inside the control
  room) is deferred — see open question Q6.

### 1.2 Navigation model

- **Tab bar, always visible, numbered.** `Tab`/`Shift+Tab` cycle; `1`–`5` jump
  directly. Numbers are printed in the tab labels so the shortcut is self-evident
  [Law 1] [Prec: lazydocker's numbered panels, btop's numbered boxes].
- **No k9s-style `:command` switcher.** It is powerful but invisible — it fails
  Law 1 (no in-pane representation) and adds a parser for five views. If the view
  count ever grows past ~7, revisit.
- **Menu bar stays** (Microsoft `edit` style) and gains a `View` menu listing the
  five views with their number keys — menus only contain things visible in the
  pane, which all five views now are [Law 1]. Menus: `Session` (New, Kill, Logs,
  Batch prompt, Find, Search transcripts), `View` (Agents, Sessions, Search,
  Build, System, Refresh now), `Help` (Keys, About).
- **Layer model** (strict stack, Esc pops topmost): base view → find bar →
  detail overlay → modal (new-session / confirm / batch). Only one modal ever; a
  modal may not open another modal except its own pulldowns (§4.4).

### 1.3 What each view contains

**1 Agents** — table: `ST · NAME · PROV · SESSION ID · UPTIME · [HOST] · WORKSPACE`.
Default sort: needs-input first, then working, then idle, each by uptime desc
[Pain: needs-input — "which agents are blocked waiting on me?" is *the* fleet
question, so blocked rows must be at the top, not findable]. Rollup strip under the
tab bar (§2.4). Auto-refresh 2 s, stale-while-revalidate (§4.5). Marks
(multi-select) enable batch ops (§4.2).

**2 Sessions** — table: `SESSION ID · PROV · MODIFIED · WORKSPACE`, newest first.
Enter = detail; `a`/`.` resume (in original workspace — Law 5 is a backend contract
the UI surfaces by always *showing* the workspace it will resume into, host-native
[Law 6]).

**3 Search** — input field + ranked results with snippets. Reuses the existing BM25
backend of `n8 sessions <query>`. Visually distinct from Find: Search is a *view*
with a persistent query box and ranked, snippeted results across all sessions; Find
is a transient filter bar inside another view [Law 3]. Enter on a result jumps to
that session's detail (Logs section scrolled to the hit).

**4 Build** — existing progress UI restyled: step list with per-step status glyphs
(same taxonomy as §2, reused — one visual language), scrolling log pane, summary
footer.

**5 System** — three stacked read-only panels: Image (tag, digest, age, "rebuild
hint"), Runtime (docker/podman version, reachable hosts + latency), Config
(effective config with source annotations: default / file / flag). `doctor` checks
render as pass/fail rows with the §2 glyphs.

---

## 2. Status taxonomy

### 2.1 Agent states

Two-axis model: *lifecycle* (starting → live → exited) crossed with *attention*
(does it need me?). Flattened to seven displayable states:

| State | Glyph | ASCII | Color (256/true) | 16-color | Meaning / detection |
|---|---|---|---|---|---|
| starting | `◌` | `~` | cyan | cyan | container created, agent not yet emitting |
| **needs input** | `◉` | `!` | **yellow, bold** | yellow bold | agent blocked on user (§2.2) |
| working | `●` | `*` | green | green | output within quiescence window |
| idle | `○` | `.` | dim white | white | live, quiet, not waiting on TTY read |
| exited ok | `■` | `-` | dim blue | blue | exit code 0 |
| exited err | `✖` | `x` | red | red | nonzero exit |
| unreachable | `?` | `?` | magenta dim | magenta | fleet host not responding [Pain: Fleet] |

Rules: glyph + color **always together** — never color alone (16-color SSH fallback
and color-blind safety; the glyph column is the truth, color is reinforcement)
[Prec: k9s status column; standard TUI degradation guidance]. ASCII column is the
fallback when the terminal/locale can't render the glyphs (detected once at startup,
overridable by flag).

### 2.2 Detecting "needs input"

[Pain: needs-input, borrowed from atria — atria doc unavailable, so this is the
recommended default, flagged as Q1.] Layered heuristic, in priority order:

1. **Provider markers** — per-provider regexes over the log tail (e.g. a trailing
   prompt like `? `, `(y/n)`, provider-specific "waiting for approval" lines).
   These live in the same data-driven provider table as everything else — never
   hardcoded in UI code [Law 7].
2. **TTY-read + quiescence** — agent process is blocked reading its TTY *and* no
   output for ≥ 5 s.
3. Otherwise: output in the last 5 s → **working**; else **idle**.

The state machine runs in the data layer and ships a plain enum to the UI; the UI
never re-derives status from logs (single source of truth, in the spirit of Law 2).

### 2.3 Attach-state legibility

[Pain: "am I attached, detached, did the agent exit?" half-disconnect confusion.]
The selected agent's detail header and the Agents table both carry an explicit
attach field: `attached (you)` / `detached` / `attached elsewhere` (fleet). On
detach (`ctrl-^`), n8 prints a one-line epilogue to the shell — `detached from
brave-fox · still running · reattach: n8 attach brave-fox` — and the same fact
appears as a toast on next control-room entry. Exit vs. detach are visually
incompatible states: an exited agent's row moves to the exited group with `■`/`✖`,
so "half-disconnected" can't be misread as "dead".

### 2.4 Fleet rollup

A one-line strip under the tab bar, always present on the Agents view:

```
◉ 2 waiting   ● 3 working   ○ 1 idle   ■ 4 done   ? 1 host unreachable
```

The waiting count is the headline number; when > 0 it is bold yellow and the
Agents tab label gains a suffix: `[1 Agents ◉2]`. Per-host grouping appears only
when >1 host: rows group under dim host header lines, collapsible with `←/→`
[Prec: lazygit's collapsible sections; k9s shows context in header rather than
per-row repetition]. Optional terminal-bell-on-new-waiting is config-gated and off
by default (notification affordance without annoyance).

---

## 3. Wireframes

All frames drawn at 80×24 (the design minimum; layouts are constraint-based —
percent/min — and scale up) [Prec: 80×24 baseline, constraint layouts]. Chrome,
top-to-bottom: menu bar · action bar (top, actions only) · tab bar · rollup strip ·
content · nav bar (bottom, navigation only) [Law 2].

### 3.1 Home — Agents view (single host, safe mode)

```
 Session   View   Help                                        ⟳2s  14:02:11
 n new  ⏎ open  a attach  . here  k kill  l logs  Space mark  b batch  / find
 [1 Agents ◉2] [2 Sessions] [3 Search] [4 Build] [5 System]
 ◉ 2 waiting   ● 3 working   ○ 1 idle   ■ 1 done
  ST  NAME           PROV     SESSION ID    UPTIME   WORKSPACE
 ▸◉   brave-fox      claude   a1b2c3d4      12m      C:\src\api
  ◉   calm-owl       codex    e5f6a7b8      3m       C:\src\web
  ●   bold-ant       gemini   c9d0e1f2      1h02m    C:\ml\train
  ●   keen-elk       claude   d3e4f5a6      44m      C:\src\api
  ●   wise-ram       ollama   b7c8d9e0      9m       C:\tools\n8
  ○   slow-bee       grok     f1a2b3c4      2h11m    C:\docs
  ■   tame-cat       codex    a9b8c7d6      —        C:\src\api



 ↑↓/jk move   PgUp/PgDn page   Home/End ends   Tab/1-5 view   q quit
```

Notes: `▸` marks selection; needs-input rows sort to top; workspace renders
host-native [Law 6]. `Space mark`/`b batch` live in the top bar because they are
actions; all movement lives in the bottom bar [Law 2]. Clock + `⟳2s` show the
auto-refresh contract is live (§4.5).

### 3.2 Agents view — fleet (multi-host) with marks and an unreachable host

```
 [1 Agents ◉2] [2 Sessions] [3 Search] [4 Build] [5 System]
 ◉ 2 waiting   ● 2 working   ○ 1 idle   ■ 1 done   ? host gpu-02 unreachable
  ST  NAME           PROV     SESSION ID    UPTIME   HOST     WORKSPACE
  ▾ local ───────────────────────────────────────────────────────────────
 ▸◉ ✓ brave-fox      claude   a1b2c3d4      12m      local    C:\src\api
  ● ✓ keen-elk       claude   d3e4f5a6      44m      local    C:\src\api
  ○   slow-bee       grok     f1a2b3c4      2h11m    local    C:\docs
  ▾ build-01 ────────────────────────────────────────────────────────────
  ◉   calm-owl       codex    e5f6a7b8      3m       build-01 /srv/web
  ● ✓ bold-ant       gemini   c9d0e1f2      1h02m    build-01 /srv/ml
  ▸ gpu-02 ? unreachable (last seen 14:01:40) ────────────────────────────
```

`✓` = marked for batch; host group lines collapse with `←/→`; the unreachable host
keeps its last-seen rows hidden under a collapsed header rather than silently
vanishing [Pain: Fleet; stale-while-revalidate §4.5].

### 3.3 Agents view — empty and error states

```
  (empty)                                 (docker unreachable)
  ┌──────────────────────────────┐        ┌──────────────────────────────┐
  │                              │        │ ✖ cannot reach docker daemon │
  │   No running agents.         │        │   retrying every 2s…         │
  │   n  launch a new session    │        │   last good data 14:01:32    │
  │   2  resume a saved session  │        │   d  run doctor (System)     │
  │                              │        │  (stale list shown below,    │
  └──────────────────────────────┘        │   dimmed)                    │
                                          └──────────────────────────────┘
```

Empty states teach the next action instead of showing a bare table [Prec: standard
TUI guidance — visible shortcuts, memory not required]. The error banner sits above
the (dimmed, stale) last-known table — the UI never freezes or blanks.

### 3.4 Detail overlay (Enter on a running agent) — Logs section

```
 ┌ brave-fox · claude · a1b2c3d4 ──────────────────────────── [Meta Tools Logs] ┐
 │ ◉ needs input · attached: no · uptime 12m · host local                       │
 │ workspace C:\src\api                                                         │
 ├──────────────────────────────────────────────────────────────────────── Logs ┤
 │ 14:01:55  applied patch src/routes/auth.rs                                ▲  │
 │ 14:01:57  cargo check … ok                                                █  │
 │ 14:02:01  ? Allow network access for `curl api.test`? (y/n)               █  │
 │           ── agent is waiting for input ──                                ▼  │
 │                                                                              │
 │  m meta  t tools  l logs   ↑↓ scroll   a attach   k kill   Esc close         │
 └──────────────────────────────────────────────────────────────────────────────┘
```

Sized ~80 % of the screen, not the old cramped box [Pain: detail cramped]. Logs
buffer 200 lines, render a scrollbar, follow tail until the user scrolls (then a
`▼ end` hint to re-follow). The "waiting for input" rule line shows *why* the
status is ◉. Tools section lists MCP servers/tools for the session as a plain list
(absorbing the Tools view, §1.1). The overlay's own keys appear in its footer —
this is the one sanctioned exception to "hints live in the bars," because the
overlay is a focus layer with its own keymap, still generated from the same `KEYS`
table [Law 2: single source, not duplicated — these keys appear nowhere else].

### 3.5 New-session modal — model pulldown open

```
            ┌ New session ────────────────────────────────┐
            │  Provider   [ claude        ▾]              │
            │  Model      [ sonnet-4.6    ▾]              │
            │             ┌─────────────────────┐         │
            │             │▸ sonnet-4.6         │         │
            │             │  opus-4.8           │         │
            │             │  haiku-4.5          │         │
            │             │  (type to filter…)  │         │
            │             └─────────────────────┘         │
            │  Workspace  [ C:\src\api          ] (cwd)   │
            │  [ ] danger — agent may touch host          │
            │                                             │
            │        [ Launch ⏎ ]      [ Cancel Esc ]     │
            └─────────────────────────────────────────────┘
```

Model list is fetched per-provider from the `/models` endpoint when the provider
field changes; on failure the field silently degrades to free-text with a dim
`(offline — type model name)` hint [Pain: model pulldown; Law 7 — list is data,
not code]. Pulldown rows are mouse-clickable **including row 0** — hit-testing must
use the same `Rect` math as rendering, with an explicit unit test for the top row
[Law 4 + the reported off-by-one bug]. Focus is trapped: Tab cycles fields, Esc
closes pulldown first, then modal [Law 4].

### 3.6 Find vs. Search [Law 3]

```
 Find (transient bar inside any table view):
 ┌ /find: api█ ── 3/7 rows ── Esc clear ┐   ← filters live, keeps sort

 Search (view 3 — persistent, ranked, all transcripts):
 [1 Agents ◉2] [2 Sessions] [3 Search] [4 Build] [5 System]
  query: [ rate limit retry█                              ]  ⏎ run
  RANK  SESSION ID  PROV    MODIFIED     SNIPPET
 ▸ 1.0  a9b8c7d6    codex   yesterday    …added retry with exponential
  0.7  f1a2b3c4    grok    3d ago        …rate limit hit on /v1/messages…
  0.4  d3e4f5a6    claude  5d ago        …limiter middleware…
  ⏎ open at hit   . resume here   Esc clear query
```

Different shape, different place, different result semantics — impossible to
confuse [Law 3].

### 3.7 Danger-armed state [Pain: danger visibility]

```
▓ Session   View   Help                          ⚠ DANGER  ⟳2s  14:02:11 ▓
▓ … entire outer frame drawn as a heavy red border; badge persists …     ▓
```

Two reinforcing signals, both passive: (1) the root block gets a heavy **red
border** for the whole run, (2) a `⚠ DANGER` badge sits in the menu bar next to
the clock (reverse-video red). No blinking, no modal nags — unmistakable at a
glance, zero ongoing cost. The new-session modal's danger checkbox renders
pre-checked and red when the process was started with `--danger`.

### 3.8 Kill confirmation and batch prompt

```
 ┌ Kill agent? ───────────────────┐   ┌ Batch prompt → 3 marked agents ──────┐
 │  brave-fox (claude, 12m)       │   │ brave-fox · keen-elk · bold-ant      │
 │  Session a1b2c3d4 is preserved │   │ > Fix the failing auth tests and█    │
 │  and resumable.                │   │                                      │
 │   [ Kill k ]   [ Cancel Esc ]  │   │   [ Send ⏎ ]        [ Cancel Esc ]   │
 └────────────────────────────────┘   └──────────────────────────────────────┘
```

Kill requires the confirm modal (friction on destructive actions); the confirm
explains the consequence (session survives) to make the action safe to learn.
Batch prompt [Pain: batch ops; Prec: atria `B`] sends one prompt to all marked
agents and reports per-agent delivery in a toast.

### 3.9 System view (5)

```
 [1 Agents] [2 Sessions] [3 Search] [4 Build] [5 System]
 ┌ Image ──────────────────────────┐ ┌ Runtime ───────────────────────────┐
 │ ● n8-agent:latest  sha256:ab…   │ │ ● docker 27.1 (local)   12ms       │
 │   built 2d ago · 1.9 GB         │ │ ● podman 5.2 (build-01) 38ms       │
 │   b rebuild (opens Build)       │ │ ✖ gpu-02 — timeout                 │
 └─────────────────────────────────┘ └────────────────────────────────────┘
 ┌ Config (effective) ─────────────────────────────────────────────────────┐
 │ runtime = docker            (default)                                   │
 │ danger  = false             (flag --danger absent)                      │
 │ hosts   = build-01, gpu-02  (file ~/.config/n8/config.toml)             │
 └──────────────────────────────────────────────────────────────────────────┘
```

Doctor checks reuse the §2 glyph language. Read-only; `b` is its one action and
simply switches to the Build view.

---

## 4. Interaction spec

### 4.1 Keymap — the extended single-source `KEYS` table [Law 2]

Each entry carries: key, label, **bar** (Top = action, Bottom = navigation,
Overlay = detail-footer, None = Help-only), and the views/layers where it is
active. Bars, the detail footer, and Help▸Keys all render from this one table;
nothing is hand-written twice.

| Key | Action | Bar | Active in |
|---|---|---|---|
| `n` | New session (modal) | Top | Agents, Sessions |
| `⏎` | Open detail / run search / activate | Top | tables, Search input, modals |
| `a` | Attach (running) / Resume (saved) | Top | Agents, Sessions, Search |
| `.` | Resume **here** (current cwd) | Top | Sessions, Search |
| `k` | Kill (confirm modal) | Top | Agents |
| `l` | Logs — open detail at Logs section | Top | Agents |
| `Space` | Mark/unmark row for batch | Top | Agents |
| `b` | Batch prompt to marked (modal) | Top | Agents (≥1 mark) |
| `/` | Find — live filter bar | Top | Agents, Sessions |
| `s` | Jump to Search view, focus query | Top | everywhere |
| `r` | Refresh now | Top | Agents, Sessions, System |
| `Tab` / `Shift+Tab` | Next / previous view | Top | base layer |
| `1`–`5` | Jump to view N | Top | base layer |
| `q` | Quit (pick mode: cancel) | Top | base layer |
| `↑↓` / `jk` | Move selection | Bottom | tables, pulldowns |
| `PgUp/PgDn` | Page | Bottom | tables, log scroll |
| `Home/End` | First / last | Bottom | tables, log scroll |
| `←/→` | Collapse / expand host group | Bottom | Agents (fleet) |
| `m` / `t` / `l` | Meta / Tools / Logs section | Overlay | detail |
| `Esc` | Pop topmost layer (clear marks if none) | None (Help) | everywhere |
| `Alt+S/V/H` | Open menu | None (Help) | base layer |

Reserved, never bound: `Ctrl+C`, `Ctrl+Z`, `Ctrl+\` (terminal's), `Ctrl+^`
(detach chord, owned by the attach layer). Single-letter verbs are inert while
Find or a modal text field has focus — typing always wins; only `Esc`/`⏎`/`Tab`
are control keys inside text inputs.

### 4.2 Marks and batch

`Space` toggles a mark (`✓` column); marks persist across refreshes (keyed by
container ID, §4.5) but clear on view switch or `Esc`. `b` with ≥1 mark opens the
batch-prompt modal (§3.8); with 0 marks it acts on the selected row. After send, a
toast reports `sent to 3 · 1 failed (calm-owl: not attached)` and failures keep
their marks for retry.

### 4.3 Mouse map [Law 4 — full parity]

| Target | Click | Other |
|---|---|---|
| Menu bar items | open menu; click item = run | hover highlights |
| Tabs | switch view | — |
| Table row | select | double-click = Enter; wheel = scroll |
| Mark column cell | toggle mark | — |
| Status glyph cell | open detail at Logs | — |
| Detail section tabs `[Meta Tools Logs]` | switch section | wheel scrolls logs |
| Modal fields / pulldown rows / buttons | focus & activate | wheel scrolls pulldown |
| Rollup strip counters | filter view to that state (click again clears) | — |
| Scrollbars | jump | drag thumb |

Hit-testing rule: every clickable widget computes its hit `Rect` from the **same
layout value used to render it** — never re-derived arithmetic — and the pulldown
top-row case gets a regression test [Law 4, reported bug].

### 4.4 Focus and modal behavior [Law 4]

One focus owner at all times; the layer stack is `view → find → detail → modal`.
A modal: centers, dims the backdrop (`Clear` + dim style), traps Tab-cycling
within its fields, traps mouse (clicks outside = no-op, not dismiss — accidental
dismissal of a half-filled launch form is worse than one extra Esc). Esc closes
the innermost thing first (pulldown → modal → detail → find). Opening a modal
pauses keybinds of lower layers entirely; auto-refresh continues underneath but
may not steal focus or move the modal.

### 4.5 Auto-refresh: stale-while-revalidate [Constraint: never a frozen UI]

- Tick every 2 s on the Agents view (1 s when any agent is `starting`; paused
  while the help overlay is open — nothing else pauses it).
- Each tick spawns the fetch (`docker ps` + status probes) on a worker; results
  arrive as messages. The draw loop **never** blocks on I/O.
- Render last-known data immediately, always. While a fetch is in flight the
  header shows `⟳`; if the newest data is older than 3 ticks, the table dims and
  the header shows `stale 8s` (and the §3.3 error banner if the daemon is gone).
- **Selection, marks, and scroll are keyed by container ID, not row index** — a
  refresh that reorders rows must not move the user's cursor. New rows appear
  without stealing selection; a deleted selected row moves selection to its
  former neighbor.
- Sessions view refreshes on focus and on `r` only (filesystem scan; no tick).
  Fleet hosts are fetched in parallel with a 1.5 s per-host timeout; a slow host
  degrades to `?` instead of slowing the tick.

---

## 5. Component inventory and migration

### 5.1 Components → ratatui widgets

| Component | ratatui mapping | New / existing |
|---|---|---|
| App shell (chrome, layer stack, tick loop) | `Layout` (vertical constraints) + message-driven update | evolve `controlroom.rs` |
| Menu bar | existing custom widget | keep |
| Action bar (top) / nav bar (bottom) | `Line` of styled `Span`s, generated from `KEYS` | keep, regenerate from extended table |
| Tab bar | `Tabs` | new (replaces 2-tab header) |
| Rollup strip | `Line` of `Span`s; counters store hit-Rects for click-to-filter | new |
| Agent/Session/Search tables | `Table` + `TableState`; selection by ID wrapper | keep core, add ID-keyed state |
| Host group headers | custom row injection above `Table` rows (render rows manually if `Table` grouping is too rigid) | new, fleet-gated |
| Detail overlay | `Clear` + `Block`, `Paragraph` + `Scrollbar` (logs), `List` (tools) | replace old overlay |
| Find bar | 1-line input (hand-rolled or `tui-input`) | keep behavior, restyle |
| Search view | input + `Table` with snippet column (`Cell` with highlighted `Span`s) | new (backend exists) |
| Modals (new-session, confirm, batch) | shared modal frame; pulldown = `List` popup with filter | evolve new-session modal; confirm/batch new |
| Toasts | bottom-right `Clear`+`Paragraph`, 4 s TTL, max 3 stacked | new |
| Danger chrome | root `Block` border style + badge `Span` | new, trivial |
| Build view | existing `ui.rs` widgets, re-themed; §2 glyphs for steps | restyle |
| System view | three `Block`+`Paragraph`/`List` panels | new |
| Theme | one module: state→(glyph, ascii, color16, color256) + spacing | new — everything above consumes it |

**Keep vs. replace:** `controlroom.rs` keeps its event loop, menu bar, `KEYS`
machinery, tables, and new-session modal — it *becomes* the shell. `picker.rs` is
replaced by pick mode and deleted (§1.1). `ui.rs` (build) is kept and re-themed.

### 5.2 Pick mode (replaces picker.rs)

`Mode::Pick { intent: Attach | Resume }` on the shell: opens directly on view 1 or
2, hides the menu bar, retitles the top bar (`⏎ attach · Esc cancel`), Enter
performs the intent and exits the TUI into the agent (resume lands in the original
workspace and leaves the shell there on exit — Law 5 surfaced in UI by printing the
target workspace in the pick footer before confirming).

### 5.3 Migration order — small patches, each shippable [owner ships incrementally]

1. **Theme + status taxonomy** in the existing Running tab (pure render; no
   behavior change). Ships glyphs/colors and the needs-input column with the
   heuristic stubbed to working/idle/exited.
2. **Lifecycle verbs**: `k` + confirm modal, `l`, `r`, 2 s auto-refresh with
   ID-keyed selection. [Pain: lifecycle verbs]
3. **Detail overlay v2** (big, sectioned, scrollable logs). [Pain: cramped]
4. **Danger chrome** (border + badge). One small patch, high value. [Pain: danger]
5. **Needs-input detection** (provider markers + quiescence) lighting up the
   taxonomy from patch 1; rollup strip + tab badge. [Pain: needs-input]
6. **Tab bar (5 views)** + System view; menu gains `View`. (Search tab present
   but routes to a "coming soon" pane only if patch 7 hasn't landed — otherwise
   land 6+7 together.)
7. **Search view** over the existing BM25 backend. [Law 3 gets its UI]
8. **Pick mode; delete picker.rs.** [Pain: overlap]
9. **Model pulldown** with offline degradation (needs `/models`). [Pain: model]
10. **Marks + batch prompt.** [Pain: batch]
11. **Fleet**: HOST column, host groups, parallel fetch, unreachable state.
    [Pain: Fleet] — last because it's the only one gated on new backend plumbing.

Each patch leaves the UI fully usable; nothing before 6 changes navigation, so
muscle memory survives the whole sequence.

---

## 6. Open questions (with recommended defaults)

| # | Question | Recommended default |
|---|---|---|
| Q1 | Exact needs-input detection per provider — `docs/competitive/atria.md` and provider log formats weren't available. Are atria's signals reusable? | Ship §2.2 layered heuristic; provider marker regexes in the provider data table [Law 7]; tune per provider behind that data, not in UI code. |
| Q2 | Does "idle vs. working" need a real signal, or is 5 s output-quiescence enough? | Quiescence only; it's cheap, and the distinction is informational, not actionable. Revisit if idle agents turn out to be a thing users kill. |
| Q3 | Should clicking a rollup counter filter (proposed §4.3) or just scroll-to-group? | Filter (click again to clear) — it reuses Find's machinery and keeps one mental model. |
| Q4 | Batch prompt delivery mechanism (stdin injection vs. provider API) — backend question with UI impact on the failure toast. | Design assumes per-agent ack/fail; UI shows partial failure and keeps failed marks (§4.2) regardless of mechanism. |
| Q5 | Sortable column headers (mouse click)? | Defer. Default sort (§1.3) answers the real question; header sorting adds state with little payoff at <50 rows. |
| Q6 | Embed Build fully (launch builds from the control room) vs. keep `n8 build` entry? | Keep separate entry now; System▸`b` switches to the Build view in-process once the tab bar lands (patch 6), which gets 90 % of the value free. |
| Q7 | Terminal bell / OS notification on new needs-input agent? | Config-gated bell, off by default. No OSC notifications until Hyperia-vs-others behavior is verified — degrade-first. |
| Q8 | Glyph set on terminals without the needed Unicode (old Windows consoles over SSH)? | Auto-detect once at startup; `--ascii` flag forces the ASCII column of §2.1. |
| Q9 | `picker.rs` parity details (it wasn't readable here) — any behavior beyond attach/resume selection? | Audit before patch 8; anything extra becomes a pick-mode flag rather than a second component. |

---

*Every screen above is generated from: 5 views (§1), 7 states (§2), 1 keymap
table (§4.1), 1 theme module (§5.1). If a future feature can't be expressed in
those four vocabularies, that's the signal to revisit this document rather than
bolt on a sixth vocabulary.*
