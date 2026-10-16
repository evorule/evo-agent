"""evo-agent ALE deployer package.

Exposes :class:`EvoAgentDeployer` (an ``ale_run.base_interface.
BaseAgentDeployer`` subclass) plus its :class:`EvoAgentConfig` dataclass,
wiring the evo-agent harness into the Agents' Last Exam runner as a
``local``-executor agent: the deployer drives a per-run ``evo-agent serve``
process on the host, which reaches the eval sandbox over the official
vm/cua MCP bridges.
"""

from .config import EvoAgentConfig
from .deployer import EvoAgentDeployer

__all__ = ["EvoAgentConfig", "EvoAgentDeployer"]
