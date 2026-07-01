# Spec: On-box Session Analytics

Turn the agent transcripts nemesis8 already produces into a local analytics
surface — tool/MCP/skill/subagent usage, cost, files touched, git activity,
faceted drill-down, full-text search, and a per-session notebook — **computed
and stored entirely on the machine.** Nothing is uploaded, ever.

---

## 1. Why this exists (read this part)

Your agent transcripts are the single most sensitive artifact you produce. Open
one up: it's every file you read, every command you ran, the contents of your
private repos, API keys that got echoed into a `Bash` result, your architecture
decisions, your unreleased product, your half-finished ideas, the exact shape of
how you think. It is, quite literally, a high-fidelity recording of your work and
your judgment.

So naturally, the going move in the **competitive landscape** is: ship all of it
— wholesale, continuously, automatically — to someone else's servers, so you can
look at a bar chart.

Think about how stupid that is. You install a daemon whose entire job is to tail
your most private files and POST them to a stranger's cloud. They land in a
columnar store you don't control, get parsed by models you don't run, and sit
there as a permanent, searchable copy of your IP on infrastructure you can't
audit — and the thing you get back is *counts*. "You called Bash 842 times."
Cool. You uploaded your brain's exhaust to a third party for the privilege of a
histogram you could have computed in an afternoon. And the moment you want it
gone, you discover there's no delete button — only "wipe the whole account," if
you're lucky and know the founder.

Here's the thing the whole category is built on pretending you don't notice: **the
data is already on your disk.** The transcripts are sitting in `~/.claude/projects`
right now. Every metric on every one of those dashboards is a `parse → classify →
index` away. The cloud isn't doing anything magic with the bytes — it's doing
arithmetic you can do locally, plus the part where they keep a copy of everything
you've ever built.

There is no reason — none — for an agent transcript to leave the machine that
made it. nemesis8 does the arithmetic on-box. Your prompts stay yours. The disk
they were written to is the only disk they ever touch.

That's the whole principle: **observability without exfiltration.**

---

## 2. What it does

Same analytics surface, zero upload:

- **Tool-call tracking** — every `tool_use` event classified by name: built-in
  tools (Read/Bash/Edit/Grep/Write/…), MCP servers (`mcp__<server>__*`),
  subagents (`Task` / `subagents/*`), and skills.
- **Aggregates** — calls + token sums per tool / MCP / skill / subagent, per
  session, per project (working dir), and org-wide.
- **Facet drill-down** — pick any tool → the list of every session that used it
  (title, model, turns, tokens, cost, recency).
- **Cost** — `$` per turn / session / project from a local model-rate table.
- **Files & git** — files touched (`Edit`/`Write`), git activity (`Bash git …`).
- **Search** — full-text over turn content via **lume** (local retrieval).
- **Notebook** — a per-session, turn-by-turn timeline.
- **Insights** (later, local) — an artifact-grounded summary/keyframes pass run
  by a **local** model; see §6.

## 3. Data source

`~/.claude/projects/<urlenc-cwd>/<session>.jsonl` (+ sibling
`<session>/subagents/agent-*.jsonl`). Each line is one record: `type`
user/assistant, `message.content[]` (text + `tool_use`/`tool_result`),
`message.usage` (token counts), `message.model`, timestamps, `uuid`. These are
the files an upload daemon would tail — we read them in place.

## 4. Design (reuse what's already in the repo)

- **`src/transcript.rs`** *(new)* — parser + classifier. Yields `ToolCall {
  session, agent_id, tool, category, tokens_in, tokens_out, ts, model }` and
  `SessionMeta { id, cwd, model, machine, turns, tokens_in/out, cost_usd,
  started, updated }`. Classifier: `mcp__X__*`→MCP(X), `Task`/subagent files→
  Subagent, skills→Skill, else→Builtin. Tail-read big files like
  `event_index::read_tail`.
- **`src/event_index.rs`** *(reuse)* — feed `ToolCall`s as JSON values; the
  existing `EventQuery` facets/time-window/free-text + `facets()` already give
  the dashboard counts and the facet drill-down.
- **lume** (`third_party/lume`, already a dep) — index turn text for search. This
  is the local equivalent of the columnar/vector store the hosted tools rent.
