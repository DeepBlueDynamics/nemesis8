# nemesis8 v0.27.3 — Codex gets its own identity 🪪

Mostly Codex and Hyperia plumbing: an agent in a container now keeps its own Hyperia identity, works in danger mode, and stays visible on the mail bus even after your machine sleeps. Plus the control room's New menu stops sprawling. `n8 update` on each machine, then `n8 build` (the Codex and liveness fixes live in the container).

## Codex: own Hyperia identity + tools work in danger mode (#152)

Two bugs in the generated `~/.codex/config.toml`:

- **Danger mode rejected every Hyperia tool.** With approvals bypassed, Codex's policy is "never", and Hyperia's annotation-less tools got rejected ("requires approval, but approval policy is never") instead of passing through. The entry now sets `default_tools_approval_mode = "approve"`; Hyperia enforces its own consent server-side.
- **Containers shared one identity.** Every container mounts the same HOME, so they shared one config, and Codex baked the Hyperia bearer in as a literal — the last container to start stole the others' identity (mail, bindings, consent landed on the wrong agent). The entry now writes `bearer_token_env_var` so each container reads its own token from its env; the shared file holds none.

## Mail notices survive a sleep (#153)

After the PC woke from sleep, the sidecar cleared each pane's shell-integration record, and with an agent running there's no shell prompt to re-send it — so Hyperia stopped treating the pane as an agent and went quiet (no mail notices, `pane_send` refused). n8's liveness now tags itself `agent:"n8"` and keeps pinging while idle, so Hyperia keeps seeing the agent. Pairs with a Hyperia-side fix.

## Control room: one New entry, host is a pulldown (#150)

"New session on <host>" used to add one Session-menu item per remote — unwieldy with several. Host is now a pulldown inside the New modal (local, then each remote), and the Session menu is back to two items.

## Under the hood

