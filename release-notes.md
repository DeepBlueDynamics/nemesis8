# nemesis8 v0.26.4 — Your container, your call 🎛️

When an interactive agent exits, n8 now asks what to do with its container instead of closing it on any key. To get it: `n8 update`, then `n8 build`, then recreate containers.

## The menu

```
What should I do with the current container?
  (R)emove — delete it from Docker; the session stays on disk
  (S)top   — keep it in Docker, stopped, to attach or resume later
  (D)etach — keep it running in the background
(R)emove, (S)top, or (D)etach container? [S]
```

Enter alone means Stop. Each answer does what it says:

- **Remove** deletes the container and prints `n8 resume <session id>` so you can pick the session up in a fresh container.
- **Stop** leaves the exited container in Docker and prints both `n8 attach <name>` to start it again and `n8 resume <session id>`.
- **Detach** keeps the container running; press Ctrl+^ to leave the terminal (Ctrl+6 on Hyperia older than 0.20.17) and `n8 attach <name>` to come back.

The old prompt read a single key, and the Enter that had just submitted the agent's quit command was often still in the terminal buffer, so the prompt vanished before anyone saw it and the container was removed. The menu reads a whole answer and ignores an Enter that arrives within a third of a second of the prompt.

Containers started by the previous image still close the old way; the host keeps removing those.

Coming from further back? [v0.26.3](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.26.3) was the previous published build.
