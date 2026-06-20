# Plan: config resolution + MCP tool model

## Why this exists

The config + MCP-enablement system grew confusing and does surprising things:

- **`Config::find` walks up the whole tree** and grabs the stray `~/.nemesis8.toml`
  from *any* dir under home ("home-root leak"). Running archive/reset/tool-edits in
  `~/Code/foo` operated on the **home** file and wrote the backup into `foo/`. Wrong.
- **Empty `mcp_tools` → "discover all"** — removing your list silently turned *every*
  tool on.
- **`n8 init` writes a hardcoded tool list** baked into `config.rs` (not data-driven).
- **Tools are copied into a shared volume drawer** (`/opt/nemesis8/mcp`) — redundant
  with the image, never pruned (the ghost-server source), and **shared across every
  session**, so two agents in two panes/dirs clobber each other's config.

This plan defines the model we want and how the container side actually works.

## The model

### 1. Two config layers — home base + local override

```
~/.nemesis8.toml          ← personal/global base (your default tools, env, etc.)
<cwd>/.nemesis8.toml      ← per-workspace override
```

- **Effective config (for RUNNING a session) = home ⊕ local, local wins.** Local
  `mcp_tools` (if present) **replace** home's; other keys merge key-by-key.
- **Fallbacks:** local-only → local; home-only → home; neither → binaries-only.
- **No directory walk-up.** Exactly these two layers — not "nearest ancestor with a
  `.nemesis8.toml`". (`Config::find`'s walk is retired; the home-leak patch is just a
  stopgap until this lands.)

### 2. Writes are CWD-only

`init`, `archive & reset`, and the tools picker operate **only on `<cwd>/.nemesis8.toml`** —
never home, never an ancestor. It just writes a TOML file to the directory you're in.

- To edit the **home** config, you `cd ~` and run init/reset there. Same code path; the
  cwd just happens to be home.
- It never reaches outside the directory you're in. Period.

### 3. Init is a data-driven checklist (no hardcoded list)

`n8 init` (and control-room → Config → Init) shows a **checklist of the tools that
actually exist in the image** (`/opt/mcp-source`), exactly like the tools picker —
"these will be installed," toggle any off, **wipe = uncheck all**. It writes the
selection to `<cwd>/.nemesis8.toml`. No tool list lives in Rust; the list comes from the
image.

**Pre-check = your current effective selection (decided).** In a dir with no
`.nemesis8.toml`, init seeds the checklist from the tools **currently enabled** — i.e.
the effective config the TUI is showing (home ⊕ anything active), not a hardcoded
default, not "all", not "none". So `n8 init` in a fresh directory means "start this
workspace with the tools I'm already using," which you then trim/extend before writing.
The always-on binaries are shown but not toggleable.

### 3a. Tool choosers stage changes and confirm — never blind-write

Today the tools picker calls `write_mcp_tools` on **every** space-toggle — each keystroke
silently rewrites `.nemesis8.toml`. That's wrong: toggling should edit an **in-memory**
selection, and **writing is an explicit, confirmed action**.

- Toggling marks the selection **dirty** (a `*`/"unsaved" indicator), but writes nothing.
- A **Save** key (e.g. `s`/`Enter`) writes; closing with unsaved changes **prompts**
  "Save changes to `<file>`? (y/n/cancel)" — `n` discards, `cancel` returns to editing.
- Same for the init checklist: pick tools, then **confirm** to write; esc discards.
- Applies to both the tools picker and Config → Init/Reset. No config file is ever
  mutated as a side effect of navigating the UI.

### 4. MCP tools: image-only, host-resolved, per-session

This is the answer to "how does it get into the container, and how do concurrent
sessions not collide."

- **Tool code lives ONLY in the image** (`/opt/mcp-source`), read-only, shared. Agents
  run the `.py` from there. **Stop copying tools into the volume drawer** — that
  eliminates the redundant copies, the never-pruned junk, and the ghost servers at the
  root. "Available tools" = `ls /opt/mcp-source` (what the init checklist + picker read).