- **`src/logpane.rs`** *(reuse pattern)* — render dashboard / facet table /
  notebook as a TUI mode.
- **`src/gateway.rs`** *(optional, later)* — a browser view = one axum `Html<>`
  route + a JSON endpoint over the local index. Still on-box (localhost).

## 4a. Aggregation container (the data plane)

The data is scattered: host `~/.claude/projects`, each agent's data home
(`.codex/sessions`, `.gemini/antigravity-cli/conversations`, `.grok/sessions`,
`opencode.db`, …), and the monitor `events.jsonl` — across processes and, in a
fleet, across machines. We need **one place to shove it all** so analytics query
a single always-current store instead of re-parsing ~1.1 GB on every CLI open.

A **service-class container** (`services/aggregator.toml`) runs a daemon
(`nemesis8-aggregator`, mirroring `nemesis8-monitor`). It is the single on-box
sink:

- **Sources** (read-only mounts): host `~/.claude/projects`; the per-provider
  session dirs under the data home; the monitor `events.jsonl`.
- **Ingest:** the transcript parser/classifier (§2) over each source →
  normalized `ToolCall`/`SessionMeta`; telemetry folded in.
- **Store:** a **lume**-backed unified index on a persistent volume
  (`/var/lib/n8-aggregator`), built once and updated **incrementally**.
- **Live:** FS-notify on the sources → incremental re-ingest as agents run; no
  full re-parse.
- **Serve:** a **localhost** query surface (unix socket / fixed-port HTTP) the
  host CLI, LOGPANE, and the optional web view read. Read-only, on-box, **no
  external network — by construction.**
- **Fleet:** one aggregator per machine; a multi-host setup federates them via
  the gateway over LAN — still never a third-party cloud.

Why a container and not just the host-side one-shot in §2: **persistence +
incremental indexing** (don't re-parse 1.1 GB per open), **isolation** (heavy
ingest off the host CLI), and **one sink that scales to the whole fleet**.
§2–§6 are the parse/classify/view; this is the warehouse they read.

It's the local, owned answer to the thing the hosted tools do with a cloud
bucket — same aggregation, on a disk you control.

## 5. Phasing

0. **Aggregation container** (`services/aggregator.toml` + `nemesis8-aggregator`)
   — the data plane §4a. Can start as a thin wrapper around the §2 parser
   writing to a lume volume; the analytics run host-side first, then move to
   querying the container.
1. **Parser + classifier** (`transcript.rs`) — the core primitive.
2. **Index + facets + search** — `event_index` rollups + lume.
3. **Cost + files/git** — cheap, high-value.
4. **Analytics view** — LOGPANE dashboard / facet / notebook.
5. **Insight pass** — §6.

Phases 1–4 are the bulk and are mostly assembly of existing pieces; phase 0 is
how it scales past one machine and stays live.

## 6. Insights — and the one real edge

A `trait Insighter { fn analyze(&self, s: &ParsedSession) -> Insights }` with a
no-op default (`title` = first user line) so the UI ships immediately. The real
implementation runs a **local** model (Ollama) — but with a difference that
matters:

A transcript-only summary describes the *motion* of a session ("read specs,
renamed the repo, dispatched three panes") and never the *substance* — it can't
tell you **what the thing being built actually is**, because it only ever saw the
conversation about it.

We have what a transcript-only view structurally cannot: **the artifacts.** The
repo, the specs, the README, the code are right there. So the insight pass
**grounds on the artifacts via lume**, not just the chat — it reads the actual
spec doc and says *"this is a payment-metered LLM gateway,"* because it read the
gateway, not a conversation that mentioned one. Transcript = the trail; artifacts
= the meaning. Grounding on artifacts is the part a cloud that only ingests
transcripts can't reach — and it's local-only by construction.

## 7. Acceptance

- Point at `~/.claude/projects` → dashboard shows per-tool/MCP/skill/subagent
  counts + token sums; facet → sessions per tool with model/turns/tokens/$;
  notebook → one session turn-by-turn; lume search returns the right sessions.
- **Zero network calls.** Verify nothing leaves the box.

## 8. Out of scope (for now)

Multi-user/team rollups, browser-UI polish, the digest/highlight passes.
