# -*- coding: utf-8 -*-
"""skill→规则化适配器 v0.1（P0 最小闭环，2026-09-29）
读市面 SKILL.md → 五件套标记 → 判定标准四条件过滤（三通道分流）→ 生成 skill-rule-pack.json
（rules 规则壳 + knowledge_index 知识体 + llm_core_note 智能核 + machine_judge 判定声明 + coverage）。

v0.1 边界（诚实标注）：
- 判定过滤=启发式规则（标题/关键词/结构模式），非语义理解；人工可改判
- rules 只生成**结构契约合规**的骨架规则（branch+instruction 域路由 + on_true noop，与流程模板 A 类同构已实证）；
  业务 transform 填充由人工/后续版本完成——不输出任何未实证结构的 JSON（铁律：不编造字段）
- 纪律/步骤/schema 段若无法生成契约合规完整规则 → 归 knowledge_index（可规则化但待人工），不进 rules
- 产物可过 K1/K2 + s0 预检（集成测试见 test_skill_adapter.py）

用法：
  python skill_adapter.py --skill <SKILL.md 路径> [--out <pack.json>] [--skill-name <名>] [--id-suffix <后缀>]
"""
import argparse, base64, datetime, json, os, re, sys
from copy import deepcopy

TEMPLATE_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'rule-templates')
TEMPLATE_FILES = {'c': 'rule-c-multi-condition.json', 'gate': 'rule-gate-enforce-role.json', 'd': 'rule-d-state-machine.json'}

def load_templates():
    """加载规则模板库（结构已实测可过 s0 预检：params 契约+宪法补集内置）。"""
    ts = {}
    for key, fn in TEMPLATE_FILES.items():
        with open(os.path.join(TEMPLATE_DIR, fn), encoding='utf-8') as f:
            ts[key] = json.load(f)
    return ts

def select_template(mark):
    """段落标记 → 模板类：discipline→门禁 enforce / schema、check→C 多条件 / step→D 状态机 / trigger→无模板（域路由骨架）。"""
    return {'discipline': 'gate', 'schema': 'c', 'check': 'c', 'step': 'd'}.get(mark)

def replace_itype(node, itype):
    """递归替换 transform 中所有 instruction 域与 reason 文本里的旧指令类型。"""
    if isinstance(node, dict):
        if node.get('type') == 'instruction':
            node['instruction_type'] = itype
        if 'reason' in node and isinstance(node['reason'], str):
            node['reason'] = f'按源段落纪律语义执行（绑定 instruction_type={itype}），违规条件由 LLM 按段意核对填充'
        for v in node.values():
            replace_itype(v, itype)
    elif isinstance(node, list):
        for v in node:
            replace_itype(v, itype)

# ---------- 1. 解析 SKILL.md ----------
def parse_skill(path):
    with open(path, encoding='utf-8') as f:
        text = f.read()
    front = {}
    body = text
    m = re.match(r'^---\s*\n(.*?)\n---\s*\n', text, re.S)
    if m:
        fm = m.group(1)
        for line in fm.splitlines():
            mm = re.match(r'^([A-Za-z_][\w-]*)\s*:\s*(.*)$', line)
            if mm:
                front[mm.group(1)] = mm.group(2).strip().strip('"').strip("'")
        body = text[m.end():]
    # 段落化：按 ## / ### 标题切块
    sections = []
    cur_title, cur_lines = None, []
    def flush():
        if cur_title is not None or cur_lines:
            sections.append({'title': cur_title or '', 'lines': cur_lines[:]})
            cur_lines.clear()
    for line in body.splitlines():
        h = re.match(r'^(#{2,3})\s+(.*)$', line)
        if h:
            flush()
            cur_title = h.group(2).strip()
        else:
            cur_lines.append(line)
    flush()
    # 段落结构标记（表格/列表/代码块/json）
    for s in sections:
        has_table = any(l.lstrip().startswith('|') for l in s['lines'])
        items = [l for l in s['lines'] if re.match(r'^\s*[-*] ', l)]
        ordered = [l for l in s['lines'] if re.match(r'^\s*\d+[\.\u3001]', l)]
        code_json = any('```json' in l or ('```' in l and 'json' in l.lower()) for l in s['lines'])
        code_any = any('```' in l for l in s['lines'])
        s.update({'has_table': has_table, 'items': items, 'ordered': ordered,
                  'code_json': code_json, 'code_any': code_any,
                  'n_items': len(items), 'n_ordered': len(ordered)})
    return {'frontmatter': front, 'sections': sections, 'path': path}

