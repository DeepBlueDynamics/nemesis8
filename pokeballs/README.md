# Community pokeballs

A pokeball is a sealed Docker container with a captured agent workload —
a `pokeball.yaml` spec describing the runtime, build steps, allowed tools,
resource limits, and provider. See `src/pokeball/spec.rs` for the full
schema and `n8 pokeball capture --help` for the scanner that produces one.

This directory is the **community catalog**. Each entry here is a complete
pokeball that anyone can deploy with `n8 pokeball deploy <name>` or by
asking the orchestrator agent: *"deploy grub-crawler"*.

## Directory layout

```
pokeballs/
  README.md          this file
  index.json         metadata: descriptions, stars, authors, versions
  <name>.yaml        a complete PokeballSpec (one per pokeball)
```

`index.json` maps a pokeball name to display metadata:

```json
{
  "grub-crawler": {
    "description": "Headless web crawler with structured extraction.",
    "stars": 4200,
    "author": "deepbluedynamics",
    "version": "0.3.1"
  }
}
```

A pokeball is valid here only if `<name>.yaml` exists alongside its
`index.json` entry. Entries without a matching yaml are ignored.

## Contributing

1. Capture your project locally: `n8 pokeball capture <path>` — this
   produces a `pokeball.yaml` after scanning the runtime, build steps,
   and dependencies.
2. Tighten the spec: review allowed tools, resource limits, network
   policy, and the system prompt. Community pokeballs should default
   to `security.network: deny` unless they genuinely need network.
3. Copy the spec to `pokeballs/<name>.yaml`.
4. Add an entry to `index.json` with a one-line description and your
   author handle (stars start at 0; the registry will mirror them
   later).
5. Open a PR. CI builds the image to verify the spec is valid.

## Distinction from tools

A **pokeball** is a containerized agent — it runs. A **tool** is an MCP
server attached to a container's `mcp_tools` list. Don't put MCP tools
here; those live in `MCP/` and are referenced by name (or URL) from a
workspace's `.nemesis8.toml`.
