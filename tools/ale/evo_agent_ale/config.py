"""Per-episode knobs for the evo-agent ALE deployer.

Standalone config dataclass (no shared base, per ``ale_run`` convention):
declares only what :class:`~evo_agent_ale.deployer.EvoAgentDeployer`
consumes. API keys / LLM routing live in the operator's shell env (the
serve process inherits ``os.environ`` plus ``executor.env``); nothing
here carries secrets.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import ClassVar


@dataclass
class EvoAgentConfig:
    """Tunables for :class:`~evo_agent_ale.deployer.EvoAgentDeployer`."""

    name: ClassVar[str] = "evo-agent"

    model: str | None = None
    """Request-level model override (``AgentRunRequest.model``). ``None``
    keeps the serve-side configured model."""

    serve_binary: str = "evo-agent"
    """Path to the evo-agent executable (absolute path recommended)."""

    serve_host: str = "127.0.0.1"
    """Bind host for the per-run serve process."""

    serve_port: int = 18091
    """Bind port for the per-run serve process. Must be unique among
    concurrently running units (``concurrency: 1`` experiments share one
    port safely)."""

    agent_type: str = "general"
    """Agent definition loaded from the serve workdir's agents dir."""

    execution_profile: str | None = None
    """Request-level execution profile (``AgentRunRequest.execution_profile``),
    e.g. ``"sandbox-trusted"`` for the benchmark line. ``None`` keeps
    workbench defaults."""

    tools: tuple[str, ...] | None = None
    """Request-level tool allowlist (``AgentRunRequest.tools``): intersected
    with the agent definition surface (restrict-only), ``*`` suffix wildcards
    supported, ``None`` = no narrowing. Example:
    ``("shell_exec", "file_*", "mcp_vm_*", "mcp_cua_*")``."""

    max_steps: int | None = None
    """Request-level step budget override. ``None`` keeps the agent
    definition value."""

    workspace: str | None = None
    """Request-level sandbox root mapping (``AgentRunRequest.workspace``).
    ``None`` keeps read/write in the serve workdir."""

    start_timeout_s: float = 120.0
    """How long ``install()`` waits for the serve process to pass its
    health check before failing the run."""

    install_bridges: bool = True
    """Install the official vm/cua MCP bridges (vendored in ``ale_run``)
    into the serve workdir and wire them into the generated project config.
    Disable for headless CLI-only tasks with no sandbox."""

    always_allow_bridges: bool = True
    """List every bridge tool in its server's ``always_allow`` entry so
    unattended runs do not stall on human approval. Config-level explicit
    authorization, fully visible in the generated ``evo-agent.toml`` and
    the audit trail. Turn off to keep the default candidate-approval flow."""

    extra_config_toml: str | None = None
    """Verbatim TOML appended to the generated project config — the
    escape hatch for per-question knobs (LLM overrides, extra MCP servers,
    approval policy tweaks) without touching this package."""

    request_timeout_s: float | None = None
    """HTTP timeout for the run POST. ``None`` waits indefinitely — the
    episode wall budget is orchestration-owned and enforced by the
    executor wrapping ``launch()``."""