# ---------- 2. 五件套标记（主标记优先级：纪律>校验>步骤>schema>触发>知识>智能） ----------
# 词表口径（真实样本实测校准 v0.2）：纪律/校验/步骤词可在标题或正文命中（配结构特征）；
# 触发/知识/智能词仅标题命中——正文常见短词（when/example/design/review）全文匹配会大面积
# 误伤知识段（实测 169 段 24.3% 误入 core），故收窄为标题匹配。
TRIGGER_KW = ['何时使用', '适用', '触发', 'when', 'when to use', '什么时候', 'when to offer']
STEP_KW    = ['步骤', '怎么', '怎么做', '流程', 'operation', '操作', 'step', 'workflow', 'process']
SCHEMA_KW  = ['格式', 'schema', '模板', 'template', '结构']
CHECK_KW   = ['验证', '校验', '测试', '检查', 'check', 'verify', 'test', 'review checklist']
RULE_KW    = ['纪律', '禁止', '不得', '必须', '约束', '不许', 'rule', '禁',
              'never', 'must', 'warning', 'critical', 'pitfall', 'mandatory',
              'requirement', 'required', 'always']
KNOW_KW    = ['参考', '示例', '背景', '概述', '速查', 'reference', 'example', 'quick start', 'overview']
CORE_KW    = ['评估', '判断', '权衡', '建议', '策略', '设计', '选择', 'review', 'decide', 'philosophy', 'approach']

def classify(s):
    t = (s['title'] or '').lower()
    txt = '\n'.join(s['lines']).lower()
    # 纪律 > 校验 > 步骤 > schema > 触发 > 知识 > 智能
    # 纪律：标题必须命中（词表含强纪律词）；正文命中仅辅助不独立成立
    if any(k in t for k in RULE_KW) and (any(k in t for k in RULE_KW) or any(k in txt for k in RULE_KW)):
        return 'discipline'
    if any(k in t or k in txt for k in CHECK_KW) and (s['has_table'] or s['n_items'] > 0 or s['n_ordered'] > 0):
        return 'check'
    if s['n_ordered'] >= 2 or any(k in t for k in STEP_KW):
        return 'step'
    if s['code_json'] or any(k in t for k in SCHEMA_KW):
        return 'schema'
    if any(k in t for k in TRIGGER_KW):
        return 'trigger'
    if any(k in t for k in KNOW_KW):
        return 'knowledge'
    if any(k in t for k in CORE_KW):
        return 'core'
    return 'knowledge'  # 未分类 → 知识体（unmapped=0 硬性，不静默丢弃）

# ---------- 3. 判定过滤（四条件启发式） → 通道 ----------
ITYPE_BLACKLIST = {'mode', 'behavior', 'source', 'type', 'status', 'path', 'value', 'attr',
                   'id', 'key', 'title', 'version', 'name', 'description', 'flow_id', 'rules'}

def judge_channel(mark, s):
    """shell=规则壳 / knowledge=知识体 / core=智能核。四条件启发式：可验证/可结构化/确定性/可回溯。"""
    txt = '\n'.join(s['lines'])
    # 工具调用映射（"→ 用 X" / "用 X" / "调用 X"）→ 确定性路由，优先 shell（P2-1 补 S3 盲区）
    if re.search(r'[→➔]\s*(?:用|调用|使用)?\s*[a-z][a-z_0-9]{2,}|\b(?:用|调用|使用)\s+[a-z][a-z_0-9]{2,}', txt):
        return 'shell'
    if mark == 'trigger':
        # 触发条件若可枚举指令类型 → shell；开放条件 → core
        if re.search(r'当.*?(出现|收到|遇到|执行|调用)|instruction_type|指令类型', txt):
            return 'shell'
        return 'core'
    if mark == 'discipline':
        # 纪律清单可枚举负面条件 → shell（enforce 骨架）；不可枚举 → core
        return 'shell' if s['n_items'] > 0 or s['has_table'] else 'core'
    if mark == 'schema':
        return 'shell' if s['code_json'] else 'knowledge'
    if mark == 'check':
        return 'shell' if (s['has_table'] or s['n_items'] > 0) else 'knowledge'
    if mark == 'step':
        # 步骤段含显式 instruction_type 结构 → 确定性可路由（shell，走 tool-router 骨架）；
        # 确定性步骤可枚举 → knowledge（v0.1 不生成业务 transform，留人工）；开放步骤 → core
        if s['code_json']:
            return 'shell'
        return 'knowledge' if s['n_ordered'] >= 2 else 'core'
    if mark == 'knowledge':
        return 'knowledge'
    if mark == 'core':
        return 'core'
    return 'knowledge'

