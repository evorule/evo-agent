# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
"""EvoRule Terminal-Bench agent adapter.

Thin protocol bridge only: forwards the task instruction to a running
evo-agent serve instance over HTTP and reports the result back to the
Terminal-Bench harness. The agent holds no task-execution logic — every
tool call (shell, file, git) is executed by the evo-agent engine inside
the task container and recorded in its audit trail.

Configuration (constructor kwargs override environment variables):

- ``server_url`` / ``EVO_AGENT_SERVER_URL``: base URL of the evo-agent
  serve instance (default ``http://127.0.0.1:8081``).
- ``agent_type`` / ``EVO_AGENT_TYPE``: agent definition to run
  (default ``tb-agent``).
- ``timeout_secs`` / ``EVO_AGENT_TIMEOUT_SECS``: HTTP timeout for a full
  task run (default 3600).
"""

from __future__ import annotations

import json
import os
from pathlib import Path

import requests

from terminal_bench.agents.base_agent import AgentResult, BaseAgent
from terminal_bench.agents.failure_mode import FailureMode


class EvoRuleAgent(BaseAgent):
    """BaseAgent implementation forwarding work to the EvoRule engine."""

    def __init__(self, **kwargs):
        super().__init__(**kwargs)
        self._server_url = str(
            kwargs.get(
                "server_url",
                os.environ.get("EVO_AGENT_SERVER_URL", "http://127.0.0.1:8081"),
            )
        ).rstrip("/")
        self._agent_type = str(
            kwargs.get("agent_type", os.environ.get("EVO_AGENT_TYPE", "tb-agent"))
        )
        self._timeout_secs = float(
            kwargs.get(
                "timeout_secs", os.environ.get("EVO_AGENT_TIMEOUT_SECS", "3600")
            )
        )

    @staticmethod
    def name() -> str:
        return "evo-agent"

    @property
    def version(self) -> str:
        return self._version or "0.1.0"

    def perform_task(
        self,
        instruction: str,
        session,
        logging_dir: Path | None = None,
    ) -> AgentResult:
        """Forward the instruction to the engine and report the outcome.

        The task container name is read from the harness-provided session
        and passed to the engine so its shell tool executes inside the
        task container. This module performs no execution of its own.
        """
        container_name = session.container.name

        response = requests.post(
            f"{self._server_url}/agents/{self._agent_type}/run",
            json={
                "agent_type": self._agent_type,
                "goal": instruction,
                "container": container_name,
            },
            timeout=self._timeout_secs,
        )
        response.raise_for_status()
        payload = response.json()

        if logging_dir is not None:
            logging_dir.mkdir(parents=True, exist_ok=True)
            raw_dump = {
                "agent": {"name": self.name(), "version": self.version},
                "server_url": self._server_url,
                "agent_type": self._agent_type,
                "container": container_name,
                "run_response": payload,
            }
            (logging_dir / "evo-agent-run.json").write_text(
                json.dumps(raw_dump, indent=2, ensure_ascii=False),
                encoding="utf-8",
            )

        if not payload.get("success"):
            return AgentResult(failure_mode=FailureMode.UNKNOWN_AGENT_ERROR)
        return AgentResult()
