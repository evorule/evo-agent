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

Per-task rule declaration (optional; disabled unless ``rules_dir`` is set):

- ``rules_dir`` / ``EVO_AGENT_RULES_DIR``: the rule directory served by the
  governance engine (``--rules-dir``). When set, the adapter declares a
  per-task "in-domain permit" rule entry before each task run and removes it
  afterwards, reloading the rule engine around the run. The entry is purely
  declarative (no enforcement of its own) and lives only for the duration of
  the task; removing the configuration removes the feature.
- ``evorule_server_url`` / ``EVO_RULE_SERVER_URL``: base URL of the
  governance engine instance (default ``http://127.0.0.1:18080``).
- ``evorule_auth_token`` / ``EVO_RULE_AUTH_TOKEN``: optional bearer token
  for the governance engine's reload endpoint (unset = unauthenticated,
  local-development mode).
"""

from __future__ import annotations

import json
import logging
import os
import re
from datetime import date
from pathlib import Path

import requests

from terminal_bench.agents.base_agent import AgentResult, BaseAgent
from terminal_bench.agents.failure_mode import FailureMode

logger = logging.getLogger(__name__)

# File-name prefix reserved for this adapter's per-task rule entries inside
# the rule directory. Entries are written before a task run and removed in a
# ``finally`` block; stale entries from a crashed run are cleaned up lazily
# at the start of the next run.
_TASK_PERMIT_PREFIX = "00_constraint_task_"


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
        rules_dir = kwargs.get("rules_dir", os.environ.get("EVO_AGENT_RULES_DIR"))
        self._rules_dir = str(rules_dir) if rules_dir else None
        self._evorule_server_url = str(
            kwargs.get(
                "evorule_server_url",
                os.environ.get("EVO_RULE_SERVER_URL", "http://127.0.0.1:18080"),
            )
        ).rstrip("/")
        token = kwargs.get(
            "evorule_auth_token", os.environ.get("EVO_RULE_AUTH_TOKEN")
        )
        self._evorule_auth_token = str(token) if token else None

    @staticmethod
    def name() -> str:
        return "evo-agent"

    # ------------------------------------------------------------------
    # Per-task rule declaration
    #
    # Before each task run the adapter declares one per-task "in-domain
    # permit" rule entry in the governance rule directory and reloads the
    # engine; after the run (success or failure) the entry is removed and
    # the engine reloaded again. The entry is declarative only: it records
    # the task-scoped permit boundary for audit/inspection and provides a
    # placeholder for future guards to grant task-domain exemptions; it
    # adds no enforcement of its own. Reload failures degrade to warnings —
    # the run proceeds, since the entry never changes enforcement behaviour.
    # ------------------------------------------------------------------

    @staticmethod
    def _permit_slug(task_id: str) -> str:
        slug = re.sub(r"[^A-Za-z0-9_-]+", "-", task_id).strip("-")
        return slug or "task"

    @staticmethod
    def _resolve_task_id(logging_dir: Path | None, container_name: str | None) -> str:
        # Harness layout: <output_path>/<task_id>/<trial_name>.
        if logging_dir is not None:
            parent = Path(logging_dir).parent.name
            if parent:
                return parent
        if container_name:
            return container_name.removeprefix("tb__")
        return "unknown"

    def _task_permit_payload(self, task_id: str) -> dict:
        """Build the per-task rule-set declaration (no enforcement)."""
        return {
            "$schema": "https://evorule.org/schemas/rule_set/v1.0.json",
            "kind": "rule_set",
            "id": f"com.evorule.constraint.task.{self._permit_slug(task_id)}",
            "version": "1.0.0",
            "metadata": {
                "tier": "constraint",
                "title": f"任务域内许可声明（{task_id}）",
                "description": (
                    "per-task 任务宪法「任务域内许可」声明条目：任务指令显式"
                    "要求的任务域内操作（含安全演练类任务对靶场内目标的操作）"
                    "属任务域内许可行为，以任务指令为界、通用工程规范边界内、"
                    "不含任务解法。语义分工：本条目不新增任何 enforce 拦截，"
                    "仅在 metadata 声明许可边界供对照检查与审计留痕，并以 "
                    "transform 为未来可能命中任务域内行为的守卫提供显式备裁位"
                    "（空动作放行，与缺省语义一致）。per-task 生命周期：任务"
                    "结束即删除本条目。"
                ),
                "license": "AGPL-3.0-or-later",
                "created": date.today().isoformat(),
                "permits": [
                    {
                        "scope": "task_domain",
                        "task_id": task_id,
                        "statement": "任务指令显式要求的任务域内操作许可（per-task 生命周期，任务结束即删）",
                    }
                ],
                "tags": ["constraint", "per-task", "task-permit", "declaration"],
            },
            "transform": [
                {
                    "type": "branch",
                    "params": {
                        "domain": {
                            "type": "all",
                            "inner": [
                                {
                                    "type": "instruction",
                                    "instruction_type": "tool_trace",
                                },
                                {
                                    "type": "exists",
                                    "path": "instruction.params.value.program_hits",
                                },
                            ],
                        },
                        "on_true": [],
                    },
                }
            ],
        }

    def _reload_rules(self) -> dict:
        headers = {}
        if self._evorule_auth_token:
            headers["Authorization"] = f"Bearer {self._evorule_auth_token}"
        resp = requests.post(
            f"{self._evorule_server_url}/api/rules/reload",
            json={},
            headers=headers,
            timeout=30,
        )
        resp.raise_for_status()
        body = resp.json()
        if not body.get("reload_ok"):
            raise RuntimeError(body.get("error") or "reload_ok=false")
        return body

    def _remove_permit_files(self, rules_dir: Path) -> None:
        for stale in rules_dir.glob(f"{_TASK_PERMIT_PREFIX}*.json"):
            stale.unlink()

    def _declare_task_permit(self, task_id: str) -> None:
        rules_dir = Path(self._rules_dir)
        rules_dir.mkdir(parents=True, exist_ok=True)
        self._remove_permit_files(rules_dir)
        slug = self._permit_slug(task_id)
        entry_path = rules_dir / f"{_TASK_PERMIT_PREFIX}{slug}.json"
        entry_path.write_text(
            json.dumps(self._task_permit_payload(task_id), indent=2, ensure_ascii=False),
            encoding="utf-8",
        )
        body = self._reload_rules()
        previous, current = body.get("previous_rules"), body.get("current_rules")
        if previous is not None and current is not None and current != previous + 1:
            logger.warning(
                "rule engine reload after task-permit declaration: "
                "expected %s+1 rules, got %s (entry may have been rejected)",
                previous,
                current,
            )
        else:
            logger.info("declared task permit %s (rules now %s)", slug, current)

    def _revoke_task_permits(self) -> None:
        rules_dir = Path(self._rules_dir)
        self._remove_permit_files(rules_dir)
        body = self._reload_rules()
        logger.info(
            "revoked task permits (rules now %s)", body.get("current_rules")
        )

    @property
    def version(self) -> str:
        return self._version or "0.1.0"

    # ------------------------------------------------------------------
    # ATIF trajectory export (G2 reporter, thin pull)
    #
    # After each run the engine reports the session id it established; the
    # full ATIF trajectory is assembled engine-side (journal x audit chain x
    # transcript three-source join, see src/agent/atif.rs) and served
    # read-only at ``GET /api/sessions/{id}/atif``. This adapter only
    # persists the trajectory (``trajectory.json``) and a small statistics
    # digest (``atif-summary.json``) next to the harness logs. Export
    # failures degrade to warnings and never affect the task outcome.
    # ------------------------------------------------------------------

    def _export_atif_trajectory(self, logging_dir: Path, payload: dict) -> None:
        session_id = payload.get("session_id")
        if not session_id:
            logger.warning(
                "run response carries no session_id; skipping ATIF export"
            )
            return
        resp = requests.get(
            f"{self._server_url}/api/sessions/{session_id}/atif",
            params={"agent_type": self._agent_type},
            timeout=120,
        )
        resp.raise_for_status()
        trajectory = resp.json()
        (logging_dir / "trajectory.json").write_text(
            json.dumps(trajectory, indent=2, ensure_ascii=False),
            encoding="utf-8",
        )
        steps = trajectory.get("steps") or []
        summary = {
            "schema_version": trajectory.get("schema_version"),
            "session_id": trajectory.get("session_id"),
            "trajectory_id": trajectory.get("trajectory_id"),
            "steps": len(steps),
            "tool_calls": sum(
                len(step.get("tool_calls") or [])
                for step in steps
                if isinstance(step, dict)
            ),
            "final_metrics": trajectory.get("final_metrics"),
            "journal_seq_range": (trajectory.get("extra") or {}).get(
                "journal_seq_range"
            ),
        }
        (logging_dir / "atif-summary.json").write_text(
            json.dumps(summary, indent=2, ensure_ascii=False),
            encoding="utf-8",
        )
        logger.info(
            "ATIF trajectory exported (%s steps) to %s",
            summary["steps"],
            logging_dir / "trajectory.json",
        )

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

        When ``rules_dir`` is configured, a per-task declarative rule entry
        is declared around the run and revoked afterwards; declaration
        failures degrade to warnings and never abort the run.
        """
        container_name = session.container.name
        task_id = self._resolve_task_id(logging_dir, container_name)

        permit_declared = False
        if self._rules_dir:
            try:
                self._declare_task_permit(task_id)
                permit_declared = True
            except Exception as exc:
                logger.warning(
                    "task rule declaration failed (continuing without it): %s",
                    exc,
                )

        try:
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
                try:
                    self._export_atif_trajectory(logging_dir, payload)
                except Exception as exc:
                    logger.warning(
                        "ATIF trajectory export failed (continuing without it): %s",
                        exc,
                    )

            if not payload.get("success"):
                return AgentResult(failure_mode=FailureMode.UNKNOWN_AGENT_ERROR)
            return AgentResult()
        finally:
            if self._rules_dir and permit_declared:
                try:
                    self._revoke_task_permits()
                except Exception as exc:
                    logger.warning(
                        "task rule revocation failed (stale entries are "
                        "cleaned up before the next run): %s",
                        exc,
                    )
