# LOGPANE Phase D — Per-Agent Network Panel + Sparklines

Adds a live **per-agent network monitor** to the LOGPANE TUI (`e` screen): one row
per active agent showing current download/upload throughput and a recent-history
sparkline. This is the visible payoff of the agent-tagging work (Phase A) — it's
what makes multi-agent monitoring *legible*.

This plan is written against the code as of **v0.18.4** (`b3a932c`). Verify line
numbers before editing — they drift.

---

## 1. What already exists (build on this, don't re-do it)

- **Events are agent-tagged.** Every `metric` event in `~/.nemesis8/home/.monitor/events.jsonl`
  now carries `agent_id` plus `net_rx_bps` / `net_tx_bps`, e.g.:
  ```json
  {"kind":"metric","agent_id":"n8-rosy-robin","cpu_pct":4.8,"load1":14.0,
   "mem_used_kb":8487820,"mem_total_kb":32557196,"net_rx_bps":173,"net_tx_bps":16,"ts":1782779385}
  ```
  (Tagging = `JsonlSink` in `src/monitor.rs`; emitted every 5 s by `collectors::MetricsCollector`.)
- **`src/event_index.rs`**: `EventIndex` (bounded ring, tail-loads the jsonl) holding
  `IndexedEvent { ts: u64, kind: String, agent_id: Option<String>, raw: serde_json::Value, .. }`.
  Query via `EventQuery`.
- **`src/logpane.rs`**: `LogPane` state + `pub fn draw(f, area, pane)`. `draw()` lays out a
  **4-row vertical** stack (search bar `Length(3)` · facets|list `Min(5)` · detail `Length(7)` ·
  help `Length(1)`). Has `pane.live` (LIVE/PAUSED), `Focus::{Search,Kinds,List}`, and a
  `human_bytes(u64) -> String` helper. Interactive loop is `pub fn run(jsonl_path, cap)`.

**So Phase D is purely additive**: derive per-agent net stats from the index that's already
loaded, and render one more panel. No new collectors, no schema change.

---

## 2. Data model (new, in `src/logpane.rs`)

```rust
/// One agent's current + recent network throughput, derived from the index.
pub struct AgentNet {
    pub agent_id: String,
    pub rx_bps: u64,            // latest sample
    pub tx_bps: u64,            // latest sample
    pub history: Vec<u64>,      // last N (rx+tx) totals, oldest→newest, for the sparkline
    pub last_ts: u64,          // newest sample ts (for stale detection)
}
```

### Aggregation
```rust
/// Scan the index for `metric` events, group by agent_id, newest-last.
/// `window` = how many recent samples to keep for the sparkline (e.g. 16).
pub fn agent_net_stats(index: &EventIndex, window: usize) -> Vec<AgentNet>
```
Implementation notes:
- Pull metric events via the existing query: `index.query(&EventQuery { kinds: vec!["metric".into()], limit: 0, ..Default::default() })`. **NOTE:** `query()` returns **newest-first**; reverse per-agent so `history` is oldest→newest.
- For each event, read from `e.raw`: `e.agent_id` (skip if `None`), `net_rx_bps`/`net_tx_bps` (`as_u64`), `ts`.
- Group into a `BTreeMap<String, AgentNet>` (BTreeMap → stable alphabetical order). `rx_bps`/`tx_bps`/`last_ts` come from the **newest** sample; `history` is the last `window` `(rx+tx)` values.
- Cap each `history` to `window` (truncate the oldest).
- Return `Vec` sorted by `agent_id` (or by `rx_bps+tx_bps` desc — pick one; alphabetical is least jumpy).

### Sparkline
```rust
/// Map a series to block glyphs scaled to the window max. Empty → spaces.
fn sparkline(values: &[u64], width: usize) -> String
```
- Glyph ramp: `[' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█']` (9 levels).
- Right-align: render the **last `width`** values; left-pad with spaces if fewer.
- Scale each value to `0..=8` by `max` of the window (guard `max == 0` → all spaces, avoids div-by-zero). Use ` (v * 8 + max/2) / max ` for rounding.
- Unit test this in isolation (see §6).

---

## 3. Rendering — slot a Net panel into `draw()`

