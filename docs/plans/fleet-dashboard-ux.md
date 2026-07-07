# Fleet dashboard UX — the actual design (v2)

The v1 page was data-first with no layout design; owner review: events pane
must be the star (Splunk/Loggly model), fleet full-width, drill-in everywhere,
streaming not polling, net as a column/table not a half-page panel.

## Layout

```
┌──────────────────────────────────────────────────────────────────┐
│ FLEET (full width, compact rows, scrolls past ~8)                │
│ agent · provider · model · workspace · state · cpu% · mem · ↓/↑  │
│ (↓/↑ cells carry inline micro-sparklines; click row → DETAIL)    │
├──────────────────────────────────────────────────────────────────┤
│ EVENTS — full remaining height, the main surface                 │
│ [search box________________] [kind ▾] [agent ▾] [⏸ live]        │
│ ts · agent · kind · summary          ← newest-first, tail-follow │
│ (row click → expand raw JSON inline)                             │
│ scrolls INSIDE the pane; page never scrolls                      │
└──────────────────────────────────────────────────────────────────┘
```

- **Events pane owns the height.** Fleet table is a fixed compact band.
- **Search/filters are the EXISTING query surface** — the search box and
  dropdowns bind 1:1 to `agent_events {q, kinds, agent_id, since}` /
  `/fleet/data.json` params. NO new search engine, no client-side grep over
  everything; the index already does this.
- **Net panel is GONE as a standalone block** — throughput lives as compact
  ↓/↑ cells + micro-sparklines in the fleet row, and as a real table+chart in
  the agent detail view.
- **Drill-in**: clicking a fleet row opens a DETAIL view (same page, no
  routing framework): that agent's cpu/mem/net history charts (agent_net),
  its recent events pre-filtered, its labels (provider/model/workspace/
  uptime), and a "kill" affordance can come later. Esc/breadcrumb back.
- Event row click expands the raw event JSON inline (copyable).

## Streaming (SSE, not polling)

- New gateway endpoint: `GET /fleet/events/stream` — SSE. Implementation:
  the gateway tails the event index (the refresh already knows byte offsets);
  a small async task publishes newly-ingested events to a
  `tokio::sync::broadcast` channel; the SSE handler subscribes and forwards.
  Include a `retry:` hint; client falls back to 2s polling if EventSource
  errors (banner says "polling fallback").
- Fleet band refresh stays on light polling (5s) — container joins are a
  docker call, not worth streaming.
- Live toggle (⏸) pauses tail-follow so scrollback reading isn't yanked.

## Rules carried forward

- ONE self-contained web/fleet.html — inline CSS/vanilla JS, no CDNs.
- NEVER mock data; unreachable → banner; empty → "no tagged agents yet".
- Absolute fetch/EventSource URLs (/fleet/data.json, /fleet/events/stream).
- Dark, dense, terminal aesthetic; no horizontal page scroll.

## Acceptance

- Events fill the viewport height and scroll internally with live tail;
  typing in search narrows via the SERVER query (watch the network tab —
  requests carry q/kind/agent params).
- New events appear without refresh (SSE frames visible in devtools).
- Fleet is full-width; clicking a row opens detail with real per-agent
  charts; Esc returns.
- Net standalone panel no longer exists; no layout element wider than its
  content needs.
