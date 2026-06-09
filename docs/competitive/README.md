# Competitive notes

Working notes on tools adjacent to nemesis8 — what they do well, what's worth
borrowing, and where n8 deliberately diverges. These are for our own planning;
they're not marketing.

| Tool | One-liner | Stack | License | Closeness to n8 |
|------|-----------|-------|---------|-----------------|
| [awman](awman.md) | Agent Workflow Manager: issue→PR, container + worktree isolated, workflow engine | Rust | Apache-2.0 | **Very close** (same genus) |
| [atria](atria.md) | TUI that discovers/monitors agents in your existing terminals | Go | MIT | Adjacent (observer, not owner) |

## Attribution plan

We routinely read other projects for ideas. The rule:

1. **Ideas are free; code is not.** Concepts (a workflow engine, worktree
   isolation, a "needs input" status) aren't copyrightable — reimplementing them
   from our own understanding carries no legal obligation. That's the default
   path: **reimplement, don't copy.**

2. **Credit prior art as a matter of practice.** When a feature lands that was
   clearly inspired by one of these tools, add a short credit in:
   - the PR description, and
   - the CHANGELOG / release notes for that version, and
   - optionally a code comment near the feature
   e.g. *"worktree-per-agent isolation, inspired by awman (Apache-2.0)."*
   Update the relevant note in this folder to record what we took.

3. **If we ever copy or adapt actual source code**, the license terms become
   mandatory and must be honored before merge:
   - **awman — Apache-2.0:** preserve the `LICENSE`, include any `NOTICE`
     content, and state significant changes. Vendor these under a `THIRD_PARTY/`
     or top-level `NOTICE` file.
   - **atria — MIT:** preserve the copyright line + permission notice alongside
     the adapted code.
   Flag any such copy explicitly in review — it should be a conscious decision,
   not an accident.

4. **This folder is the ledger.** Each note carries an "Attribution" block
   (repo, author, license, version examined, date). When we act on an idea, link
   the n8 issue/PR back from the note so provenance stays traceable.

_Examined: 2026-06-08._
