"""Regression tests for EvoAgentDeployer._wait_healthy.

The serve's ``GET /health`` probe returns a plain-text body (``ok``), not
JSON — this is the established evo-agent serve liveness convention
(``GET /health`` → ``"ok"``). These tests pin the deployer contract:
an HTTP 200 probe response must be accepted regardless of body format.
"""

from __future__ import annotations

import asyncio
import json
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer

from evo_agent_ale.config import EvoAgentConfig
from evo_agent_ale.deployer import EvoAgentDeployer


class _Handler(BaseHTTPRequestHandler):
    body = b"ok"
    status = 200

    def do_GET(self):
        self.send_response(self.status)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Content-Length", str(len(self.body)))
        self.end_headers()
        self.wfile.write(self.body)

    def log_message(self, *args):  # silence per-request stderr
        pass


def _serve():
    srv = HTTPServer(("127.0.0.1", 0), _Handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv


class _StubExecutor:
    config = None  # convenience alias; _wait_healthy takes cfg as a parameter


def _deployer(port: int) -> EvoAgentDeployer:
    deployer = EvoAgentDeployer(_StubExecutor())
    deployer._serve_proc = None  # no child process to poll in these tests
    cfg = EvoAgentConfig(serve_port=port, start_timeout_s=5.0)
    return deployer, cfg


def test_wait_healthy_accepts_plain_text_ok():
    """/health returning plain text ``ok`` (HTTP 200) must be healthy.

    Regression: the probe used to JSON-parse the body, so every poll raised
    ``JSONDecodeError`` and install() burned the whole start timeout before
    failing with "serve not healthy" — even though the serve was up.
    """
    srv = _serve()
    try:
        deployer, cfg = _deployer(srv.server_address[1])
        asyncio.run(deployer._wait_healthy(cfg))  # must not raise
    finally:
        srv.shutdown()


def test_wait_healthy_accepts_json_body():
    """A JSON-bodied 200 (in case the probe format evolves) stays healthy."""
    srv = _serve()
    _Handler.body = json.dumps({"status": "ok"}).encode()
    try:
        deployer, cfg = _deployer(srv.server_address[1])
        asyncio.run(deployer._wait_healthy(cfg))
    finally:
        srv.shutdown()


def test_wait_healthy_rejects_http_error():
    """A 5xx probe must NOT be reported healthy."""
    srv = _serve()
    _Handler.status = 503
    try:
        deployer, cfg = _deployer(srv.server_address[1])
        try:
            asyncio.run(deployer._wait_healthy(cfg))
        except RuntimeError as exc:
            assert "not healthy" in str(exc)
        else:
            raise AssertionError("503 probe unexpectedly reported healthy")
    finally:
        srv.shutdown()
