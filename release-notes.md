# nemesis8 v0.26.2 — Show your work ✏️

Two changes to the telemetry n8 emits about a running agent. Edits made through the file tools now report which file, which lines, and how much changed, so a pane can show "editing tests/test_candidates.py, lines 40 to 61, +12 −3". And the filesystem watcher stops reporting reads, which were 93 % of all events and said nothing. To get it: `n8 update`, then `n8 build` for the container side, then recreate containers.

## Edit telemetry

Every agent edits through the `nuts-files` server (`nuts_edit`, `nuts_replace`, `nuts_write`), which is the one place that knows what changed inside a file. It now writes one `edit` event per successful write into the shared events file the container monitor already uses:

```
kind=edit  tool=nuts_replace  path=/workspace/format/tests/test_candidates.py
lines_added=12  lines_removed=3  substitutions=2  regions=[{start_line:40,end_line:61}]
bytes_before=8193  bytes_after=8410
```

Line numbers are 1-based. `regions` lists the line ranges an edit touched (for `nuts_replace`, the lines where matches were replaced; for `nuts_write`, the whole file) and is capped at 20 entries. Lines added and removed come from a line diff of the file before and after. Previews write nothing and emit nothing.

The gateway pushes each event to Hyperia as an `Edit` next to the existing network, token and file-operation events. A Hyperia build without the new kind rejects them; n8 treats that as non-fatal and keeps going.

## Containers exit promptly

After an agent quit, its container lingered about five seconds before Docker saw it die, and in a pane the "press any key to close" prompt appeared only after that pause. The cause was in n8's own small HTTP client: the gateway answers the entry's exit-time deregistration with `204 No Content`, which carries no body and no `Content-Length`, and the client treated that as "read until the server closes" and waited out its five-second timeout on a connection the server was keeping open. Bodiless replies are now recognised and returned at once. Container side, so it rides the next image.

## Filesystem reads dropped

The monitor's inotify watcher reported every read of a file or directory as an "accessed" event. Any tool listing a folder produced one, and every container watching the same workspace produced its own copy: on one host that was 8,300 events in 15 minutes, 93 % of the total, carrying no state change. Reads are no longer emitted. Create, modify and remove are unchanged.

Coming from further back? [v0.26.1](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.26.1) was the previous published build.