- Release notes now show only the current version, not every past one (#140).

Coming from further back? [v0.27.2](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.27.2) quieted container startup chatter over agent TUIs.
# nemesis8 v0.27.2 — Quiet containers 🤫

A remote or local agent session no longer gets the container's own startup chatter dumped into its terminal UI. If you run Grok, Claude, Codex, or antigravity in an n8 pane, this is the one you want. `n8 update` on every machine, then `n8 build` so the container picks it up (the fix lives in the in-container entry).

## No more entry diagnostics over the agent's TUI (#141)

The container entry prints `[nemesis8-entry] …` lines as it sets up. While an interactive provider's UI owns the terminal, a line emitted after launch — the session poller reporting to the control plane, a tunnel-client thread — landed in the middle of the agent's screen, and even dropped into its input box ("provider session reported to control plane" mid-prompt in Grok). v0.27.1 silenced that one line; this silences the whole class.

Every entry diagnostic now goes through one helper. While the provider TUI is up, the lines go only to a log file (`/opt/nemesis8/entry.log`); boot output still prints before the TUI opens, the exit menu still prints after it closes, and a headless run's `docker logs` are unchanged. So the agent's screen stays clean and nothing you type collides with a stray log line.

Coming from further back? [v0.27.1](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.27.1) made remote sessions render and stay alive in a Windows pane.
# nemesis8 v0.27.1 — Remote sessions that actually hold 🧵

0.27.0 put another machine's agents in your control room. Using them from a Windows pane was rough: a remote TUI rendered with shifted rows and stray characters, then froze and dropped. This release makes a streamed remote session behave like a local one. Point-release on top of 0.27.0, so `n8 update` everywhere, then `n8 build` on each machine (the container entry changed too).

## Remote attach renders and stays alive on Windows (#138)

A remote attach into a ConPTY-backed pane (a Hyperia pane) hit four separate client-side bugs, worst for inline renderers like antigravity:

- **It froze and disconnected.** The terminal write sat inside the async read loop; when the pane's console stalled, the loop stopped answering pings and the gateway closed the session. Output now runs on its own thread, so the reader keeps flowing and the picture catches up when the console recovers.
- **Rows shifted and duplicated.** The console wraps the last column where a real terminal waits; a full-width rule gained a line each time. The client now reports one column fewer on Windows (`N8_PTY_COLS_SLACK` overrides).
- **Stale header lines stayed on screen.** The screen is cleared right after switching to the alternate buffer.
- **Newlines** on the output console now match `docker run -it`.

Diagnosed live with the Hyperia dev session tapping the pane's raw stream; a remote antigravity session then stayed interactive across many prompts with clean rendering.

## Quieter container entry (#137)

The entry printed "provider session reported to control plane" onto the TTY while the provider's UI owned it, splitting an inline renderer's prompt. The session report is silent now; the gateway still logs it on its side.

## Developer fix (#136)

A test served its fixture from `0.0.0.0`, tripping the firewall prompt on every `cargo test`; it binds loopback now. `docs/HANDOFF.md` gains an inventory of the three listeners that are meant to bind all interfaces (the gateway, the tunnel acceptor, a provider inside its container) so the list stays short.

Coming from further back? [v0.27.0](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.27.0) is the one that brought remote hosts into the control room.

# nemesis8 v0.27.0 — Two boxes, one control room 🎉🖥️🖥️

Party release. For the first time, `n8` on one machine runs agents on another and brings their terminals home. Point it at the gateway on your house server, and that server's containers and sessions sit in your control room next to the local ones. Pick **New session on nemesis**, and a fresh Claude or Codex boots over there with its TUI streaming into the pane in front of you. Same keys, same exit menu, same `n8 attach` later. The fleet just stopped being one machine.

To get it: `n8 update` on every machine (on Windows, run the installer line it prints). Restart `n8 serve` on the box you want to reach, then on your desk:

```
ssh nemesis cat ~/.nemesis8/gateway-token | n8 remotes add nemesis http://nemesis.local:9801 --token-stdin
n8
```

## Remote hosts in the control room (#135) 🛰️

- **`[[remotes]]`** in the global config, managed by `n8 remotes list|add|rm`. The token lives in the OS keychain under `NEMESIS8_TOKEN_<NAME>`, never in the file (unless there is no keychain, and then it says so).
- **HOST column** on the Running and Sessions tabs once a remote exists, local rows first, then each host. `/` filters by host too.
- **Badges** in the top bar: `● nemesis v0.27.0` when it answers, `○ nemesis unreachable` when it does not, `◌` while polling.
- **Session ▸ New session on \<host\>** opens the usual New modal and starts an interactive agent on that machine, then attaches your terminal to it. Enter on a remote session resumes it there. `a` attaches to a remote agent, `k` kills it through the gateway.
- **`n8 attach nemesis/n8-merry-lemur`** and `n8 shell nemesis/…` route by the host prefix, the id form `n8 agents list` prints for a remote. An unknown prefix is a plain error instead of Docker's "No such container".
- **`n8 interactive --remote URL`** starts an interactive agent on that gateway and attaches (it used to be refused).

## Gateway: interactive spawn 🚀

- `POST /agents/spawn` takes `interactive: true` (and `session_id` to resume): a TTY container running the provider's own UI, answered at once with its agent id. No prompt, no timeout, no slot taken from the one-shot run limit. Attach with `GET /agents/{id}/pty?mode=attach`.
- When an attach ends on a container that has exited, the gateway honours the exit menu's answer: Remove deletes the container, Stop keeps it for a later attach.

## Fixes that came along 🧹

- **Phantom "running" agents.** The registry counted every container in `docker ps -a` as alive, so an agent whose container had stopped stayed "running" for ever. Ten of them on one real gateway. Only a running container is live now; a stopped one marks its record exited.
- **Arrow keys over a remote attach on Windows.** The attach client read stdin in a way that never saw arrow or function keys, so a remote TUI could not be navigated (Enter and letters worked). The console is now in VT input mode for the attach.

Spec: `docs/specs/remote-hosts.md`. Coming from further back? [v0.26.6](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.26.6) explained the first run and fixed the cold-start panic.

# nemesis8 v0.26.6 — First run, explained 🕰️

Two fixes for the first `n8 build` on a machine, one of them from a new contributor. To get it: `n8 update` (on Windows, run the installer line it prints), then `n8 build`.

## First-run setup progress and the cold-start panic (#131, @schofield)

- A cold install could panic before the build began: the build-context download ran a blocking HTTP client inside the async runtime. The context is now resolved once, off the async worker, and reused for the whole build.
- Setup stages announce themselves ("Downloading v0.26.6 build files…", "Extracting…", "Checking prebuilt container binaries…") with an elapsed-time heartbeat every 10 s, so a multi-minute first run is never silent.
- The build screen shows the latest real output instead of collapsing repeated lines, says how long it has been since the last line, labels the step meter as approximate, recognises BuildKit's image export as a distinct finalizing phase, distinguishes success from failure, and prints the full log path when the UI exits.

## Build from the context that matches the binary (#133)

- `n8 build` run inside a nemesis8 checkout used that checkout as the build context whatever its version, so a stale branch could bake old provider files into the image (the way Claude's MCP servers went missing on one machine). The checkout is now skipped, with a message, when its version differs from the binary's; `NEMESIS8_PROJECT_DIR` still forces a specific checkout.

Coming from further back? [v0.26.5](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.26.5) brought scheduled-run options and Ferricula identity discovery.