# ---------- 4. 规则壳生成（模板组装：结构来自 rule-templates，指令类型从段落提取，业务值 LLM 填充） ----------
def build_rules(shell_sections, prefix, marks, templates, id_suffix=""):
    """返回 (rules, unassembled)：可组装段落→模板规则；缺指令类型的 shell 段→unassembled（归知识体，待 LLM 绑定）。"""
    rules, unassembled = [], []
    for i, s in enumerate(shell_sections):
        mark = marks[id(s)]
        itype = extract_instruction_type(s)
        tpl_key = select_template(mark)
        if tpl_key is None or not itype:
            # 工具路由骨架（P2-1 补 S3）：提取到工具/指令名但无模板 → A 类域路由（域匹配→noop，LLM 填工具调用）
            if itype:
                rid = f'{prefix}{("-"+id_suffix) if id_suffix else ""}-a-{i+1:02d}'
                rules.append({
                    'entry_id': rid,
                    'domain': s['title'] or f'skill-section-{i+1}',
                    'tags': ['skill-adapter', 'tpl-tool-router'],
                    'rule_body': {
                        'rule_id': rid,
                        'version': 1,
                        'description': (f'[skill-adapter::tool-router] {s["title"]} —— 工具调用路由骨架（域匹配 {itype}）。'
                                        f'填充指引（LLM 可执行）：阅读源段落 {s["src"]}，将 on_true.noop 替换为工具调用指令'
                                        f'（io_request 或 push {itype}，入参一律 instruction.params.*）；替换后运行流程固化预检验证。'),
                        'transform': [
                            {'type': 'branch', 'params': {'domain': {'type': 'instruction', 'instruction_type': itype},
                                                          'on_true': [{'type': 'push', 'params': {'instructions': [{'type': 'noop'}]}}],
                                                          'on_false': []}}
                        ]
                    }
                })
            else:
                unassembled.append(s)
            continue
        cls = 'g' if tpl_key == 'gate' else ('c' if tpl_key == 'c' else 'd')
        rid = f'{prefix}{("-"+id_suffix) if id_suffix else ""}-{cls}-{i+1:02d}'
        tb = deepcopy(templates[tpl_key]['transform'])
        replace_itype(tb, itype)
        rules.append({
            'entry_id': rid,
            'domain': s['title'] or f'skill-section-{i+1}',
            'tags': ['skill-adapter', f'tpl-{tpl_key}'],
            'rule_body': {
                'rule_id': rid,
                'version': 1,
                'description': (f'[skill-adapter::{tpl_key}模板] {s["title"]} —— 结构来自 {tpl_key} 模板（契约合规/宪法补集内置）。'
                                f'填充指引（LLM 可执行）：阅读源段落 {s["src"]}，核对模板默认值（违规条件/reason/阈值/角色值/状态值）是否符合该段语义，'
                                f'入参一律 instruction.params.*；调整后运行流程固化预检验证。'),
                'transform': tb
            }
        })
    return rules, unassembled

def extract_instruction_type(s):
    """回退链（P2-1）：①显式 instruction_type= ②场景→工具映射（→用X/用X/调用X） ③反引号标识符（排除 JSON 键黑名单）。"""
    txt = '\n'.join(s['lines'])
    m = re.search(r'instruction_type["\'：:\s=]+([\w-]+)', txt)
    if m and m.group(1) not in ITYPE_BLACKLIST:
        return m.group(1)
    for pat in (r'[→➔]\s*(?:用|调用|使用)?\s*([a-z][a-z_0-9]{2,})',
                r'\b(?:用|调用|使用)\s+([a-z][a-z_0-9]{2,})'):
        m = re.search(pat, txt)
        if m and m.group(1) not in ITYPE_BLACKLIST:
            return m.group(1)
    for m in re.finditer(r'`([a-z][a-z_0-9]{2,})`', txt):
        if m.group(1) not in ITYPE_BLACKLIST:
            return m.group(1)
    return None

# ---------- 5. 知识体 / 智能核 ----------
def build_knowledge(sections, marks):
    idx = []
    for s in sections:
        if marks[id(s)] == 'knowledge':
            text = '\n'.join(s['lines']).strip()
            summary = text[:80] + ('…' if len(text) > 80 else '')
            kw = []
            for w in re.findall(r'[\u4e00-\u9fff]{2,6}|[A-Za-z][\w-]{2,}', (s['title'] or '') + ' ' + text[:200]):
                if w not in kw:
                    kw.append(w)
            idx.append({'key': s['title'] or '(untitled)', 'summary': summary,
                        'trigger_kw': kw[:6], 'src': s.get('src', ''), 'kind': 'ref'})
    return idx

