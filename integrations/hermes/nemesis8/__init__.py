"""Nemesis8 Hermes Plugin.

Provides native integration between Hermes and the Nemesis8 control plane gateway.
Registers namespaced tools:
- nemesis8_gateway_status: Check status, active runs, agent count, uptime
- nemesis8_agent_list: List all managed agent containers and their states
- nemesis8_agent_spawn: Launch a new containerized agent for a given task
- nemesis8_agent_stop: Terminate an active agent container
- nemesis8_agent_kill: Alias for nemesis8_agent_stop
"""

import logging
from pathlib import Path
from . import schemas, tools

logger = logging.getLogger(__name__)


def register(ctx):
    """Wire schemas to handlers for Nemesis8 gateway tools and register skills.

    Follows the Hermes plugin registration protocol:
    ctx.register_tool(name=..., toolset=..., schema=..., handler=...)
    ctx.register_skill(name=..., path=..., description=...)
    """
    toolset = "nemesis8"

    ctx.register_tool(
        name="nemesis8_gateway_status",
        toolset=toolset,
        schema=schemas.GATEWAY_STATUS,
        handler=tools.gateway_status,
    )
    ctx.register_tool(
        name="nemesis8_agent_list",
        toolset=toolset,
        schema=schemas.AGENT_LIST,
        handler=tools.agent_list,
    )
    ctx.register_tool(
        name="nemesis8_agent_spawn",
        toolset=toolset,
        schema=schemas.AGENT_SPAWN,
        handler=tools.agent_spawn,
    )
    ctx.register_tool(
        name="nemesis8_agent_stop",
        toolset=toolset,
        schema=schemas.AGENT_STOP,
        handler=tools.agent_stop,
    )
    ctx.register_tool(
        name="nemesis8_agent_kill",
        toolset=toolset,
        schema=schemas.AGENT_KILL,
        handler=tools.agent_kill,
    )

    ctx.register_skill(
        name="nemesis8-fleet",
        path=Path(__file__).parent / "skills" / "nemesis8-fleet" / "SKILL.md",
        description="Operate the Nemesis8 gateway and agent fleet",
    )

    logger.info("Nemesis8 plugin registered successfully with toolset '%s'", toolset)
