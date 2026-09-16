# nemesis8 v0.25.1 — Tunnels that notice 🩺

The gateway now watches every tunnelled port. When the container behind one dies or restarts, the mapping is marked degraded and logged, held for a grace window so a restarting container can be re-attached, and removed if nothing comes back — freeing the host port. Before this, a mapping stayed "live" forever after its container died: the port stayed bound, the next backend on that port got "already in use", and a desktop that connected found a tunnel with nothing behind it. To get it: `n8 update`, then restart the gateway (`n8 serve --stop && n8 serve --background`).

## The gateway watches its tunnels

Every ten seconds the gateway checks each mapping for one thing: is a container-side tunnel client actually attached? That signal is deliberately service-agnostic. Probing the host port itself always succeeds (the forwarder accepts, then waits), and OAuth callback ports have no listener inside the container until a login is in flight, so probing the *service* would wrongly tear those down. A dead container's clients disconnect and are reaped; a healthy one always has some parked.

A mapping with nothing attached becomes **degraded** and starts a two-minute clock (`NEMESIS8_TUNNEL_DECAY_SECS` to change it). If its container is running — the Docker-restart case, where `--restart unless-stopped` brings the container back but its tunnel clients, which were `docker exec`'d, don't come with it — the gateway re-execs them, and the mapping goes back to live on the next tick. If the window runs out, the mapping is removed and the host port is released. Every transition is a line in `~/.nemesis8/home/gateway.log`: degraded (with the reason), recovered, re-attached, removed.

## Asking for a port someone else holds

`/expose` for an exact host port now checks the current holder first. A holder that is degraded, or whose container the registry says has exited, is evicted on the spot and the port granted — no waiting out the window. A holder that is healthy is refused with its name and state and the command to stop it. A port held by some other program on the machine — a desktop app's own local server, say — is reported as exactly that, since n8 can't evict it. An in-flight or unknown holder is never evicted: unknown is not the same as dead.

## Also

- `/unexpose` no longer tries to `docker exec` into a container that is already gone, so removing a dead mapping stops producing a spurious "failed to stop tunnel client" warning.
- `/exposed` now shows `degraded` as a mapping state, with `degraded_since` and `degraded_reason` when set.

Coming from further back? [v0.25.0](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.25.0) was the previous published build.
