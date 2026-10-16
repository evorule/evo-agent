"""EvoAgentDeployer — ALE deployer for the evo-agent harness.

Runs under the ``local`` executor: the deployer lives in the framework's
own Python process and drives a dedicated ``evo-agent serve`` subprocess
on the host. The serve process reaches the eval sandbox through the
official vm/cua MCP bridges (vendored in ``ale_run.agents._assets``),
pointed at the sandbox's cua-server via the ``CUA_SERVER_URL`` env var
that ``executor.cua_bridge_url()`` resolves per substrate.

Surface (``ale_run.base_interface.BaseAgentDeployer`` contract):

  ``install()``        stage the per-run workdir: write the project
                       ``evo-agent.toml`` (bridge MCP config + request
                       knobs), install the two MCP bridges, start serve,
                       wait for ``/health``.
  ``launch(prompt)``   POST the task prompt to ``/agents/<type>/run``,
                       collect the run response, snapshot the session's
                       ATIF trajectory + message transcript to disk,
                       stop serve, return an ``AgentRunResult``.
  ``parse_artifacts``  map the snapshot ATIF trajectory onto the ALE
                       ``TrajectoryBuilder`` (ALE-v1.0 steps), re-attaching
                       screenshots from the content-addressed image store.

HTTP uses the Python stdlib only (``urllib``), per the deployer contract
("operates with Python stdlib only").
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import subprocess
import urllib.request
from pathlib import Path
from typing import Any, ClassVar

from ale_run.base_interface import (
    AgentRunResult,
    BaseAgentDeployer,
    TrajectoryBuilder,
)

from .config import EvoAgentConfig
from .transcript_to_trajectory import populate_builder_from_atif

logger = logging.getLogger(__name__)

# Official bridge tool names (server-local; evo-agent prefixes them with
# ``mcp_<server>_`` when surfacing to the LLM). ``always_allow`` is exact-match,
# so the lists below must track the bridges' READMEs verbatim.
_VM_BRIDGE_TOOLS: tuple[str, ...] = (
    "run_command",
    "read_text",
    "write_text",
    "read_bytes",
    "write_bytes",
    "read_clipboard",
    "write_clipboard",
    "pty_start",
    "pty_input",
    "pty_read",
    "pty_resize",
    "pty_kill",
)
_CUA_BRIDGE_TOOLS: tuple[str, ...] = (
    "key",
    "key_down",
    "key_up",
    "type",
    "hold_key",
    "mouse_move",
    "click",
    "drag",
    "mouse_down",
    "mouse_up",
    "scroll",
    "screenshot",
    "cursor_position",
    "wait",
)

_ATIF_NAME = "atif.json"
_TRANSCRIPT_NAME = "transcript.json"


def _toml_str(value: str) -> str:
    """Render a TOML basic string (JSON escaping is TOML-compatible)."""
    return json.dumps(value)


class EvoAgentDeployer(BaseAgentDeployer):
    """Deployer driving the evo-agent harness against the eval sandbox."""

    default_executor: ClassVar[str] = "local"
    supported_executors: ClassVar[frozenset[str]] = frozenset({"local"})

    def __init__(self, executor: Any):
        super().__init__(executor)
        self._serve_proc: subprocess.Popen[bytes] | None = None

    @property
    def version(self) -> str | None:
        return None

    # ------------------------------------------------------------------
    # install
    # ------------------------------------------------------------------

    async def install(self) -> None:
        cfg: EvoAgentConfig = self.config  # type: ignore[assignment]
        work_dir = Path(self.executor.work_dir)
        agent_root = work_dir / "agent"
        agent_root.mkdir(parents=True, exist_ok=True)
        self._agent_root = agent_root

        bridge_toml = ""
        if cfg.install_bridges:
            bridge_toml = await self._install_bridges(cfg)

        config_path = agent_root / "evo-agent.toml"
        config_path.write_text(
            bridge_toml + (cfg.extra_config_toml or ""), encoding="utf-8"
        )
        logger.info("evo-agent: project config written to %s", config_path)

        self._start_serve(cfg, work_dir)
        try:
            await self._wait_healthy(cfg)
        except Exception:
            self._stop_serve()
            raise
        logger.info(
            "evo-agent: install ok (serve=%s:%s work_dir=%s)",
            cfg.serve_host,
            cfg.serve_port,
            self.executor.work_dir,
        )

    async def _install_bridges(self, cfg: EvoAgentConfig) -> str:
        """Install the official vm/cua MCP bridges and return their TOML.

        Bridges are vendored inside ``ale_run.agents._assets``; the shared
        bootstrap helpers copy them into the serve workdir and run
        ``npm install --production``. Both read ``CUA_SERVER_URL`` and are
        pointed at ``executor.cua_bridge_url()`` (the sandbox cua-server
        endpoint reachable from where the bridges run — this host).
        """
        from ale_run.agents._bootstrap import (
            cua_bridge_env,
            ensure_cua_mcp_server_at,
            ensure_node_npm,
            ensure_vm_mcp_server,
            vm_bridge_env,
        )

        agent_root = self._agent_root
        node_path, _ = await ensure_node_npm()
        vm_dir = await ensure_vm_mcp_server(str(agent_root / "mcp" / "vm"))
        cua_dir = await ensure_cua_mcp_server_at(str(agent_root / "mcp" / "cua"))

        cua_url = self.executor.cua_bridge_url()
        vm_env = vm_bridge_env(self.executor)
        cua_env = cua_bridge_env(self.executor)

        def _server(name: str, bridge_dir: str, env: dict[str, str],
                    tools: tuple[str, ...], timeout: int) -> str:
            allow = ""
            if cfg.always_allow_bridges:
                listed = ", ".join(_toml_str(t) for t in tools)
                allow = f"always_allow = [{listed}]\n"
            env_pairs = ", ".join(
                f"{k} = {_toml_str(v)}" for k, v in env.items()
            )
            return (
                f"[[mcp.servers]]\n"
                f"name = {_toml_str(name)}\n"
                f"command = {_toml_str(node_path)}\n"
                f"args = [{_toml_str(str(Path(bridge_dir) / 'src' / 'index.js'))}]\n"
                f"env = {{ {env_pairs} }}\n"
                f"timeout_secs = {timeout}\n"
                f"{allow}\n"
            )

        logger.info("evo-agent: bridges installed, cua-server url=%s", cua_url)
        return (
            _server("vm", vm_dir, vm_env, _VM_BRIDGE_TOOLS, 1800)
            + _server("cua", cua_dir, cua_env, _CUA_BRIDGE_TOOLS, 600)
        )

    def _start_serve(self, cfg: EvoAgentConfig, work_dir: Path) -> None:
        env = {**os.environ, **(self.executor.env or {})}
        stdout = open(work_dir / "serve-stdout.log", "wb")  # noqa: SIM115
        stderr = open(work_dir / "serve-stderr.log", "wb")  # noqa: SIM115
        self._serve_proc = subprocess.Popen(
            [
                cfg.serve_binary,
                "--workdir", str(self._agent_root),
                "serve",
                "--host", cfg.serve_host,
                "--port", str(cfg.serve_port),
            ],
            stdout=stdout,
            stderr=stderr,
            env=env,
            cwd=str(self._agent_root),
        )

    async def _wait_healthy(self, cfg: EvoAgentConfig) -> None:
        base = f"http://{cfg.serve_host}:{cfg.serve_port}"
        deadline = asyncio.get_running_loop().time() + cfg.start_timeout_s
        while True:
            if self._serve_proc is not None and self._serve_proc.poll() is not None:
                raise RuntimeError(
                    f"evo-agent serve exited early (rc={self._serve_proc.returncode}); "
                    "see serve-stderr.log in the run work_dir"
                )
            try:
                await asyncio.to_thread(self._http_json, "GET", f"{base}/health", None, 5.0)
                return
            except Exception:
                if asyncio.get_running_loop().time() >= deadline:
                    raise RuntimeError(
                        f"evo-agent serve not healthy after {cfg.start_timeout_s}s "
                        f"at {base}/health"
                    ) from None
                await asyncio.sleep(0.5)

    # ------------------------------------------------------------------
    # launch
    # ------------------------------------------------------------------

    async def launch(self, prompt: str) -> AgentRunResult:
        cfg: EvoAgentConfig = self.config  # type: ignore[assignment]
        work_dir = Path(self.executor.work_dir)
        base = f"http://{cfg.serve_host}:{cfg.serve_port}"

        body: dict[str, Any] = {
            "agent_type": cfg.agent_type,
            "goal": prompt,
        }
        if cfg.model is not None:
            body["model"] = cfg.model
        if cfg.execution_profile is not None:
            body["execution_profile"] = cfg.execution_profile
        if cfg.tools:
            body["tools"] = list(cfg.tools)
        if cfg.max_steps is not None:
            body["max_steps"] = cfg.max_steps
        if cfg.workspace is not None:
            body["workspace"] = cfg.workspace

        logger.info("evo-agent: launch (agent_type=%s)", cfg.agent_type)
        started = asyncio.get_running_loop().time()
        try:
            resp = await asyncio.to_thread(
                self._http_json,
                "POST",
                f"{base}/agents/{cfg.agent_type}/run",
                body,
                cfg.request_timeout_s,
            )
        except asyncio.CancelledError:
            # Orchestration wall-budget expiry: reap the child, then re-raise
            # (contract: the executor turns this into status="timeout").
            self._stop_serve()
            raise
        duration_s = asyncio.get_running_loop().time() - started

        success = bool(resp.get("success"))
        session_id = resp.get("session_id")
        error = resp.get("error") if not success else None

        transcript_path: str | None = None
        if session_id:
            transcript_path = await self._snapshot_artifacts(base, work_dir, str(session_id))

        pid = self._serve_pid()
        self._stop_serve()

        if not success:
            return AgentRunResult(
                status="failed",
                transcript_path=transcript_path,
                pid=pid,
                duration_s=duration_s,
                error=str(error or resp),
            )
        return AgentRunResult(
            status="completed",
            transcript_path=transcript_path,
            pid=pid,
            duration_s=duration_s,
        )

    async def _snapshot_artifacts(
        self, base: str, work_dir: Path, session_id: str
    ) -> str | None:
        """Persist the session's ATIF trajectory (+ message transcript).

        Both snapshots land in the executor work_dir so ``parse_artifacts``
        stays a pure local-file function after the serve process is gone.
        Best-effort: a failed snapshot logs a warning and leaves the run
        result intact (parse_artifacts then emits a gap-noting system step).
        """
        atif_path = work_dir / _ATIF_NAME
        try:
            atif = await asyncio.to_thread(
                self._http_json, "GET",
                f"{base}/api/sessions/{session_id}/atif", None, 60.0,
            )
            await asyncio.to_thread(
                atif_path.write_text, json.dumps(atif, indent=2), "utf-8"
            )
        except Exception as exc:  # noqa: BLE001
            logger.warning("evo-agent: ATIF snapshot failed: %s", exc)
            return None
        try:
            transcript = await asyncio.to_thread(
                self._http_json, "GET",
                f"{base}/api/sessions/{session_id}/transcript", None, 60.0,
            )
            await asyncio.to_thread(
                (work_dir / _TRANSCRIPT_NAME).write_text,
                json.dumps(transcript, indent=2), "utf-8",
            )
        except Exception as exc:  # noqa: BLE001
            logger.warning("evo-agent: transcript snapshot failed: %s", exc)
        return str(atif_path)

    def _serve_pid(self) -> int | None:
        return self._serve_proc.pid if self._serve_proc is not None else None

    def _stop_serve(self) -> None:
        proc, self._serve_proc = self._serve_proc, None
        if proc is None or proc.poll() is not None:
            return
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=10)

    # ------------------------------------------------------------------
    # parse_artifacts
    # ------------------------------------------------------------------

    @classmethod
    def parse_artifacts(
        cls,
        *,
        work_dir: Path,
        config: EvoAgentConfig,
        run_result: AgentRunResult,
        builder: TrajectoryBuilder,
    ) -> None:
        atif_path = work_dir / _ATIF_NAME
        if not atif_path.exists():
            builder.add_step(
                source="system",
                message=(
                    f"evo-agent: ATIF snapshot missing at {atif_path}; "
                    "trajectory unavailable (see serve logs in work_dir)"
                ),
                extra={"reason": "no_atif", "run_status": run_result.status},
            )
            return
        try:
            atif = json.loads(atif_path.read_text(encoding="utf-8"))
        except Exception as exc:  # noqa: BLE001
            builder.add_step(
                source="system",
                message=f"evo-agent: ATIF snapshot unreadable: {exc}",
                extra={"reason": "bad_atif", "run_status": run_result.status},
            )
            return

        images_dir = work_dir / "agent" / "data" / "agent-images"
        populate_builder_from_atif(builder, atif, images_dir)

    # ------------------------------------------------------------------
    # HTTP helper (stdlib only)
    # ------------------------------------------------------------------

    @staticmethod
    def _http_json(
        method: str,
        url: str,
        body: dict[str, Any] | None,
        timeout: float | None,
    ) -> Any:
        data = json.dumps(body).encode("utf-8") if body is not None else None
        headers = {"Content-Type": "application/json"} if data else {}
        req = urllib.request.Request(url, data=data, method=method, headers=headers)
        with urllib.request.urlopen(req, timeout=timeout) as resp:  # noqa: S310
            payload = resp.read().decode("utf-8")
        return json.loads(payload) if payload else None