In `src/logpane.rs::draw()`, change the vertical layout to insert a network row
**between the search bar and the facets|list** (only when shown — see §4):

```rust
let net = agent_net_stats(&pane.index, 16);
let net_h = if pane.show_net && !net.is_empty() {
    (net.len() as u16 + 2).min(8)   // 1 row/agent + borders, capped
} else { 0 };

let chunks = Layout::vertical([
    Constraint::Length(3),       // search bar
    Constraint::Length(net_h),   // ← NEW: per-agent network panel (0 when hidden)
    Constraint::Min(5),          // facets | list
    Constraint::Length(7),       // detail
    Constraint::Length(1),       // help
]).split(area);
```
Then shift the existing `chunks[1..]` indices down by one (search=0, **net=1**, mid=2,
detail=3, help=4). A `Constraint::Length(0)` row renders nothing, so the panel cleanly
disappears when toggled off or no agents are present.

**Row format** (use the existing `human_bytes`):
```
n8-rosy-robin    [▃▅█▆▃▂▂ ]   ↓ 1.4MB/s   ↑ 120KB/s
n8-glint-otter   [  ▂▃▃▂   ]   ↓  45KB/s   ↑   2KB/s
```
- Left-pad `agent_id` to a fixed width (e.g. 16, truncate longer).
- Sparkline in `[..]`, width 8–10.
- `↓ {human_bytes(rx)}/s   ↑ {human_bytes(tx)}/s`.
- **Stale agents** (no sample in >15 s vs the newest event's ts in the index): dim the row
  (`Color::DarkGray`) so dead agents are visibly idle rather than showing a frozen rate.
- Wrap in a `Block::default().borders(ALL).title(" NETWORK ")`.

---

## 4. Toggle + state

Add to `LogPane`:
```rust
show_net: bool,   // default true
```
- Init `true` in `LogPane::new`.
- In `run()`'s key handler, bind a key to flip it — **`n`** (for "network"), guarded so it
  only fires when `focus != Focus::Search` (otherwise `n` types into the search box). Mirror
  how the existing `Tab`/kind keys are gated.
- Add `n net` to the help bar string (the `Length(1)` help row).

---

## 5. File-by-file

| File | Change |
|---|---|
| `src/logpane.rs` | Add `AgentNet`, `agent_net_stats()`, `sparkline()`; `show_net` field + `n` toggle; new layout row + render block in `draw()`; help-bar text. |
| `src/event_index.rs` | **No change** — `agent_id` + `raw` already exposed. |
| `src/monitor.rs` / `collectors.rs` | **No change** — already emitting tagged metrics. |

No new dependencies. All `std` + `ratatui` already in use.

---

## 6. Tests (`#[cfg(test)]` in `logpane.rs`)

1. **`sparkline_scales_and_right_aligns`** — `sparkline(&[0,4,8], 5)` → `"   ▁█"` (pad left, max-scaled); `sparkline(&[], 4)` → `"    "`; all-zero → all spaces (no panic / no div-by-zero).
2. **`agent_net_stats_groups_newest_wins`** — ingest metric events for two agents across several ts; assert two `AgentNet`, each `rx_bps`/`tx_bps` from the newest sample, `history` oldest→newest, length ≤ window.
3. **`untagged_metrics_are_skipped`** — a metric with no `agent_id` does not produce an `AgentNet`.
4. **`draws_with_net_panel`** — `TestBackend`, `show_net=true`, ingest a tagged metric, `draw()`; assert the buffer contains the agent name and `NETWORK`.

---

## 7. Acceptance criteria

- Launch a couple of agents (`n8 --danger` → new sessions on the v0.18.4 image), `e` into LOGPANE.
- The **NETWORK** panel lists each running agent with live `↓/↑` rates that move, and a sparkline that fills over ~16 ticks (~80 s).
- `n` toggles the panel; the rest of the UI reflows with no layout glitch.
- Stale/stopped agents dim out instead of showing a frozen rate.
- `cargo test --lib logpane` green.

---

## 8. Out of scope (future)

- Per-agent **kind facet** in the sidebar (filter the event list by agent) — separate, also enabled by `agent_id`.
- CPU/mem sparklines (same mechanism, different field).
- Browser/HTML view of the stream.
