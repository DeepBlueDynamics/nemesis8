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
