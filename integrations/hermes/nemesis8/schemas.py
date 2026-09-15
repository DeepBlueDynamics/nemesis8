"""Tool schemas for the Nemesis8 Hermes plugin.

Defines the JSON Schema definitions exposed to Hermes LLMs for interacting with
the Nemesis8 control plane gateway.
"""

GATEWAY_STATUS = {
    "name": "nemesis8_gateway_status",
    "description": (
        "Check whether the Nemesis8 gateway/control-plane is running, and if so its "
        "active runs, scheduler state, agent count, and uptime. Returns gracefully if "
        "the gateway is not currently running."
    ),
    "parameters": {
        "type": "object",
        "properties": {},
        "required": [],
    },
}

AGENT_LIST = {
    "name": "nemesis8_agent_list",
    "description": (
        "List all Nemesis8 agents known to the control plane (running, idle, exited), "
        "including container IDs, providers, models, and current status."
    ),
    "parameters": {
        "type": "object",
        "properties": {},
        "required": [],
    },
}

AGENT_SPAWN = {
    "name": "nemesis8_agent_spawn",
    "description": (
        "Spawn a new Nemesis8 agent via the gateway to work a prompt in a container. "
        "Returns the newly created agent ID. For workflow guidance, load the "
        "nemesis8:nemesis8-fleet skill with skill_view."
    ),
    "parameters": {
        "type": "object",
        "properties": {
            "prompt": {
                "type": "string",
                "description": "The task or prompt for the new agent to execute",
            },
            "provider": {
                "type": "string",
                "description": "Optional agent provider (e.g. 'claude', 'gemini', 'codex', 'pi', 'hermes')",
            },
            "model": {
                "type": "string",
                "description": "Optional model identifier for the provider",
            },
            "workspace": {
                "type": "string",
                "description": "Optional host workspace directory path to mount into the agent container",
            },
        },
        "required": ["prompt"],
    },
}

AGENT_STOP = {
    "name": "nemesis8_agent_stop",
    "description": (
        "Stop a running Nemesis8 agent by its agent ID. Preserves session artifacts "
        "while terminating the container."
    ),
    "parameters": {
        "type": "object",
        "properties": {
            "id": {
                "type": "string",
                "description": "Agent ID to stop (e.g. host/local ID or slug from nemesis8_agent_list)",
            },
        },
        "required": ["id"],
    },
}

AGENT_KILL = {
    "name": "nemesis8_agent_kill",
    "description": (
        "Alias for nemesis8_agent_stop. Terminate a running agent container by ID."
    ),
    "parameters": {
        "type": "object",
        "properties": {
            "id": {
                "type": "string",
                "description": "Agent ID to kill",
            },
        },
        "required": ["id"],
    },
}