- **The HOST resolves the effective tool list** (home ⊕ local) and passes it into the
  container **per-session** as an env var (e.g. `NEMESIS8_MCP_TOOLS=calculate.py,...`).
  The container does **not** read `.nemesis8.toml` — the host already merged it and hands
  over the final list. (So the answer to "is it baked or written at start?": the *tools*
  are baked into the image; the *selection* is passed per-session.)
- **`entry` generates the provider config** (e.g. agy `mcp_config.json`) from that env
  list, registering each tool as `command=/opt/mcp-venv/bin/python3 /opt/mcp-source/<t>`,
  plus the always-on binaries (`nuts-files`, `shivvr`, `ask`, `nemesis8`).

### 5. Concurrency — per-session config, shared persistent state

The clobbering happens because the generated provider config lives in the **shared**
volume (`/opt/nemesis8/.gemini/antigravity-cli/mcp_config.json`). Two concurrent
sessions overwrite it.

**Fix:** split the provider's config dir into *generated* vs *persistent*:

- **Generated, per-session** (each container writes its own, NOT shared): the MCP config
  (`mcp_config.json`), the schema cache (`mcp/`). Write these to a per-container path
  (the container's own filesystem or a per-container subdir), so panes/dirs never collide.
- **Persistent, shared** (bind-mounted from the volume): conversations/sessions, auth
  tokens, history — the stuff you want to survive and resume across containers.

So: your selected tools are isolated per session; your conversations + logins persist
and resume. A session in another pane/dir can have a totally different tool set with no
interference.

> **Interim option (smaller change):** keep the config in the volume but have `entry`
> write it fresh at startup right before launching the agent (which reads it once). The
> clobber window shrinks to milliseconds and sessions usually share the home base anyway
> — last-writer-wins is harmless when the lists match. The per-session-dir split is the
> robust version; the write-at-startup is the cheap 80%.

## What's already in flight (this session)

- `Config::find` home-leak patched (stopgap → replaced by §1 two-layer resolution).
- discover-all removal in progress (§ empty = binaries-only).
- init template de-hardcoded (§3 makes it a real checklist).

## Open decisions (need your call)

All decided:
1. **Merge** — local `mcp_tools` **replace** home's; other keys overlay.
2. **Concurrency** — **(a) per-session config dir** (true isolation; conversations/auth
   shared+mounted).
3. **Init default** — seed from the current effective/enabled selection (what the TUI
   shows), user trims/extends, then confirms to write.

## Status (2026-06-20)

- **Phase 1 — DONE** (v0.15.8): discover-all removed (install copies only named `.py`;
  config-gen enables exactly what's listed; empty = binaries only).
- **Phase 2 — DONE** (v0.15.8): `Config::load_layered` (home ⊕ local, local wins) feeds
  the existing `NEMESIS8_CONFIG_JSON` handoff; writes are cwd-only (no walk-up / home
  stray). *(2b "drop the volume drawer entirely" deferred — Phase 1 already removed the
  junk-drawer harm; running tools straight from `/opt/mcp-source` is cleanup, not a fix.)*
- **Phase 3a — DONE** (v0.15.9): tools picker stages + confirms before writing.
- **Phase 3b — DONE** (v0.15.10): init seeds from the current selection; no hardcoded list.
- **Phase 4 — DEFERRED (needs live testing).** Per-session provider config dir +
  shared/mounted persistent state is mount-restructuring: if the split is wrong, sessions
  fail to launch or lose their conversation history. That can't be verified without
  running real agy/codex sessions and a two-pane concurrency test — which needs you at
  the keyboard. Implementing it blind while you're away risks breaking every session, so
  it waits for your return + a test pass. Design is fully specified in §5 above.

## Rough implementation order

1. `Config` gains explicit two-layer load (`load_effective(home, cwd)`); retire `find`
   for writes (cwd-only) and reads (home ⊕ cwd). Remove discover-all.
2. Host passes `NEMESIS8_MCP_TOOLS` (resolved list) into the container; `entry` reads it
   instead of the workspace `.nemesis8.toml`; tools register against `/opt/mcp-source`;
   drop `install_mcp_servers`' volume copy/sync entirely.
3. Init/reset → interactive checklist from `/opt/mcp-source`, writes cwd config.
4. Concurrency split (per chosen option in decision #2).