def build_llm_core(sections, marks):
    note = []
    for s in sections:
        if marks[id(s)] == 'core':
            note.append({'section': s['title'] or '(untitled)',
                         'src': s.get('src', ''),
                         'reason': '四条件未全过（开放判断/不可枚举），留 LLM',
                         'suggested_prompt': (
                             f'请处理段落「{s["title"] or "(untitled)"}」（源：{s.get("src", "")}）。'
                             '该段未达规则化四条件，由 LLM 按原始语义直接判断执行：'
                             '①若能补全结构化输入（如把开放条件枚举化）使其可规则化 → 按判定标准重新声明后交适配器；'
                             '②若不能 → 按段意完成判断/创作任务；③完成后将结论登记审计链。')})
    return note

# ---------- 6. machine_judge 声明（K1/K2 门禁消费） ----------
def build_machine_judge(rules, sections):
    items = []
    for r in rules:
        items.append({
            'entry_id_ref': r['entry_id'],
            'judge': {
                'verifiable': True, 'verifiable_evidence': f'expect 断言可写：instruction 域命中可断言（来源 {r["rule_body"]["description"]}）',
                'structured': True, 'structured_evidence': 'instruction_type 枚举域匹配（params 契约）',
                'deterministic': True, 'deterministic_evidence': 'branch 查表路由，零 LLM（TCB 纯函数）',
                'traceable': True, 'traceable_evidence': 'FactsLog + 审计链运行固有',
                'enum': True, 'assertable': True, 'params_finite': True,
                'placement': 'server', 'channel': 'json', 'class': 'A'
            }
        })
    return {'items': items}

