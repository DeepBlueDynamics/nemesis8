# Archived docs

Superseded or completed plans/specs, kept for history. Nothing here describes
current behavior — see the live docs in `docs/` instead.

| File | Why archived | Live replacement |
|---|---|---|
| `PROMPT.md` | The agent system prompt is now **embedded** in the binary (`prompts/BASE.md` via `include_str!`) + a per-provider `persona` line, composed in `config::compose_system_prompt`. It is no longer read from the workspace or baked into the image. | `prompts/BASE.md` + `[provider.system_prompt].persona` in each `providers/*.toml` |
| `provider-abstraction.md` | Completed. The "make providers fully data-driven, zero provider names in `src/`" plan landed — `build.rs` generates the registry, login/preflight/auth-sync/api-keys/aliases/emoji are all TOML-driven. | `docs/adding-a-provider.md` |
| `ui-redesign-brief.md` | The design brief that was handed to the design pass; it produced the v3 design and the control room shipped. | `docs/design/n8-tui-v3-design.md` |
| `NEMESIS_SETUP_LOG.md` | One-off 2026-03 setup log; references the retired `.codex-container.toml` naming (now `.nemesis8.toml`) and the old data home. | — (historical) |
