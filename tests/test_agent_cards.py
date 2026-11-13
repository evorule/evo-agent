"""Agent card contract tests: schema completeness, tool registry membership,
card/filename identity, and public-face cleanliness.

Scope: FORMAL cards only (complete schema, non-fixture naming). Test fixtures
(deliberately broken cards exercising the serve-side rejection path, e.g.
probe_violator, and ephemeral test-series cards) are exempt from the formal
contract but must match the fixture naming allowlist.
"""
from __future__ import annotations

import glob
import json
import os
import re

import pytest

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AGENTS_DIR = os.path.join(REPO, "agents")
MANIFEST_RS = os.path.join(REPO, "src", "agent", "tool_manifest.rs")
SERVE_TOOLS_RS = os.path.join(REPO, "src", "api", "serve_tools.rs")
SERVICE_TOOLS_RS = os.path.join(REPO, "src", "service_tools.rs")

REQUIRED_FIELDS = (
    "agent_type", "version", "description", "license", "system_prompt",
    "model", "temperature", "max_steps", "context_window_tokens",
    "step_timeout_secs", "tools", "memory",
)
INTERNAL_REF_RE = re.compile(r"\b[A-Z]-\d{2,3}\b")
HAN_RE = re.compile(r"[\u4e00-\u9fff]")
# 测试夹具卡命名模式（serve 校验拒绝路径夹具/短命测试系列）
FIXTURE_NAME_RE = re.compile(r"^(l1-|probe_|smoke|tb|tc|ua|verdict-demo)")


def _all_cards():
    out = {}
    for p in sorted(glob.glob(os.path.join(AGENTS_DIR, "*.json"))):
        with open(p, encoding="utf-8") as fh:
            out[os.path.splitext(os.path.basename(p))[0]] = json.load(fh)
    return out


def _formal_cards():
    """正式卡 = 非 fixture 命名 + schema 完整。"""
    out = {}
    for name, card in _all_cards().items():
        if FIXTURE_NAME_RE.match(name):
            continue
        if all(f in card for f in REQUIRED_FIELDS):
            out[name] = card
    return out


def _fixture_cards():
    formal = set(_formal_cards())
    return {n: c for n, c in _all_cards().items() if n not in formal}


def _registry_tools() -> set:
    """权威工具值域 = 静态 manifest + serve 工具面清单（含表驱动 adapter 名）。"""
    tools: set = set()
    for path in (MANIFEST_RS, SERVE_TOOLS_RS, SERVICE_TOOLS_RS):
        with open(path, encoding="utf-8") as fh:
            src = fh.read()
        tools |= set(re.findall(r'"([a-z][a-z0-9_]*)"', src))
    return tools


def test_all_cards_parse():
    assert len(_all_cards()) >= 6


def test_formal_cards_have_complete_schema():
    for name, card in _formal_cards().items():
        for field in REQUIRED_FIELDS:
            assert field in card, f"{name}: missing field {field}"


def test_incomplete_nonfixture_cards_are_named_as_fixtures():
    for name, card in _all_cards().items():
        if FIXTURE_NAME_RE.match(name):
            continue
        missing = [f for f in REQUIRED_FIELDS if f not in card]
        assert not missing, (
            f"{name}: incomplete card outside fixture naming: missing {missing}"
        )


def test_card_type_matches_filename_and_unique_among_formal():
    formal = _formal_cards()
    types = [c["agent_type"] for c in formal.values()]
    assert len(set(types)) == len(types)
    for name, card in formal.items():
        assert card["agent_type"] == name


def test_all_formal_referenced_tools_exist_in_registry():
    registry = _registry_tools()
    assert registry, "tool registry extraction failed"
    for name, card in _formal_cards().items():
        for tool in card.get("tools", []):
            if tool.endswith("_*"):
                # 通配族=动态/adapter 注册面（browser_*/inference_*/mcp_*）,
                # 静态提取不可证——跳过核验, 注记动态面
                continue
            assert tool in registry, f"{name}: tool not in registry: {tool}"


def test_formal_license_is_agpl():
    for name, card in _formal_cards().items():
        assert card.get("license") == "AGPL-3.0-or-later", f"{name}: license drift"


def test_formal_system_prompt_english_only_public_face():
    for name, card in _formal_cards().items():
        prompt = card.get("system_prompt", "")
        assert not HAN_RE.search(prompt), f"{name}: system_prompt must be English"
        assert not INTERNAL_REF_RE.search(prompt), f"{name}: internal registry number"
        assert len(prompt) > 200, f"{name}: system_prompt too thin"


def test_formal_memory_namespace_matches_type():
    for name, card in _formal_cards().items():
        mem = card.get("memory") or {}
        assert mem.get("namespace") == card["agent_type"], f"{name}: memory namespace drift"


def test_m1_family_cards_present():
    formal = set(_formal_cards())
    for expected in (
        "spokenword_director", "spokenword_reviewer",
        "clip_factory_director", "clip_factory_reviewer",
        "podcast_director", "podcast_reviewer",
    ):
        assert expected in formal, f"missing M1 family card: {expected}"
