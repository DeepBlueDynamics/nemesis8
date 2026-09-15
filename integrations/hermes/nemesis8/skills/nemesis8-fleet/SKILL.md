---
name: nemesis8-fleet
description: Orchestrate multi-agent workflows using the Nemesis8 gateway. Guide agent spawning with configured providers and models, monitor fleet status, and manage task agent lifecycles.
---

# Nemesis8 Fleet Orchestration Skill

Use this skill when coordinating multi-agent workflows or delegating bounded tasks via the Nemesis8 gateway.

## Workflow

1. **Verify Gateway**: Call `nemesis8_gateway_status` to ensure the host gateway is running.
2. **Review Fleet**: Call `nemesis8_agent_list` to see existing agents and active runs before spawning new ones.
3. **Spawn Task Agent**: Call `nemesis8_agent_spawn` with a clear, self-contained prompt.
   - **Provider & Model**: Use currently configured providers and models available on the host gateway. Do not invent model IDs. Omit `provider` and `model` to use the gateway's configured defaults.
   - **Workspace**: The `workspace` parameter requires an absolute **host** filesystem path, not a container-local path. Omit `workspace` unless the exact host path is known.
4. **Monitor Progress**: Periodically check `nemesis8_agent_list` to track execution and completion.
5. **Clean Up**: Call `nemesis8_agent_stop` using the agent ID. **Only stop agents created for your current task or explicitly authorized by the user.** Never terminate unknown or unrelated containers.

## Example Invocations

### Spawning an Agent (Default Host Configuration)
```json
{
  "prompt": "Run integration tests in the repo and report failures."
}
```

### Spawning with Explicit Provider
```json
{
  "prompt": "Profile memory usage during gateway reconciliation",
  "provider": "hermes"
}
```

### Stopping an Authorized Agent
```json
{
  "id": "host/n8-task-agent-123"
}
```
