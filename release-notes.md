# nemesis8 v0.25.5 — First ask counts 📞

A container's request to open its OAuth callback port could fail on the first try with "agent has no live container", and nothing retried. Login flows in that container then had no callback tunnel. This release makes the gateway answer that first request correctly. To get it: `n8 update`; containers pick up their half after `n8 build`.

## What went wrong

On boot the entry registers with the gateway and, right after, asks it to expose the provider's fixed OAuth callback port (antigravity: 8766). The registration carried no container name, and the gateway only learned which container belonged to the agent from its reconcile loop, which runs every 10 seconds. So the expose that followed within the same second found a record with no container behind it and got a 404. Ten seconds later the record was complete, but the expose was not retried.

## What changed

- **Gateway**: when a record has no container yet, the tunnel resolver now looks the container up by name (n8 names containers after the agent id, and checks the agent label too), completes the record, and proceeds. This alone fixes the race for every existing container image.
- **Entry**: registration now includes the container's name, so the record is complete from the first request. The OAuth callback expose retries three times with a short backoff, which also absorbs the transient "docker exec exited 1" seen right after launch.

Coming from further back? [v0.25.4](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.25.4) was the previous published build.