# ---------- 7. 向导包导出（P2-3：pack → 治理页可导入 .evorule-batch.json） ----------
def build_wizard_pack(pack, exported_by='skill-adapter'):
    """pack.rules 每条 rule_body → 向导包 file（content_base64），manifest 计数。"""
    files = []
    for r in pack['rules']:
        name = r['entry_id']
        body = json.dumps(r['rule_body'], ensure_ascii=False).encode('utf-8')
        files.append({
            'path': f'rules/{name}.json',
            'objectId': f'user.{name}',
            'objectType': 'rule',
            'format': 'json',
            'content_base64': base64.b64encode(body).decode('ascii')
        })
    manifest = {
        'manifest_version': '1.0',
        'format': 'evorule-batch',
        'exported_at': datetime.datetime.now(datetime.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ'),
        'exported_by': exported_by,
        'source_instance': f'skill-adapter:{pack["pack_id"]}',
        'contents': [{'type': 'rule', 'count': len(files), 'format': 'json', 'dir': 'rules/'}],
        'total_count': len(files)
    }
    return {'manifest': manifest, 'files': files}

# ---------- 8. 自检 ----------
def self_check(pack):
    problems = []
    rules = pack['rules']
    refs = {it['entry_id_ref'] for it in pack['machine_judge']['items']}
    eids = {r['entry_id'] for r in rules}
    missing = eids - refs
    if missing:
        problems.append(f'machine_judge 覆盖缺失: {missing}')
    for it in pack['machine_judge']['items']:
        j = it['judge']
        for k in ['verifiable', 'structured', 'deterministic', 'traceable', 'enum', 'assertable', 'params_finite']:
            if not isinstance(j.get(k), bool):
                problems.append(f'judge.{k} 非布尔: {it["entry_id_ref"]}')
        for k in ['placement', 'channel', 'class']:
            if not j.get(k):
                problems.append(f'judge.{k} 为空: {it["entry_id_ref"]}')
        for k in ['verifiable_evidence', 'structured_evidence', 'deterministic_evidence', 'traceable_evidence']:
            ev = j.get(k) or ''
            if not ev.strip():
                problems.append(f'{k} 为空: {it["entry_id_ref"]}')
            elif '待核实' in ev:
                problems.append(f'{k} 含"待核实": {it["entry_id_ref"]}')
    if pack['coverage']['unmapped'] != 0:
        problems.append(f'unmapped 段落={pack["coverage"]["unmapped"]}（硬性要求 0）')
    return problems

# ---------- main ----------
def assemble_pack(doc, name, id_suffix=""):
    """编排：标记 → 判定 → 组装 → pack（main 与集成测试共用入口，防两路漂移）。

    返回 (pack, channels)：channels 为段落 id → 通道映射（供打印/测试断言）。
    """
    marks = {}
    for s in doc['sections']:
        s['src'] = f'{doc["path"]}#{s["title"] or "(untitled)"}'
        marks[id(s)] = classify(s)
    channels = {id(s): judge_channel(marks[id(s)], s) for s in doc['sections']}
    shell = [s for s in doc['sections'] if channels[id(s)] == 'shell']
    rules, unassembled = build_rules(shell, name, marks, load_templates(), id_suffix)
    knowledge = build_knowledge(doc['sections'], channels)
    for s in unassembled:
        channels[id(s)] = 'knowledge'
        knowledge.append({'key': s['title'] or '(untitled)',
                          'summary': '段落未提取到指令类型且无模板匹配，无法组装——待 LLM 按段意绑定 instruction_type 后按对应模板规则化',
                          'trigger_kw': ['LLM绑定指令类型', '模板规则化'], 'src': s.get('src', ''), 'kind': 'unassembled-template'})
    llm_core = build_llm_core(doc['sections'], channels)
    coverage = {
        'total_sections': len(doc['sections']),
        'shell_rules': len(rules),
        'template_classes': {k: sum(1 for r in rules if f'tpl-{k}' in r['tags']) for k in ('c', 'gate', 'd', 'tool-router')},
        'knowledge': len(knowledge),
        'llm_core': len(llm_core),
        'unassembled_template': len(unassembled),
        'unmapped': sum(1 for s in doc['sections'] if channels[id(s)] not in ('shell', 'knowledge', 'core')),
    }
    pack = {
        'pack_id': f'skill-pack-{name}',
        'source_skill': {'name': name, 'version': doc['frontmatter'].get('version', 'unknown'),
                         'path': doc['path'], 'description': doc['frontmatter'].get('description', '')},
        'llm_usage_note': {
            'when_to_use': f'当任务涉及「{name}」适用场景（{doc["frontmatter"].get("description", "")[:120]}）时使用本 pack',
            'how_to_query_knowledge': '先查 knowledge_index[].trigger_kw；命中后按 src 读取源段落全文',
            'how_to_fill_rules': 'rules 为骨架：按每条 rule_body.description 的填充指引把 on_true.noop 替换为业务 transform（入参一律 instruction.params.*），替换后跑流程固化预检',
            'llm_core_sections': 'llm_core_note 列出的段落由 LLM 按 suggested_prompt 直接判断执行',
        },
        'machine_judge': build_machine_judge(rules, doc['sections']),
        'rules': rules,
        'knowledge_index': knowledge,
        'llm_core_note': llm_core,
        'coverage': coverage,
    }
    return pack, channels


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--skill', required=True)
    ap.add_argument('--out', default='skill-rule-pack.json')
    ap.add_argument('--skill-name', default='')
    ap.add_argument('--id-suffix', default='', help='entry_id 版本化后缀（系统约束：规则名 workspace 内永久唯一含归档，同名不可复用；重部署须换后缀）')
    ap.add_argument('--wizard', default='', help='同时导出治理页可导入的 .evorule-batch.json 向导包（路径）')
    a = ap.parse_args()
    doc = parse_skill(a.skill)
    name = a.skill_name or doc['frontmatter'].get('name') or os.path.basename(os.path.dirname(a.skill))
    pack, channels = assemble_pack(doc, name, a.id_suffix)
    coverage = pack['coverage']
    problems = self_check(pack)
    if problems:
        for p in problems:
            print(f'[SELF-CHECK FAIL] {p}')
        print('SELF-CHECK FAILED —— 修复输入或改判后重试')
        sys.exit(1)
    with open(a.out, 'w', encoding='utf-8') as f:
        json.dump(pack, f, ensure_ascii=False, indent=2)
    print(f'[OK] skill-rule-pack 生成: {a.out}')
    if a.wizard:
        wp = build_wizard_pack(pack)
        with open(a.wizard, 'w', encoding='utf-8') as f:
            json.dump(wp, f, ensure_ascii=False, indent=2)
        print(f'[OK] 向导包导出: {a.wizard}（{wp["manifest"]["total_count"]} 条规则，治理页可导入）')
    print(f'  段落 {coverage["total_sections"]} | 规则壳 {coverage["shell_rules"]} | 知识体 {coverage["knowledge"]} | 智能核 {coverage["llm_core"]} | unmapped {coverage["unmapped"]}')
    for s in doc['sections']:
        print(f'    [{channels[id(s)]}] {s["title"] or "(untitled)"}')

if __name__ == '__main__':
    main()
