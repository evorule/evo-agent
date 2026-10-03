# -*- coding: utf-8 -*-
"""skill→规则化适配器 集成回归（自包含合成样本版）。

三形态内置样本（samples/：表格密集 / 代码块富 / 纯列表）覆盖 gate/c/d/tool-router
四类模板组装路径；测试与产品代码共用 assemble_pack 生成路径（防两路漂移）。
纯本地零外部依赖；真实市面 skill 的补充验证可临时替换 SAMPLES 目录注入。

T1 适配器跑通（unmapped=0 / coverage 三通道恒等）
T3 产物自检（self_check 全绿）
T4 LLM 可消费（knowledge_index / llm_core_note 字段完备）
T5 模板组装正确性（四类模板结构 + replace_itype 全局生效）
T6 id_suffix 版本化 + 向导包 base64 往返

注：流程固化 s0 预检不在本目录依赖链内（执行器属部署流程工具链），
    pack 产物的预检在部署环节执行，此处不重复覆盖。
"""
import base64
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import skill_adapter as sa

SAMPLES_DIR = os.path.join(HERE, 'samples')
SAMPLES = ['table-dense', 'code-rich', 'list-minimal', 'en-mixed']


def build(name):
    doc = sa.parse_skill(os.path.join(SAMPLES_DIR, name, 'SKILL.md'))
    skill_name = doc['frontmatter'].get('name') or name
    return sa.assemble_pack(doc, skill_name, 'it')


def test_t1_assemble():
    for name in SAMPLES:
        pack, channels = build(name)
        cov = pack['coverage']
        assert cov['unmapped'] == 0, (name, cov)
        assert cov['shell_rules'] == len(pack['rules']) >= 1, (name, cov)
        assert cov['knowledge'] >= 1, (name, cov)
        assert cov['total_sections'] == cov['shell_rules'] + cov['knowledge'] + cov['llm_core'], (name, cov)
        assert len(channels) == cov['total_sections'], (name, cov)
    print(f'T1 适配器跑通 PASS（{len(SAMPLES)} 样本）')


def test_t3_self_check():
    for name in SAMPLES:
        pack, _ = build(name)
        problems = sa.self_check(pack)
        assert problems == [], (name, problems)
    print('T3 自检 PASS')


def test_t4_llm_consumable():
    for name in SAMPLES:
        pack, _ = build(name)
        for k in pack['knowledge_index']:
            assert k['trigger_kw'] and k['src'] and k.get('kind'), k
        for c in pack['llm_core_note']:
            assert c['suggested_prompt'] and c['src'], c
        assert 'how_to_fill_rules' in pack['llm_usage_note']
    print('T4 LLM 可消费 PASS')


def test_t5_templates():
    seen = set()
    for name in SAMPLES:
        pack, _ = build(name)
        cls = pack['coverage']['template_classes']
        seen |= {k for k, v in cls.items() if v >= 1}
        for r in pack['rules']:
            tf = json.dumps(r['rule_body']['transform'])
            # replace_itype 全局生效：产物不含任何模板域残留
            assert 'expense_submit' not in tf, r['entry_id']
        if cls['gate'] >= 1:
            g = next(r for r in pack['rules'] if 'tpl-gate' in r['tags'])
            assert '"type": "all"' in json.dumps(g['rule_body']['transform']), g['entry_id']
        if cls['c'] >= 1:
            c = next(r for r in pack['rules'] if 'tpl-c' in r['tags'])
            assert '"type": "all"' in json.dumps(c['rule_body']['transform']), c['entry_id']
        if cls['d'] >= 1:
            d = next(r for r in pack['rules'] if 'tpl-d' in r['tags'])
            assert '"type": "set"' in json.dumps(d['rule_body']['transform']), d['entry_id']
        if cls['tool-router'] >= 1:
            t = next(r for r in pack['rules'] if 'tpl-tool-router' in r['tags'])
            assert json.dumps(t['rule_body']['transform']).count('"type": "branch"') >= 1, t['entry_id']
    assert seen == {'gate', 'c', 'd', 'tool-router'}, f'四类模板路径未全覆盖: {seen}'
    print(f'T5 模板组装 PASS（覆盖 {sorted(seen)}）')


def test_t6_suffix_and_wizard():
    doc = sa.parse_skill(os.path.join(SAMPLES_DIR, 'table-dense', 'SKILL.md'))
    pack, _ = sa.assemble_pack(doc, 'order-audit', 'v9')
    assert pack['rules'], 'v9 生成无规则'
    for r in pack['rules']:
        assert '-v9' in r['entry_id'], r['entry_id']
    wp = sa.build_wizard_pack(pack)
    f0 = wp['files'][0]
    body = json.loads(base64.b64decode(f0['content_base64']))
    orig = next(r['rule_body'] for r in pack['rules'] if r['entry_id'] == body['rule_id'])
    assert body == orig, '向导包 base64 往返失真'
    print('T6 后缀版本化+向导包往返 PASS')


def test_t7_en_mixed_channels():
    """英文混合形态样本通道分布快照（真实样本校准回归：纪律/校验/步骤段须命中预期通道）。"""
    pack, _ = build('en-mixed')
    cov = pack['coverage']
    assert cov['shell_rules'] >= 2, cov
    assert cov['knowledge'] >= 2, cov
    assert cov['unmapped'] == 0, cov
    # 纪律清单段（Requirements for Every Output）须进 enforce 骨架（gate 模板命中）
    assert cov['template_classes']['gate'] >= 1, cov['template_classes']
    print('T7 英文混合样本通道分布 PASS')


if __name__ == '__main__':
    test_t1_assemble()
    test_t3_self_check()
    test_t4_llm_consumable()
    test_t5_templates()
    test_t6_suffix_and_wizard()
    test_t7_en_mixed_channels()
    print('ALL PASS —— skill-adapter 集成回归全绿（自包含样本版）')
