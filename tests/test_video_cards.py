#!/usr/bin/env python3
"""B1 角色卡指令契约测试（tests/test_video_cards.py 的执行本体）。

判据（02 档 §七 B1 行：自著 4-6 张+指令契约测试——引用工具必须真实存在）：
1. 卡结构契约：六节齐（定位/前置依赖/作业流程/质量判据/闸提示/边界）+ frontmatter 平铺 name+description
2. 引用真实性：卡内引用的引擎子命令必须真实存在于 engine.py 的 cmd 分发表；
   引用的技能名必须真实存在于 agents/skills/ 与 agents/*.json 注册表
3. 注册一致性：agents/general.json skills 数组声明的 path 必须指向存在的文件
"""
import json
import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
VIDEO_DIR = REPO / "agents" / "skills" / "video"
ENGINE_PY = Path("D:/evorule-video-engine/engine.py")  # engine 仓真身（sibling）
AGENTS_DIR = REPO / "agents"

REQUIRED_SECTIONS = ["定位", "前置依赖", "作业流程", "质量判据", "闸提示", "边界"]
KNOWN_SKILLS = {"video-producer", "video-reviewer", "video-taste", "video-intake",
                "video-narration", "video-delivery",
                "evorule-deliverable", "git-discipline",
                "evorule-deterministic-compute", "evorule-deep-research"}

# engine.py 真实子命令面（自 engine.py main() cmd 分发实测提取，勿凭记忆增删）
ENGINE_COMMANDS = {"build", "verify", "plan", "report", "events", "init", "pick",
                   "review", "scene", "timeline", "selftest", "shots", "script"}


def parse_frontmatter(text):
    m = re.match(r"\A---\s*\n(.*?)\n---\s*\n", text, re.S)
    if not m:
        return None, text
    fm = {}
    for line in m.group(1).splitlines():
        if ":" in line and not line.strip().startswith("-"):
            k, v = line.split(":", 1)
            fm[k.strip()] = v.strip()
    return fm, text[m.end():]


def collect_cards():
    return sorted(VIDEO_DIR.glob("*.md"))


def test_card_structure():
    """每张卡：frontmatter 平铺 name+description + 六节齐。"""
    cards = collect_cards()
    assert 4 <= len(cards) <= 6, f"卡数量 {len(cards)} 不在 B1 判据带 4-6"
    for card in cards:
        text = card.read_text(encoding="utf-8")
        fm, body = parse_frontmatter(text)
        assert fm is not None, f"{card.name}: 缺 frontmatter 围栏"
        assert fm.get("name"), f"{card.name}: 缺 name"
        assert fm.get("description"), f"{card.name}: 缺 description"
        # 轻量解析器兼容：frontmatter 不得含列表行/嵌套块指示符
        for line in text.split("---")[1].splitlines():
            assert not line.strip().startswith("- "), f"{card.name}: frontmatter 含列表行（definition.rs 会跳过）"
        for sec in REQUIRED_SECTIONS:
            assert f"## " in body and sec in body, f"{card.name}: 缺六节之一「{sec}」"


def test_referenced_engine_commands_exist():
    """卡内引用的 engine.py 子命令必须真实存在（指令契约核心）。"""
    for card in collect_cards():
        text = card.read_text(encoding="utf-8")
        for cmd in re.findall(r"engine\.py\s+(\w+)", text):
            assert cmd in ENGINE_COMMANDS, f"{card.name}: 引用了不存在的子命令 engine.py {cmd}"


def test_referenced_skills_exist():
    """卡内引用的技能名必须真实存在（目录面）。"""
    for card in collect_cards():
        text = card.read_text(encoding="utf-8")
        for skill in re.findall(r"`([a-z][a-z0-9-]+)`", text):
            if skill.startswith("video-") or skill in KNOWN_SKILLS:
                assert skill in KNOWN_SKILLS, f"{card.name}: 引用了不存在的技能 {skill}"


def test_registry_consistency():
    """agents/*.json 里声明的 skills path 必须指向存在的文件。"""
    for jf in AGENTS_DIR.glob("*.json"):
        data = json.loads(jf.read_text(encoding="utf-8"))
        for s in data.get("skills", []):
            p = AGENTS_DIR / s["path"]
            assert p.exists(), f"{jf.name}: 注册的技能文件不存在 {s['path']}"
            fm, _ = parse_frontmatter(p.read_text(encoding="utf-8"))
            assert fm and fm.get("name") == s["name"], \
                f"{jf.name}: 注册名 {s['name']} 与文件 frontmatter name 不一致"


def test_video_cards_registered():
    """六张视频卡全部注册进 general.json（卡注册判据）。"""
    data = json.loads((AGENTS_DIR / "general.json").read_text(encoding="utf-8"))
    reg = {s["name"] for s in data.get("skills", [])}
    for card in collect_cards():
        fm, _ = parse_frontmatter(card.read_text(encoding="utf-8"))
        assert fm["name"] in reg, f"卡 {fm['name']} 未注册进 general.json"


def test_no_upstream_text():
    """防抄袭闸：不得出现对方工艺库特征串（自著重铸铁律）。"""
    banned = ["But-Therefore", "3Blue1Brown 引导发现", "design read"]
    for card in collect_cards():
        text = card.read_text(encoding="utf-8")
        for b in banned:
            assert b not in text, f"{card.name}: 疑似对方原文特征串「{b}」"


if __name__ == "__main__":
    fails = 0
    for fn in [test_card_structure, test_referenced_engine_commands_exist,
               test_referenced_skills_exist, test_registry_consistency,
               test_video_cards_registered, test_no_upstream_text]:
        try:
            fn()
            print(f"PASS {fn.__name__}")
        except AssertionError as e:
            fails += 1
            print(f"FAIL {fn.__name__}: {e}")
    sys.exit(1 if fails else 0)
