# nemesis8 v0.26.6 — First run, explained 🕰️

Two fixes for the first `n8 build` on a machine, one of them from a new contributor. To get it: `n8 update` (on Windows, run the installer line it prints), then `n8 build`.

## First-run setup progress and the cold-start panic (#131, @schofield)

- A cold install could panic before the build began: the build-context download ran a blocking HTTP client inside the async runtime. The context is now resolved once, off the async worker, and reused for the whole build.
- Setup stages announce themselves ("Downloading v0.26.6 build files…", "Extracting…", "Checking prebuilt container binaries…") with an elapsed-time heartbeat every 10 s, so a multi-minute first run is never silent.
- The build screen shows the latest real output instead of collapsing repeated lines, says how long it has been since the last line, labels the step meter as approximate, recognises BuildKit's image export as a distinct finalizing phase, distinguishes success from failure, and prints the full log path when the UI exits.

## Build from the context that matches the binary (#133)

- `n8 build` run inside a nemesis8 checkout used that checkout as the build context whatever its version, so a stale branch could bake old provider files into the image (the way Claude's MCP servers went missing on one machine). The checkout is now skipped, with a message, when its version differs from the binary's; `NEMESIS8_PROJECT_DIR` still forces a specific checkout.

Coming from further back? [v0.26.5](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.26.5) brought scheduled-run options and Ferricula identity discovery.
