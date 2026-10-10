// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! G9:工作流引擎 —— DAG 拓扑编排多 agent
//!
//! ## 设计
//!
//! 工作流是一个 **DAG**(有向无环图),用 JSON DSL 定义:
//!
//! ```json
//! {
//!   "workflow_id": "research_and_write",
//!   "nodes": [
//!     { "id": "research_rust", "agent_type": "researcher", "task": "...", "depends_on": [] },
//!     { "id": "write_report",  "agent_type": "writer",     "task_template": "基于: {research_rust}", "depends_on": ["research_rust"] }
//!   ],
//!   "output_node": "write_report"
//! }
//! ```
//!
//! ## 执行算法
//!
//! 1. **拓扑排序**(Kahn 分层):按 `depends_on` 把节点分成若干层,同层无互相依赖
//! 2. **逐层规划**:先决定本层哪些节点执行、哪些跳过——
//!    - 节点声明了 `run_when`(workflow_dag v1.1 条件分支)→ 按条件求值决定去留
//!      (豁免级联:即使上游被跳过,条件为真仍执行)
//!    - 未声明 `run_when` 的节点:任一直接依赖被跳过 → 级联跳过
//! 3. **逐层执行**:同层待执行节点并行(`DelegateContext::delegate_parallel`)
//! 4. **模板渲染**:下一层的 `task_template` 中 `{node_id}` 被上游结果替换;
//!    被跳过的上游节点占位符替换为空字符串
//! 5. 任一**执行中**节点失败 → 整个工作流终止,返回 Err(被跳过的节点不算失败)
//! 6. 返回 `output_node` 的结果;`output_node` 被跳过 → 返回 Err(无静默空结果)
//!
//! ## 边界(§9.6)
//!
//! - 条件分支为**节点级静态条件**(v1.1 `run_when`,纯函数求值,谓词最小集
//!   `contains`/`equals`/`not_contains`);动态 plan-and-execute 循环不在本引擎范围
//! - 节点失败默认终止整个工作流(无 `on_failure: skip`)
//! - `run_when` 引用的节点被跳过/无结果时视作空字符串(确定性语义,同输入必同输出)

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use jsonschema::Validator;

use crate::agent::delegate::DelegateContext;
use crate::agent::journal::{evorule_digest, JournalEvent, JournalLine, JournalWriter};

/// 条件谓词(workflow_dag v1.1 `run_when.op` 最小集,冻结于该版本)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionOp {
    /// 上游结果包含 `value`
    Contains,
    /// 上游结果完全等于 `value`
    Equals,
    /// 上游结果不包含 `value`
    NotContains,
}

/// 节点级执行条件(workflow_dag v1.1 新增可选字段 `run_when`)
///
/// 语义(纯函数,同输入必同输出):
/// - 被观察节点(`node`)的结果取自已完成结果表;该节点被跳过或尚无结果时视作**空字符串**
/// - 求值为真 → 执行本节点;为假 → 跳过本节点
/// - 声明了 `run_when` 的节点**豁免级联跳过**(去留完全由自身条件决定);
///   未声明的节点在任一直接依赖被跳过时级联跳过
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunWhen {
    /// 被观察节点的 id(必须存在于工作流,且位于本节点的更早拓扑层)
    pub node: String,
    /// 比较谓词
    pub op: ConditionOp,
    /// 期望值(与被观察节点的结果字符串比较)
    pub value: String,
}

/// compute 节点输入引用（workflow_dag v1.2 / 交付物 4 §3；交付物 5 注记一·2）
///
/// 源文档形态只有 `Node`（字符串引用）；`Empty` 为物化器改写产物——iter0 的
/// `prev.X` 输入消解为空输入字面量，执行期视作空字符串（与 run_when 空观察源
/// 同一语义：被引用节点无结果 → 空串）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ComputeInput {
    /// 节点引用（展开后 = 副本全名/全局原名）
    Node(String),
    /// 空输入字面量（物化器产物，源文档无此形态）
    Empty,
}

/// strcmp 比较模式（交付物 4 §4.1）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrcmpMode {
    /// 两输入完全相等（收敛检测主用；结果词表 equal|different）
    Equal,
    /// inputs[0] 包含 inputs[1]（结果词表 contained|not_contained）
    Contains,
}

/// numeric_cmp 比较模式（交付物 4 §4.2）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NumericCmpMode {
    /// 小于
    Lt,
    /// 小于等于
    Le,
    /// 大于
    Gt,
    /// 大于等于
    Ge,
    /// 相等
    Eq,
}

/// 纯函数节点声明（workflow_dag v1.2 / D-03 拍板方案③；交付物 4 实现契约）
///
/// 执行形态约束：不经 delegate（不占 max_concurrent/max_depth）、execute 层循环
/// 内同步内联求值、无 IO 无副作用、不产生 IoRequest；函数目录封闭
/// （v1.2 三种 strcmp/numeric_cmp/regex_match + v1.3 新增 24 种算术/字符串/日期/逻辑，
/// 共 27 种），同输入必同输出。新增函数 = 新 schema 版本 + 治理评审。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "function", rename_all = "snake_case")]
pub enum ComputeSpec {
    /// 字符串比较（恰 2 输入）
    Strcmp {
        /// 输入引用（恰 2 个）
        inputs: Vec<ComputeInput>,
        /// 比较模式
        mode: StrcmpMode,
    },
    /// 数值比较（IEEE 754 双精度；1..=2 输入，单输入必带 threshold、双输入禁带）
    NumericCmp {
        /// 输入引用（1..=2 个）
        inputs: Vec<ComputeInput>,
        /// 比较模式
        mode: NumericCmpMode,
        /// 单输入形态的阈值（双输入形态禁止携带）
        #[serde(default, skip_serializing_if = "Option::is_none")]
        threshold: Option<f64>,
    },
    /// 正则匹配（恰 1 输入；pattern 加载期校验，非法即拒载）
    RegexMatch {
        /// 输入引用（恰 1 个）
        inputs: Vec<ComputeInput>,
        /// 正则 pattern（加载期编译校验）
        pattern: String,
    },
    // ----- v1.3 新增 24 函数（结果词表与错误语义总纲见 schema v1.3 description） -----
    /// 算术加（2..=8 输入，i64 依次累加，checked 溢出=节点失败）
    Add {
        /// 输入引用（2..=8 个）
        inputs: Vec<ComputeInput>,
    },
    /// 算术减（2..=8 输入，i64 依次累减）
    Sub {
        /// 输入引用（2..=8 个）
        inputs: Vec<ComputeInput>,
    },
    /// 算术乘（2..=8 输入，i64 依次累乘）
    Mul {
        /// 输入引用（2..=8 个）
        inputs: Vec<ComputeInput>,
    },
    /// 算术除（恰 2 输入，向零取整；除零/i64::MIN÷-1=节点失败）
    Div {
        /// 输入引用（恰 2 个）
        inputs: Vec<ComputeInput>,
    },
    /// 绝对值（恰 1 输入；i64::MIN 取绝对值溢出=节点失败）
    Abs {
        /// 输入引用（恰 1 个）
        inputs: Vec<ComputeInput>,
    },
    /// 最小值（2..=8 输入，i64）
    Min {
        /// 输入引用（2..=8 个）
        inputs: Vec<ComputeInput>,
    },
    /// 最大值（2..=8 输入，i64）
    Max {
        /// 输入引用（2..=8 个）
        inputs: Vec<ComputeInput>,
    },
    /// 无分隔符拼接（2..=8 输入按序原样连接）
    Concat {
        /// 输入引用（2..=8 个）
        inputs: Vec<ComputeInput>,
    },
    /// Unicode 字符（char）数（恰 1 输入；空串→"0"）
    Length {
        /// 输入引用（恰 1 个）
        inputs: Vec<ComputeInput>,
    },
    /// 转大写（恰 1 输入；Unicode 全字符集映射，如 ß→SS）
    Upper {
        /// 输入引用（恰 1 个）
        inputs: Vec<ComputeInput>,
    },
    /// 转小写（恰 1 输入）
    Lower {
        /// 输入引用（恰 1 个）
        inputs: Vec<ComputeInput>,
    },
    /// 去首尾空白（恰 1 输入；中间空白不动）
    Trim {
        /// 输入引用（恰 1 个）
        inputs: Vec<ComputeInput>,
    },
    /// 字面子串替换（恰 1 输入；find 非空加载期校验；find 不在场=原串原样）
    Replace {
        /// 输入引用（恰 1 个 = 原串）
        inputs: Vec<ComputeInput>,
        /// 被替换子串（字面匹配非正则，非空）
        find: String,
        /// 替换为该串（可为空串=删除）
        replacement: String,
    },
    /// 按分隔符分段取第 index 段（0 基；越界=节点失败）
    SplitAt {
        /// 输入引用（恰 1 个 = 原串）
        inputs: Vec<ComputeInput>,
        /// 分隔符（字面串非正则，非空）
        separator: String,
        /// 第几段（0 基）
        index: u64,
    },
    /// 按字符索引取子串（start+length 越界=节点失败，无静默截断）
    Substr {
        /// 输入引用（恰 1 个 = 原串）
        inputs: Vec<ComputeInput>,
        /// 起始位置（char 索引，0 基；start==字符数 且 length==0 合法）
        start: u64,
        /// 取多少个字符（char 数）
        length: u64,
    },
    /// 以分隔符连接（2..=8 输入；空字符串原样参与=连续分隔符属确定性语义）
    Join {
        /// 输入引用（2..=8 个）
        inputs: Vec<ComputeInput>,
        /// 连接分隔符（非空）
        separator: String,
    },
    /// Unix epoch 秒 → UTC 日期 YYYY-MM-DD（恰 1 输入；域 0000-01-01..=9999-12-31）
    EpochToDate {
        /// 输入引用（恰 1 个 = epoch 秒）
        inputs: Vec<ComputeInput>,
    },
    /// 两日期差整天数（恰 2 输入，严格 YYYY-MM-DD；inputs[1]−inputs[0]，可负）
    DateDiffDays {
        /// 输入引用（恰 2 个 = [start, end]）
        inputs: Vec<ComputeInput>,
    },
    /// epoch 秒加天数偏移（恰 1 输入 + days 可负；输出 epoch 秒非日期串）
    EpochAddDays {
        /// 输入引用（恰 1 个 = epoch 秒）
        inputs: Vec<ComputeInput>,
        /// 偏移天数（可负）
        days: i64,
    },
    /// 逻辑与（2..=8 输入，各值须在封闭布尔词表 true|false；全 true→true）
    And {
        /// 输入引用（2..=8 个）
        inputs: Vec<ComputeInput>,
    },
    /// 逻辑或（2..=8 输入；任一 true→true）
    Or {
        /// 输入引用（2..=8 个）
        inputs: Vec<ComputeInput>,
    },
    /// 逻辑非（恰 1 输入；true↔false）
    Not {
        /// 输入引用（恰 1 个）
        inputs: Vec<ComputeInput>,
    },
    /// 条件选择器（恰 3 输入：[0]=条件（布尔词表），true→[1] 原样，false→[2] 原样）
    IfElse {
        /// 输入引用（恰 3 个 = [条件, true 分支, false 分支]）
        inputs: Vec<ComputeInput>,
    },
    /// 区间截断（恰 1 输入 + min_val/max_val；min_val>max_val 加载期拒载）
    Clamp {
        /// 输入引用（恰 1 个）
        inputs: Vec<ComputeInput>,
        /// 下界（i64）
        min_val: i64,
        /// 上界（i64）
        max_val: i64,
    },
}

impl ComputeSpec {
    /// 全部输入引用（按声明序，只读取用）
    pub fn inputs(&self) -> &[ComputeInput] {
        match self {
            ComputeSpec::Strcmp { inputs, .. }
            | ComputeSpec::NumericCmp { inputs, .. }
            | ComputeSpec::RegexMatch { inputs, .. }
            | ComputeSpec::Add { inputs }
            | ComputeSpec::Sub { inputs }
            | ComputeSpec::Mul { inputs }
            | ComputeSpec::Div { inputs }
            | ComputeSpec::Abs { inputs }
            | ComputeSpec::Min { inputs }
            | ComputeSpec::Max { inputs }
            | ComputeSpec::Concat { inputs }
            | ComputeSpec::Length { inputs }
            | ComputeSpec::Upper { inputs }
            | ComputeSpec::Lower { inputs }
            | ComputeSpec::Trim { inputs }
            | ComputeSpec::Replace { inputs, .. }
            | ComputeSpec::SplitAt { inputs, .. }
            | ComputeSpec::Substr { inputs, .. }
            | ComputeSpec::Join { inputs, .. }
            | ComputeSpec::EpochToDate { inputs }
            | ComputeSpec::DateDiffDays { inputs }
            | ComputeSpec::EpochAddDays { inputs, .. }
            | ComputeSpec::And { inputs }
            | ComputeSpec::Or { inputs }
            | ComputeSpec::Not { inputs }
            | ComputeSpec::IfElse { inputs }
            | ComputeSpec::Clamp { inputs, .. } => inputs,
        }
    }
}

/// 工作流节点
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowNode {
    /// 节点 id(工作流内唯一,被 `depends_on` / `task_template` / `output_node` 引用)
    pub id: String,
    /// 执行该节点的 agent 类型(对应 `agents/<agent_type>.json`);compute 节点无 agent 语义
    pub agent_type: String,
    /// 任务描述(静态文本,与 `task_template` 二选一)
    ///
    /// 若同时提供 `task` 和 `task_template`,优先用 `task_template`。
    #[serde(default)]
    pub task: String,
    /// 任务模板(可含 `{node_id}` 占位符,被上游节点结果替换)
    ///
    /// 例:`"基于以下调研写报告:\nRust: {research_rust}\nPython: {research_python}"`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_template: Option<String>,
    /// 依赖的节点 id 列表(必须全部完成后本节点才能执行)
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// 执行条件(可选,workflow_dag v1.1):求值为假 → 跳过本节点;不写 = v1.0 现行为
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_when: Option<RunWhen>,
    /// 纯函数节点声明(可选,workflow_dag v1.2 / D-03):存在时本节点为 compute 节点,
    /// 禁止 agent_type/task/task_template(execute 层循环内同步内联求值,不走 delegate)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compute: Option<ComputeSpec>,
    /// 粒间契约 JSON Schema(可选,契约 v0):本节点产出(LLM 与 compute 节点均适用)
    /// 须为 JSON 文本且通过本 schema 校验,失败即接口失败(区别于粒失败,分类见
    /// `replan::classify_node_failure`)。与 `AgentDefinition.output_format` 分层共存:
    /// 本字段管**粒间契约**(占位符渲染输入与产出校验),output_format 管 LLM 输出
    /// 格式重试。装载期门卫见 [`validate_granule_contracts`],运行期校验钩子在
    /// [`WorkflowEngine::execute`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
    /// 节点判据声明（可选，判据 v0 第二级）：节点产出后由引擎在宿主/容器内
    /// 执行判据命令，退出码与 stdout 形态符合期望才判过——判据不过=节点失败
    /// （fail-closed：粒不过不进下一粒）。与 `compute` 互斥（compute 无 agent
    /// 产出可判，见 `Self::validate` 门卫）。判据结果以中性信号沿 PhaseGate
    /// 通路落标记会话链（acceptance_passed 同款强制注入字段，不采信 LLM 自报），
    /// 执行机械见 [`WorkflowEngine::execute`]。serde default = 全版本兼容
    /// （未声明的资产零影响；schema 对未知字段宽容，语义由本仓门卫执法）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge: Option<JudgeSpec>,
    /// 原子粒标记（可选，分类路由）：声明本节点为不可再切的原子工作粒——
    /// 粒失败且重切预算耗尽时路由判为能力缺口（终止不再拆 + 上报），见
    /// `replan::route_for_failure_class`。serde default = 全版本兼容（未声明
    /// 的资产零影响；schema 对未知字段宽容直通，语义由本仓门卫执法，
    /// judge/output_schema 同款）。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub atomic: bool,
}

/// 节点判据声明（判据 v0 第二级）
///
/// 判据命令为静态文本（v0 不做占位符替换——产出形态校验走 `output_schema`
/// （粒间契约），环境态验收走本字段，判据对象正交）。执行域：引擎持有容器
/// 名时经 docker exec 进容器执行（P1 执行桥同款宿主侧 argv）；否则宿主直
/// 执行（acceptance 门禁同款，cwd=引擎进程工作目录）。注意两域差：file 工具
/// 相对路径以沙箱根 `workspace/` 为基——判据命令引用 file 工具产物须跨域
/// 前缀（见 judge_drill 资产）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JudgeSpec {
    /// 判据命令（如 `cargo build` / `findstr <needle> <file>`）
    pub command: String,
    /// 期望退出码（缺省 0）
    #[serde(default)]
    pub expect_exit: i64,
    /// 期望 stdout 包含子串（可选；声明时 stdout 须包含该子串才判过）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_stdout: Option<String>,
}

/// 工作流定义
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    /// 工作流 id(用于日志/加载)
    pub workflow_id: String,
    /// 人类可读描述
    #[serde(default)]
    pub description: String,
    /// 节点列表
    pub nodes: Vec<WorkflowNode>,
    /// 输出节点 id(其结果作为整个工作流的返回值)
    pub output_node: String,
}

// ============================================================================
// 粒间契约(契约 v0)——节点产出的 JSON Schema 校验与装载期门卫
// ============================================================================

/// 接口失败文本标记(契约 v0):节点产出违反粒间契约时,引擎上抛的工作流失败
/// 文本携带此标记段;外层按 `replan::classify_node_failure` 将其区别于粒失败
/// (分类随 WorkflowFailureRecord.failure_class 入摘要,路由消费面在后续批次)。
pub const INTERFACE_CONTRACT_MARKER: &str = "interface contract violation";

/// 构造接口失败文本(保持 `workflow node '<id>' failed: <error>` 包裹形态,
/// 失败节点 id 可被外层 `parse_failed_node_id` 解析)
pub fn interface_contract_error(node_id: &str, detail: &str) -> String {
    format!("workflow node '{node_id}' failed: {INTERFACE_CONTRACT_MARKER}: {detail}")
}

/// 校验节点产出是否满足粒间契约 schema(契约 v0)
///
/// 产出必须为合法 JSON 文本且通过 schema 校验;`Err` 即接口失败。schema 编译
/// 失败在此按防御性错误处理(装载期门卫 [`validate_granule_contracts`] 应已拦截)。
pub fn validate_node_output_against_schema(
    schema: &serde_json::Value,
    content: &str,
) -> Result<(), String> {
    let value: serde_json::Value =
        serde_json::from_str(content).map_err(|e| format!("output is not valid JSON: {e}"))?;
    let validator = Validator::new(schema).map_err(|e| format!("schema compile failed: {e}"))?;
    if let Err(errors) = validator.validate(&value) {
        let msgs: Vec<String> = errors
            .map(|e| format!("{}: {:?}", e.instance_path, e.kind))
            .collect();
        return Err(msgs.join("; "));
    }
    Ok(())
}

/// 扫描模板中的 `{node_id}` 占位符引用(契约 v0 装载期门卫用,纯函数)
///
/// 仅内容全部为 `[A-Za-z0-9_-]` 且非空的花括号段计为引用(保序去重);
/// JSON 字面量(`{"k":1}`)、路径式访问(`{prev.X}`)、含空格段不算引用,
/// 未闭合花括号段忽略。
fn extract_template_refs(tmpl: &str) -> Vec<String> {
    let mut refs: Vec<String> = Vec::new();
    let mut rest = tmpl;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) => {
                let inner = &after[..end];
                if !inner.is_empty()
                    && inner
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                    && !refs.iter().any(|r| r == inner)
                {
                    refs.push(inner.to_string());
                }
                rest = &after[end + 1..];
            }
            None => break,
        }
    }
    refs
}

/// 粒间契约装载期门卫(契约 v0;fail-closed)
///
/// **契约态**才强制:工作流内任一节点声明了 `output_schema` 即进入契约态——
/// - 铁律一:每个 `output_schema` 必须可编译为合法 JSON Schema(非法拒载)
/// - 铁律二:契约态下,`task_template` 占位符引用的上游节点必须声明
///   `output_schema`(缺契约不上阵——下游渲染输入无契约保证即拒载)
///
/// 未声明任何 `output_schema` 的工作流零影响(存量资产不受此门卫约束)。
pub fn validate_granule_contracts(wf: &Workflow) -> Result<(), Vec<String>> {
    if !wf.nodes.iter().any(|n| n.output_schema.is_some()) {
        return Ok(());
    }
    let mut errors: Vec<String> = Vec::new();
    for node in &wf.nodes {
        if let Some(schema) = &node.output_schema {
            if let Err(e) = Validator::new(schema) {
                errors.push(format!(
                    "节点 '{}' output_schema 编译失败(契约态拒载): {e}",
                    node.id
                ));
            }
        }
    }
    let ids: HashSet<&str> = wf.nodes.iter().map(|n| n.id.as_str()).collect();
    for node in &wf.nodes {
        if let Some(tmpl) = &node.task_template {
            for r in extract_template_refs(tmpl) {
                if ids.contains(r.as_str())
                    && wf
                        .nodes
                        .iter()
                        .find(|n| n.id == r)
                        .is_some_and(|upstream| upstream.output_schema.is_none())
                {
                    errors.push(format!(
                        "契约态拒载:缺契约不上阵 —— 节点 '{}' 的模板引用 '{}',但后者未声明 output_schema",
                        node.id, r
                    ));
                }
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// M5-c:阶段前置裁决通道(②③ 载体)——标记会话 + 客户端
///
/// 引擎不承载任何协作纪律知识(哪些节点有何前置=规则层
/// 00_constraint_collab_acceptance 的事);引擎只在每个 LLM 节点 delegate
/// 前向标记会话提交中性阶段信号 `set meta_workflow.phase = <node_id>`,
/// 并按会话 version 判别 enforce 是否拦截(被拦=前置条件不满足)。
///
/// 配对时序契约(2026-09-25 修正):phase 门查 `exists(meta_task.*)` 判据,
/// 而标记由 `meta_signal.node_done` 完成信号经业务规则裁决写入。M5-b 的
/// driver 层 drain 信号在 `execute` 返回后才提交——晚于门,任何带约束规则
/// 的 workflow 第二个节点必被误拦(E2E 正例 a 实测)。故 phase_gate 激活时,
/// LLM 节点成功分支内即时打标(见 [`Self::mark_node_done`]),下一节点的
/// phase 门才可见前置标记;driver 层 drain 保留作幂等兜底(未激活形态照旧)。
#[derive(Debug, Clone)]
pub struct PhaseGate {
    /// M5-b 协作标记会话 id(meta_task.* 状态所在,即 ②③ exists 判据的求值域)
    pub marks_session: String,
    /// evorule API 客户端(信号提交 + version 感知)
    pub client: crate::api::evorule_client::EvoruleApiClient,
}

/// M5-c:协作阶段信号指令形态(纯函数)
///
/// 中性事件:`set meta_workflow.phase = <node_id>`(进入某节点阶段的信号)。
/// 前置条件的裁决完全在规则层 enforce(未尽责调不得实施/未实施不得核收)。
pub fn phase_signal(node_id: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "set",
        "params": {
            "attr": "meta_workflow.phase",
            "operation": "set",
            "value": node_id
        }
    })
}

/// 判据结果中性信号指令形态（纯函数；判据 v0 第二级）
///
/// `set meta_signal.judge = <node_id>`，params 携带 acceptance_passed 同款
/// 强制注入字段（引擎侧机制填写，不采信 LLM 自报——G8 同款字段名；失败时
/// 附 acceptance_detail）。判据过/不过的处置知识在规则层；引擎只报告事实。
pub fn judge_signal(node_id: &str, passed: bool, detail: &str) -> serde_json::Value {
    let mut params = serde_json::json!({
        "attr": "meta_signal.judge",
        "operation": "set",
        "value": node_id,
        "acceptance_passed": passed,
    });
    if !passed {
        params["acceptance_detail"] = serde_json::json!(detail);
    }
    serde_json::json!({ "type": "set", "params": params })
}

/// 判据命令执行（判据 v0 第二级；tokio 异步执行不阻塞执行器线程）
///
/// 容器在位 = `docker exec <container> sh -c <cmd>`（P1 执行桥同款宿主侧 argv，
/// 容器名过白名单校验防 argv 注入）；否则宿主直执行（acceptance 门禁同款：
/// windows=cmd /C，其余=sh -c）。spawn 失败按判据不过处理（Err 携带原因）。
async fn run_judge_command(
    cmd: &str,
    container: Option<&str>,
) -> Result<std::process::Output, String> {
    // H2 运行期闸:spawn 前白名单校验(物化期闸防 PlanFact 面;此处防手写
    // DSL v1.2 workflow json 绕过物化器校验的路径——materialize_workflow_dag
    // 也走物化期闸,此闸对直列 JudgeSpec 的调用面兜底,双闸纵深)
    crate::agent::judge_guard::validate_judge_command(cmd)
        .map_err(|e| format!("judge command blocked by H2 guard: {e}"))?;
    if let Some(c) = container {
        crate::builtin_tools::shell_exec::validate_container_name(c)
            .map_err(|e| format!("judge container name invalid: {e}"))?;
        let mut command = tokio::process::Command::new("docker");
        command.args(["exec", c, "sh", "-c", cmd]);
        return command
            .output()
            .await
            .map_err(|e| format!("judge command spawn failed: {e}"));
    }
    #[cfg(windows)]
    let mut command = {
        let mut c = tokio::process::Command::new("cmd");
        // raw_arg 直传判据串：arg() 按 MSVCRT 约定对内层引号做反斜杠转义，
        // 而 cmd 不认转义——含引号判据串经 arg() 必被搅碎（实锤：探针
        // python -c "exit(7)" 返 0）；直传让 cmd 收到与声明一致的串，
        // 引号语义交给 cmd 原生规则，与人工在终端输入完全同形
        c.raw_arg("/C").raw_arg(cmd);
        c
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut c = tokio::process::Command::new("sh");
        c.args(["-c", cmd]);
        c
    };
    command
        .output()
        .await
        .map_err(|e| format!("judge command spawn failed: {e}"))
}

/// 判据裁决（纯函数；判据 v0 第二级）
///
/// exit 码等于期望且 stdout 含期望子串（声明时）=过；`detail` 为审计文本
/// （exit/stdout 尾/stderr 尾，acceptance 门禁同形态）。spawn 失败不经本函数
/// （调用方按不过处理，Err 原文即 detail）。
fn judge_verdict(judge: &JudgeSpec, output: &std::process::Output) -> (bool, String) {
    let exit = output.status.code().map(i64::from).unwrap_or(-1);
    let passed = exit == judge.expect_exit
        && judge.expect_stdout.as_ref().map_or(true, |want| {
            String::from_utf8_lossy(&output.stdout).contains(want.as_str())
        });
    let detail = format!(
        "exit={} stdout_tail={} stderr_tail={}",
        exit,
        crate::agent::acceptance::tail_str(&String::from_utf8_lossy(&output.stdout), 400),
        crate::agent::acceptance::tail_str(&String::from_utf8_lossy(&output.stderr), 400),
    );
    (passed, detail)
}

/// run 级账本槽位(引擎自举语义:ctx.journal_dir 在场即按需建账,缺省开;
/// 恢复面据此回放,`planrun-` 前缀会话与 delegate 子会话天然区分)
#[derive(Clone)]
enum RunLedgerSlot {
    /// 未初始化:首个检查点写点按需自举(ctx.journal_dir 在场)或保持缺账
    Uninitialized,
    /// 显式关停(builder 注入;特殊场景不想落 run 账时用,自举不再发生)
    Disabled,
    /// 就绪句柄(自举产物或显式注入)
    Ready(JournalWriter),
}

impl std::fmt::Debug for RunLedgerSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Uninitialized => f.write_str("Uninitialized"),
            Self::Disabled => f.write_str("Disabled"),
            Self::Ready(_) => f.write_str("Ready(<journal writer>)"),
        }
    }
}

/// 工作流引擎
///
/// 持有 [`DelegateContext`],负责拓扑排序 + 并行执行 + 模板渲染。
#[derive(Debug, Clone)]
pub struct WorkflowEngine {
    ctx: DelegateContext,
    /// 已成功完成节点计数（外层驱动 replan 预算 `nodes_executed` 累加源；
    /// 交付物 6 §4.1，Phase 1-B 接线。compute 与 LLM 节点都计，跳过节点不计。
    /// Arc 包装保 `Clone`（克隆共享同一计数器——同一驱动循环语义）
    executed_nodes: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// 已成功完成节点 id 累积表（Phase 2：静态拦截 R8-T03 与重复执行埋点的
    /// 数据源。`take_executed_node_ids` drain 语义 = 取走自上次调用以来的
    /// 增量；外层驱动跨版本累积成「已执行注册表」。Arc 共享同一份——克隆共享）
    executed_node_ids: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// M5-c:阶段前置裁决通道(None = 既有行为零变更)
    phase_gate: Option<PhaseGate>,
    /// 判据执行容器（判据 v0 第二级；None = 宿主直执行）。serve 面经 run 请求
    /// container 字段传入（P1 执行桥同源），CLI 面恒 None（宿主语义）。
    judge_container: Option<String>,
    /// run 级账本槽位(粒级检查点载体;Arc 共享保 Clone 同驱动循环语义,
    /// Mutex 支撑 &self 执行路径上的按需自举)
    run_ledger: std::sync::Arc<std::sync::Mutex<RunLedgerSlot>>,
}

impl WorkflowEngine {
    /// 创建工作流引擎
    pub fn new(ctx: DelegateContext) -> Self {
        Self {
            ctx,
            executed_nodes: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            executed_node_ids: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            phase_gate: None,
            judge_container: None,
            run_ledger: std::sync::Arc::new(std::sync::Mutex::new(RunLedgerSlot::Uninitialized)),
        }
    }

    /// 显式注入 run 级账本句柄(外部装配/测试桩场景;注入后自举不再发生)
    pub fn with_run_journal(mut self, writer: JournalWriter) -> Self {
        *self.run_ledger.lock().unwrap_or_else(|p| p.into_inner()) = RunLedgerSlot::Ready(writer);
        self
    }

    /// 显式关停 run 级账本(即使 ctx.journal_dir 在场也不自举;既有形态
    /// 显式化——不想落 run 账的调用方用此关停)
    pub fn without_run_journal(mut self) -> Self {
        *self.run_ledger.lock().unwrap_or_else(|p| p.into_inner()) = RunLedgerSlot::Disabled;
        self
    }

    /// run 级账本会话 id(自举后可得;恢复面据此回放。None=尚未建账/已关停)
    pub fn run_ledger_session_id(&self) -> Option<String> {
        let slot = self.run_ledger.lock().unwrap_or_else(|p| p.into_inner());
        match &*slot {
            RunLedgerSlot::Ready(w) => Some(w.session_id()),
            _ => None,
        }
    }

    /// run 级账本句柄按需就绪(自举:ctx.journal_dir 在场即建
    /// `planrun-{workflow_id}-{指纹}` 会话;指纹=blake3(工作流 id+纳秒+
    /// 进程内序号+pid) 截 16 hex——并行/同毫秒启动的多个 run 不串账,
    /// 与计划锚正交成双闸:锚管版本错配,会话唯一性管跨 run 串账;
    /// 本地派生会话名不耦合服务侧活性,恢复只读本地账本即可续)。
    /// 开账失败=Err 显式上抛(账面硬义务,缺账降级只对无账域成立)。
    fn run_ledger_for(&self, workflow_id: &str) -> Result<Option<JournalWriter>, String> {
        let mut slot = self.run_ledger.lock().unwrap_or_else(|p| p.into_inner());
        if matches!(*slot, RunLedgerSlot::Uninitialized) {
            *slot = match self.ctx.journal_dir.clone() {
                Some(dir) => {
                    let sid = Self::bootstrap_session_id(workflow_id);
                    let writer = JournalWriter::open(&dir, &sid)
                        .map_err(|e| format!("run ledger open failed (session '{sid}'): {e}"))?;
                    RunLedgerSlot::Ready(writer)
                }
                None => RunLedgerSlot::Disabled,
            };
        }
        Ok(match &*slot {
            RunLedgerSlot::Ready(w) => Some(w.clone()),
            _ => None,
        })
    }

    /// 自举会话名(planrun- 前缀 + 64bit 指纹;run 身份要求唯一性而非
    /// 确定性——恢复路径按显式会话名回放)
    fn bootstrap_session_id(workflow_id: &str) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        let digest = blake3::hash(format!("{workflow_id}:{nanos}:{seq}:{pid}").as_bytes());
        format!("planrun-{}-{}", workflow_id, &digest.to_hex()[..16])
    }

    /// 生成新的 run 级账本会话名(驱动装配面用:驱动持有句柄注入引擎,
    /// 会话名与引擎自举同一套命名策略;恢复面凭会话名回放续写)
    pub fn new_run_session_id(workflow_id: &str) -> String {
        Self::bootstrap_session_id(workflow_id)
    }

    /// 计划锚:物化 Workflow canonical JSON 的 64-hex hash(与注入组锚同款
    /// 口径,同源可复算;引擎级锚=执行工件本身,外层驱动级锚=注入后
    /// PlanFact,两层各锚其所见)
    pub fn workflow_plan_hash(wf: &Workflow) -> Result<String, String> {
        let canonical = serde_json::to_string(wf).map_err(|e| e.to_string())?;
        Ok(crate::agent::driver::plan_canonical_hash(&canonical))
    }

    /// 粒级检查点落账(账先于状态转移:调用方在本方法成功后才把结果置入
    /// results/skipped——账写不掉=Err 上抛中止,节点保持未完成态=安全重跑
    /// 方向,宁可重跑不可错续)。无账域(未注入且 ctx 无 journal_dir)=零
    /// 账直通,既有形态零变更。
    fn checkpoint_node(
        &self,
        wf: &Workflow,
        node_id: &str,
        status: &str,
        result: &str,
    ) -> Result<(), String> {
        let Some(journal) = self.run_ledger_for(&wf.workflow_id)? else {
            return Ok(());
        };
        let plan_hash = Self::workflow_plan_hash(wf)?;
        journal
            .node_checkpointed(&wf.workflow_id, &plan_hash, node_id, status, result)
            .map_err(|e| format!("workflow checkpoint write failed (node '{node_id}'): {e}"))?;
        Ok(())
    }

    /// M5-c:注入阶段前置裁决通道(驱动层按 marks_session 组装;未注入 =
    /// 节点执行零额外链上往返,行为与 M5-b 前完全一致)
    pub fn with_phase_gate(mut self, gate: PhaseGate) -> Self {
        self.phase_gate = Some(gate);
        self
    }

    /// 注入判据执行容器（判据 v0 第二级；None = 宿主直执行，既有行为零变更）
    pub fn with_judge_container(mut self, container: Option<String>) -> Self {
        self.judge_container = container;
        self
    }

    /// M5-c:LLM 节点成功后即时向标记会话提交完成信号并等待标记落链
    /// (version 感知;时序契约见 [`PhaseGate`] 文档)
    ///
    /// 信号形态:`set meta_signal.node_done = <node_id>`(中性事件,复用
    /// driver 的 [`crate::agent::driver::node_done_signal`],标记知识在规则层
    /// collab_task_marks 的 branch 壳)。branch 为放行型转换——version 推进
    /// 即标记已 stable 落链,下一节点 phase 门可见。
    ///
    /// 失败语义:通道故障 = Err fail-fast(留痕是硬义务,同 M5-b drain 纪律);
    /// 被规则层拒绝(Ok(false),当前规则面无此形态)= Err 显式报错——静默会
    /// 退化为下一节点 phase 门的误导性拦截(归因失真)。
    async fn mark_node_done(gate: &PhaseGate, node_id: &str) -> Result<(), String> {
        let allowed = crate::agent::runner::submit_signal_and_await_verdict(
            &gate.client,
            &gate.marks_session,
            &crate::agent::driver::node_done_signal(node_id),
        )
        .await
        .map_err(|e| format!("workflow mark signal failed (node '{node_id}'): {e}"))?;
        if !allowed {
            return Err(format!(
                "workflow mark signal rejected by rule layer (node '{node_id}') \
                 - task mark not recorded"
            ));
        }
        Ok(())
    }

    /// 累计已成功完成的节点数（跨多次 `execute` 调用累加；外层驱动 replan
    /// 循环据此维护 `BudgetCounters.nodes_executed`，交付物 6 §4.1）
    pub fn executed_nodes(&self) -> u64 {
        self.executed_nodes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 取走自上次调用以来成功完成的节点 id（drain 语义 = 每版增量；
    /// 外层驱动跨 replan 版本累积成已执行注册表，供 R8-T03 静态拦截比对）
    pub fn take_executed_node_ids(&self) -> Vec<String> {
        let mut guard = self
            .executed_node_ids
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        std::mem::take(&mut *guard)
    }

    /// 记录一个成功完成节点（内部辅助：compute 与 LLM 成功点各调一次）
    fn record_executed_node(&self, id: &str) {
        let mut guard = self
            .executed_node_ids
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        guard.push(id.to_string());
    }

    /// 只读访问内部 `DelegateContext`
    pub fn delegate_context(&self) -> &DelegateContext {
        &self.ctx
    }

    /// 执行工作流
    ///
    /// # 算法
    ///
    /// 1. 校验(`output_node` 存在、无重复 id、依赖合法、无环、`run_when` 引用合法)
    /// 2. 拓扑排序成分层结构
    /// 3. 逐层规划:按 `run_when` / 级联规则决定本层执行与跳过(见 [`plan_layer`])
    /// 4. 本层待执行节点并行执行(`delegate_parallel`)
    /// 5. 上游结果填入下游 `task_template`(被跳过的上游占位符替换为空字符串)
    /// 6. 返回 `output_node` 的结果
    ///
    /// # 错误
    ///
    /// - `output_node` 不存在
    /// - 节点 id 重复
    /// - `depends_on` 引用不存在的节点
    /// - 自依赖
    /// - 检测到环
    /// - `run_when` 引用不存在节点 / 自身 / 同层或下游节点
    /// - 任一**执行中**节点失败(终止整个工作流;被跳过的节点不算失败)
    /// - `output_node` 被跳过(无静默空结果)
    pub async fn execute(&self, wf: &Workflow) -> Result<String, String> {
        self.execute_with_resume(wf, NodeCheckpointReplay::default())
            .await
    }

    /// 执行工作流(可恢复入口):`resumed` = run 级账本检查点的回放重建
    /// (已完成粒零重执行、已跳过粒不再复判;空种子 = 全量跑,与 [`Self::execute`]
    /// 等价)。检查点经 [`Self::checkpoint_node`] 账先于状态落账。
    pub async fn execute_with_resume(
        &self,
        wf: &Workflow,
        resumed: NodeCheckpointReplay,
    ) -> Result<String, String> {
        // 1. 校验
        self.validate(wf)?;

        // 2. 拓扑排序
        let layers = self.topological_sort(&wf.nodes)?;

        // 2.5 run_when 上游层检查:被观察节点必须位于更早拓扑层,
        //     保证条件求值时其结果已就绪(同层并行节点的结果在规划期不可得)
        Self::check_run_when_layers(wf, &layers)?;

        // 2.6 compute inputs 层序检查(交付物 4 §3.4,复用同族校验):
        //     输入仅可引用更早拓扑层节点,自引用/同层/下游引用一律拒绝
        Self::check_compute_layers(wf, &layers)?;

        // 3. 逐层规划 + 执行(种子态 = 恢复回放重建,空种子 = 全量跑)
        let mut results: BTreeMap<String, String> = resumed.results;
        let mut skipped: HashSet<String> = resumed.skipped;
        for (layer_idx, layer) in layers.iter().enumerate() {
            let (mut to_run, newly_skipped) = plan_layer(layer, &results, &skipped);
            // 恢复面:已完成粒零重执行——种子 results 已含的节点本层直接跳过
            //(其结果原样供下游模板渲染;不重复计数不重复落账——崩溃前进程
            // 已留账,恢复进程的计数器/注册表增量只含本进程新执行粒)
            let restored: Vec<String> = to_run
                .iter()
                .filter(|n| results.contains_key(&n.id))
                .map(|n| n.id.clone())
                .collect();
            to_run.retain(|n| !results.contains_key(&n.id));
            for id in &restored {
                tracing::info!(
                    workflow_id = %wf.workflow_id,
                    node_id = %id,
                    "resume: node restored from checkpoint, skipping re-execution"
                );
            }
            for id in &newly_skipped {
                tracing::info!(
                    workflow_id = %wf.workflow_id,
                    layer = layer_idx,
                    node_id = %id,
                    "workflow node skipped"
                );
            }
            // 检查点(跳过):账先于状态;种子已含的跳过不重复落账
            for id in &newly_skipped {
                if !skipped.contains(id) {
                    self.checkpoint_node(wf, id, "skipped", "")?;
                }
            }
            skipped.extend(newly_skipped);

            if to_run.is_empty() {
                continue;
            }

            // compute 节点同步内联求值(交付物 4 §5.1,不经 delegate:不占并发槽、
            // 不耗 max_depth、无 IoRequest;同层按层内序依次求值,纯函数微秒级)
            // LLM/工具节点照常并行(delegate_parallel,零改动)
            let mut llm_nodes: Vec<&WorkflowNode> = Vec::with_capacity(to_run.len());
            for node in to_run {
                match &node.compute {
                    Some(spec) => {
                        let output = eval_compute(spec, &results)
                            .map_err(|e| format!("workflow node '{}' failed: {}", node.id, e))?;
                        // 粒间契约(契约 v0):产出须过 schema,失败=接口失败(fail-fast,
                        // 不计 executed_nodes、不进 results)
                        if let Some(schema) = &node.output_schema {
                            if let Err(d) = validate_node_output_against_schema(schema, &output) {
                                return Err(interface_contract_error(&node.id, &d));
                            }
                        }
                        tracing::info!(
                            workflow_id = %wf.workflow_id,
                            layer = layer_idx,
                            node_id = %node.id,
                            content_len = output.len(),
                            "workflow compute node evaluated"
                        );
                        // 账先于状态:检查点落账成功后才置入 results
                        self.checkpoint_node(wf, &node.id, "completed", &output)?;
                        results.insert(node.id.clone(), output);
                        self.executed_nodes
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        self.record_executed_node(&node.id);
                    }
                    None => llm_nodes.push(node),
                }
            }

            if llm_nodes.is_empty() {
                continue;
            }

            // M5-c:阶段前置裁决(②③ 载体)——LLM 节点 delegate 前向标记会话
            // 提交阶段信号 `set meta_workflow.phase = <node_id>`,由规则层
            // 00_constraint_collab_acceptance enforce 裁决前置条件(未尽责调
            // 不得实施/未实施不得核收)。感知通道故障=fail-fast(裁决不可信);
            // 被拦=前置条件不满足,错误文本带 `enforce violation:` 前缀——
            // 外层驱动 D-01 判别终止整个循环且不 replan。
            // 配对:节点成功后 mark_node_done 即时打标(见 PhaseGate 时序契约),
            // 保证下一节点的门可查到前置标记。
            if let Some(gate) = &self.phase_gate {
                for node in &llm_nodes {
                    let allowed = crate::agent::runner::submit_signal_and_await_verdict(
                        &gate.client,
                        &gate.marks_session,
                        &phase_signal(&node.id),
                    )
                    .await
                    .map_err(|e| {
                        format!("workflow phase signal failed (node '{}'): {}", node.id, e)
                    })?;
                    if !allowed {
                        return Err(format!(
                            "enforce violation: workflow phase '{}' rejected by collab \
                             acceptance rule (prerequisite task mark missing; see marks \
                             session audit for Violation attribution)",
                            node.id
                        ));
                    }
                }
            }

            let tasks: Vec<(String, String)> = llm_nodes
                .iter()
                .map(|node| {
                    let task = self.render_task(node, &results, &skipped);
                    (node.agent_type.clone(), task)
                })
                .collect();

            tracing::info!(
                workflow_id = %wf.workflow_id,
                layer = layer_idx,
                node_count = llm_nodes.len(),
                "executing workflow layer"
            );

            let layer_results = self.ctx.delegate_parallel(tasks).await;

            for (node, result) in llm_nodes.iter().zip(layer_results.iter()) {
                match result {
                    Ok(content) => {
                        // 粒间契约(契约 v0):产出须过 schema,失败=接口失败(fail-fast,
                        // 不计 executed_nodes、不进 results、不打成功标)
                        if let Some(schema) = &node.output_schema {
                            if let Err(d) = validate_node_output_against_schema(schema, content) {
                                return Err(interface_contract_error(&node.id, &d));
                            }
                        }
                        // 判据 v0 第二级(JudgeNode):环境态验收命令——exit 码/stdout
                        // 形态判过(不采信 LLM 自报,G8 同款)。判据结果=中性信号沿
                        // PhaseGate 通路落标记会话链(journal 可查账;None = 无链路面,
                        // 门禁照常执行);判据不过=节点失败(fail-closed:粒不过不进
                        // 下一粒,失败文本无接口契约标记→外层分类为粒失败)。通道
                        // 故障=fail-fast;被 enforce 拦截=D-01 语义(外层不 replan)。
                        if let Some(judge) = &node.judge {
                            let (passed, detail) =
                                run_judge_command(&judge.command, self.judge_container.as_deref())
                                    .await
                                    .map(|output| judge_verdict(judge, &output))
                                    .unwrap_or_else(|e| (false, e));
                            if let Some(gate) = &self.phase_gate {
                                let allowed =
                                    crate::agent::runner::submit_signal_and_await_verdict(
                                        &gate.client,
                                        &gate.marks_session,
                                        &judge_signal(&node.id, passed, &detail),
                                    )
                                    .await
                                    .map_err(|e| {
                                        format!(
                                            "workflow judge signal failed (node '{}'): {}",
                                            node.id, e
                                        )
                                    })?;
                                if !allowed {
                                    return Err(format!(
                                        "enforce violation: workflow judge signal rejected by \
                                         rule layer (node '{}', passed={passed}; {detail})",
                                        node.id
                                    ));
                                }
                            }
                            if !passed {
                                return Err(format!(
                                    "workflow node '{}' failed: judge failed ({})",
                                    node.id, detail
                                ));
                            }
                            tracing::info!(
                                workflow_id = %wf.workflow_id,
                                node_id = %node.id,
                                "workflow node judge passed"
                            );
                        }
                        tracing::info!(
                            workflow_id = %wf.workflow_id,
                            node_id = %node.id,
                            content_len = content.len(),
                            "workflow node succeeded"
                        );
                        // 账先于状态:检查点落账成功后才置入 results
                        self.checkpoint_node(wf, &node.id, "completed", content)?;
                        results.insert(node.id.clone(), content.clone());
                        self.executed_nodes
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        self.record_executed_node(&node.id);
                        // M5-c:成功即时打标(version 感知等待标记落链)——
                        // 下一层/下一节点的 phase 前置门依赖此标记存在。
                        // driver 层 drain 保留(幂等),此处 fail-fast 同纪律。
                        if let Some(gate) = &self.phase_gate {
                            Self::mark_node_done(gate, &node.id).await?;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            workflow_id = %wf.workflow_id,
                            node_id = %node.id,
                            error = %e,
                            "workflow node failed, aborting workflow"
                        );
                        return Err(format!("workflow node '{}' failed: {}", node.id, e));
                    }
                }
            }
        }

        // 4. 返回 output_node 结果
        results
            .get(&wf.output_node)
            .cloned()
            .ok_or_else(|| format!("output node '{}' produced no result", wf.output_node))
    }

    /// 校验工作流定义(不检查环,环在拓扑排序阶段检测)
    fn validate(&self, wf: &Workflow) -> Result<(), String> {
        if wf.nodes.is_empty() {
            return Err(format!("workflow '{}' has no nodes", wf.workflow_id));
        }

        let mut id_set: HashSet<&str> = HashSet::new();
        for n in &wf.nodes {
            // 门卫(P2-M7 前置补丁):node id 必须是标识符 [A-Za-z0-9_-]+——
            // id 会被拼进 task_template 占位符 `{id}`,含 {} / 空格等字符会
            // 污染模板机制;强制白名单消除该隐性边角。
            if n.id.is_empty()
                || !n
                    .id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return Err(format!(
                    "workflow '{}': node id '{}' is not a valid identifier ([A-Za-z0-9_-]+)",
                    wf.workflow_id, n.id
                ));
            }
            // 门卫(路径穿越防护):agent_type 最终会进入 AgentDefinition 加载器,
            // 此处提前拦截并给出工作流上下文的可读错误。
            // compute 节点无 agent 语义(D-03:不经 delegate),豁免本门卫。
            if n.compute.is_none() {
                if let Err(e) =
                    crate::agent::definition::AgentDefinition::validate_agent_type(&n.agent_type)
                {
                    return Err(format!("workflow '{}': {}", wf.workflow_id, e));
                }
            }
            if !id_set.insert(n.id.as_str()) {
                return Err(format!(
                    "workflow '{}' has duplicate node id: '{}'",
                    wf.workflow_id, n.id
                ));
            }
            // 门卫(判据 v0):judge 与 compute 互斥——compute 节点无 agent 产出可判
            // (D-03:纯函数求值不经 delegate),judge 面向 LLM 粒产出的环境态验收
            if n.compute.is_some() && n.judge.is_some() {
                return Err(format!(
                    "workflow '{}': node '{}' cannot declare both compute and judge",
                    wf.workflow_id, n.id
                ));
            }
        }

        // output_node 必须存在
        if !id_set.contains(wf.output_node.as_str()) {
            return Err(format!(
                "workflow '{}' output_node '{}' not found in nodes",
                wf.workflow_id, wf.output_node
            ));
        }

        // 依赖合法性 + 自依赖 + run_when 引用合法性
        for n in &wf.nodes {
            for dep in &n.depends_on {
                if dep == &n.id {
                    return Err(format!(
                        "workflow '{}' node '{}' depends on itself",
                        wf.workflow_id, n.id
                    ));
                }
                if !id_set.contains(dep.as_str()) {
                    return Err(format!(
                        "workflow '{}' node '{}' depends on unknown node '{}'",
                        wf.workflow_id, n.id, dep
                    ));
                }
            }
            // run_when 引用合法性(层序检查在拓扑分层后进行,见 check_run_when_layers)
            if let Some(cond) = &n.run_when {
                if cond.node == n.id {
                    return Err(format!(
                        "workflow '{}' node '{}' run_when references itself",
                        wf.workflow_id, n.id
                    ));
                }
                // 空观察源豁免(交付物 5 注记一·3):物化器把 iter0 的 prev.X 观察
                // 消解为空观察源(node=""),执行期求值 results.get("") → None → 空串,
                // 与既有「被观察节点无结果 → 空串」语义天然一致;手写 DSL 路径
                // schema pattern 本就拒空串,不受影响。
                if cond.node.is_empty() {
                    continue;
                }
                if !id_set.contains(cond.node.as_str()) {
                    return Err(format!(
                        "workflow '{}' node '{}' run_when references unknown node '{}'",
                        wf.workflow_id, n.id, cond.node
                    ));
                }
            }
            // compute 形态校验(封闭目录代码层校验,交付物 4 §4.6 双保险):
            // 输入数量/threshold 互斥/pattern 编译/inputs 引用存在性
            if let Some(spec) = &n.compute {
                validate_compute_spec(&wf.workflow_id, &n.id, spec, &id_set)?;
            }
        }

        Ok(())
    }

    /// 渲染 `task_template`:把 `{node_id}` 替换为上游结果
    ///
    /// 无 `task_template` 时返回 `node.task`。上游结果未就绪时占位符保留
    /// (但拓扑排序保证执行时上游已完成,不会出现未就绪)。
    /// 被跳过的上游节点无结果:占位符替换为**空字符串**(可预测,不会把
    /// `{id}` 字面量漏进下游任务文本)。
    fn render_task(
        &self,
        node: &WorkflowNode,
        results: &BTreeMap<String, String>,
        skipped: &HashSet<String>,
    ) -> String {
        if let Some(tmpl) = &node.task_template {
            let mut task = tmpl.clone();
            for (id, content) in results {
                task = task.replace(&format!("{{{}}}", id), content);
            }
            for id in skipped {
                task = task.replace(&format!("{{{}}}", id), "");
            }
            task
        } else {
            node.task.clone()
        }
    }

    /// run_when 上游层检查:被观察节点必须位于本节点的**更早拓扑层**
    ///
    /// Kahn 分层保证更早层的节点在本层规划前已完成;同层并行节点的结果在
    /// 规划期不可得,下游节点同理。引用同层/下游节点几乎必然是定义错误,
    /// 在执行前 fail-fast。validate 已保证引用节点存在,此处查不到层视为内部错误。
    fn check_run_when_layers(wf: &Workflow, layers: &[Vec<&WorkflowNode>]) -> Result<(), String> {
        let mut layer_of: HashMap<&str, usize> = HashMap::new();
        for (i, layer) in layers.iter().enumerate() {
            for n in layer {
                layer_of.insert(n.id.as_str(), i);
            }
        }
        for node in &wf.nodes {
            if let Some(cond) = &node.run_when {
                // 空观察源豁免(交付物 5 注记一·3,同 validate):iter0 空观察源
                // 天然早于一切层,无需层序检查
                if cond.node.is_empty() {
                    continue;
                }
                let target = layer_of.get(cond.node.as_str()).copied().ok_or_else(|| {
                    format!(
                        "workflow '{}': node '{}' run_when references unknown node '{}'",
                        wf.workflow_id, node.id, cond.node
                    )
                })?;
                let own = layer_of
                    .get(node.id.as_str())
                    .copied()
                    .unwrap_or(usize::MAX);
                if target >= own {
                    return Err(format!(
                        "workflow '{}': node '{}' run_when must reference an upstream node \
                         from an earlier layer ('{}' is at same-or-later layer)",
                        wf.workflow_id, node.id, cond.node
                    ));
                }
            }
        }
        Ok(())
    }

    /// compute inputs 上游层检查(交付物 4 §3.4):输入引用必须位于本节点的
    /// **更早拓扑层**——保证求值时输入已就绪;与 `check_run_when_layers` 同族。
    /// validate 已保证引用存在,此处查不到层视为内部错误。
    fn check_compute_layers(wf: &Workflow, layers: &[Vec<&WorkflowNode>]) -> Result<(), String> {
        let mut layer_of: HashMap<&str, usize> = HashMap::new();
        for (i, layer) in layers.iter().enumerate() {
            for n in layer {
                layer_of.insert(n.id.as_str(), i);
            }
        }
        for node in &wf.nodes {
            let Some(spec) = &node.compute else {
                continue;
            };
            for input in spec.inputs() {
                let ComputeInput::Node(name) = input else {
                    continue; // Empty 为物化产物空输入字面量,无层序概念
                };
                let target = layer_of.get(name.as_str()).copied().ok_or_else(|| {
                    format!(
                        "workflow '{}': compute node '{}' references unknown node '{}'",
                        wf.workflow_id, node.id, name
                    )
                })?;
                let own = layer_of
                    .get(node.id.as_str())
                    .copied()
                    .unwrap_or(usize::MAX);
                if target >= own {
                    return Err(format!(
                        "workflow '{}': compute node '{}' input '{}' must reference an \
                         upstream node from an earlier layer",
                        wf.workflow_id, node.id, name
                    ));
                }
            }
        }
        Ok(())
    }

    /// 拓扑排序:返回按依赖层级分组的节点列表
    ///
    /// 同层节点无互相依赖,可并行执行。检测到环时返回 Err。
    ///
    /// 算法:Kahn 分层 —— 反复选出"所有依赖都已处理"的节点作为下一层,
    /// 直到全部处理完;若中途无进展但仍有未处理节点,说明存在环。
    fn topological_sort<'a>(
        &self,
        nodes: &'a [WorkflowNode],
    ) -> Result<Vec<Vec<&'a WorkflowNode>>, String> {
        let mut processed: HashSet<&str> = HashSet::new();
        let mut layers: Vec<Vec<&WorkflowNode>> = Vec::new();

        loop {
            // 选出:未处理 + 所有依赖均已处理
            let layer: Vec<&WorkflowNode> = nodes
                .iter()
                .filter(|n| !processed.contains(n.id.as_str()))
                .filter(|n| n.depends_on.iter().all(|d| processed.contains(d.as_str())))
                .collect();

            if layer.is_empty() {
                break;
            }

            for n in &layer {
                processed.insert(n.id.as_str());
            }
            layers.push(layer);
        }

        // 环检测:若有未处理节点,说明它们互相依赖成环
        if processed.len() != nodes.len() {
            let cycle_nodes: Vec<&str> = nodes
                .iter()
                .map(|n| n.id.as_str())
                .filter(|id| !processed.contains(*id))
                .collect();
            return Err(format!(
                "cycle detected in workflow; nodes involved: {:?}",
                cycle_nodes
            ));
        }

        Ok(layers)
    }
}

/// 条件求值(纯函数):同输入必同输出
///
/// 被观察节点无结果(被跳过/未完成)时视作**空字符串**——这是跳过语义的
/// 确定性根基:同一执行轨迹下,条件结果只依赖已完成结果表的内容。
fn eval_condition(cond: &RunWhen, results: &BTreeMap<String, String>) -> bool {
    let actual = results.get(&cond.node).map(String::as_str).unwrap_or("");
    match cond.op {
        ConditionOp::Contains => actual.contains(cond.value.as_str()),
        ConditionOp::Equals => actual == cond.value,
        ConditionOp::NotContains => !actual.contains(cond.value.as_str()),
    }
}

/// compute 输入解析(纯函数):Node 引用读 results 表——被引用节点被跳过/尚无结果
/// 时视作**空字符串**(与 run_when 既有语义逐字一致,交付物 4 §3.3);Empty 为
/// 物化器产物(iter0 的 prev.X 消解),执行期即空输入字面量
fn resolve_compute_input(input: &ComputeInput, results: &BTreeMap<String, String>) -> String {
    match input {
        ComputeInput::Node(name) => results
            .get(name)
            .map(String::as_str)
            .unwrap_or("")
            .to_string(),
        ComputeInput::Empty => String::new(),
    }
}

// ----- v1.3 compute 辅助（全部纯函数，确定性：同输入必同输出） -----

/// 日期域 0000-01-01..=9999-12-31 的天数边界（days since 1970-01-01，UTC）
const DATE_DOMAIN_MIN_DAYS: i64 = -719_528; // 0000-01-01T00:00:00Z = -62167219200 秒
const DATE_DOMAIN_MAX_DAYS: i64 = 2_932_896; // 9999-12-31T00:00:00Z

/// i64 输入解析（十进制整数；解析失败 = 节点失败 = 工作流终止）
fn parse_i64_input(func: &str, s: &str) -> Result<i64, String> {
    s.trim()
        .parse::<i64>()
        .map_err(|_| format!("{func} 输入 '{s}' 整数解析失败(节点失败,无静默回退)"))
}

/// 布尔词表解析（封闭 true|false；不隐式真值化，词表外 = 节点失败；
/// 首尾空白容忍——真实 LLM 输出常带空白噪声，trim 后仍须精确匹配）
fn parse_bool_vocab(func: &str, s: &str) -> Result<bool, String> {
    match s.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!(
            "{func} 条件值 '{s}' 不在布尔词表 true|false(节点失败,无静默回退)"
        )),
    }
}

/// civil_from_days（Howard Hinnant 算法）：days since 1970-01-01 → (年, 月, 日)
/// （proleptic Gregorian，UTC。evo-agent 无 chrono 依赖，手写实现保证确定性）
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]（u64）
    (if m <= 2 { y + 1 } else { y }, m as u32, d)
}

/// days_from_civil（Howard Hinnant 算法）：(年, 月, 日) → days since 1970-01-01
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let mp = u64::from(if m > 2 { m - 3 } else { m + 9 }); // [0, 11]
    let doy = (153 * mp + 2) / 5 + u64::from(d) - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe as i64 - 719_468
}

/// 严格 YYYY-MM-DD 解析（4 位零填充年份，proleptic Gregorian；形态不符=节点失败）
/// 域校验（0000..=9999）由 4 位年份形态保证；月日按闰年规则校验；
/// 首尾空白容忍——真实 LLM 输出常带空白噪声，trim 后仍须严格形态（拒绝消息保留原文）
fn parse_date_str(func: &str, s: &str) -> Result<i64, String> {
    let reject = || format!("{func} 日期 '{s}' 形态不符(须严格 YYYY-MM-DD)(节点失败,无静默回退)");
    let parts: Vec<&str> = s.trim().split('-').collect();
    if parts.len() != 3 {
        return Err(reject());
    }
    let (y, m, d) = (parts[0], parts[1], parts[2]);
    let dd = |p: &str| p.len() == 2 && p.bytes().all(|b| b.is_ascii_digit());
    if y.len() != 4 || !y.bytes().all(|b| b.is_ascii_digit()) || !dd(m) || !dd(d) {
        return Err(reject());
    }
    let y: i64 = y.parse().map_err(|_| reject())?;
    let m: u32 = m.parse().map_err(|_| reject())?;
    let d: u32 = d.parse().map_err(|_| reject())?;
    if !(1..=12).contains(&m) {
        return Err(reject());
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days_in_month = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ][(m - 1) as usize];
    if d < 1 || d > days_in_month {
        return Err(reject());
    }
    Ok(days_from_civil(y, m, d))
}

/// i64 多输入折叠求值（add/sub/mul/min/max 共用；数量 2..=8，checked 溢出=节点失败）
fn eval_i64_fold(
    func: &str,
    resolved: &[String],
    op: fn(i64, i64) -> Option<i64>,
) -> Result<String, String> {
    if resolved.len() < 2 || resolved.len() > 8 {
        return Err(format!("{func} 需 2..=8 输入,实得 {}", resolved.len()));
    }
    let mut acc = parse_i64_input(func, &resolved[0])?;
    for v in &resolved[1..] {
        let v = parse_i64_input(func, v)?;
        acc = op(acc, v).ok_or_else(|| format!("{func} 整数溢出(节点失败,无静默回退)"))?;
    }
    Ok(acc.to_string())
}

/// compute 纯函数求值(封闭目录 27 函数：v1.2 三种 + v1.3 新增 24 种;同输入必同输出)
///
/// 输入数量/常量参数合法性已在 validate 期拦截,此处兜底防御(返回 Err 而非 panic);
/// 错误语义总纲(v1.3 全目录统一,沿用 v1.2 numeric_cmp 既有口径):数值/整数解析失败、
/// 除零、整数溢出、索引越界、日期域外、条件值不在词表、分隔符或查找串为空
/// = 节点失败 = 工作流终止(无静默回退)。
/// 结果词表总表(v1.3):strcmp→equal|different / contained|not_contained;
/// numeric_cmp→true|false;regex_match→match|no_match;and/or/not→true|false;
/// 算术族/length/date_diff_days/epoch_add_days→十进制整数字符串(可带负号);
/// epoch_to_date→YYYY-MM-DD;concat/join/upper/lower/trim/replace/split_at/
/// substr/if_else→原样字符串(无词表)。
fn eval_compute(spec: &ComputeSpec, results: &BTreeMap<String, String>) -> Result<String, String> {
    let resolved: Vec<String> = spec
        .inputs()
        .iter()
        .map(|i| resolve_compute_input(i, results))
        .collect();
    // 恰 1 输入形态的兜底解析(单输入函数共用)
    let one = |func: &str| -> Result<&str, String> {
        match resolved.as_slice() {
            [a] => Ok(a.as_str()),
            other => Err(format!("{func} 需恰 1 输入,实得 {}", other.len())),
        }
    };
    // 2..=8 输入形态的数量兜底(多输入函数共用)
    let multi = |func: &str| -> Result<(), String> {
        if resolved.len() < 2 || resolved.len() > 8 {
            return Err(format!("{func} 需 2..=8 输入,实得 {}", resolved.len()));
        }
        Ok(())
    };
    match spec {
        // ===== v1.2 三函数（词表沿用） =====
        ComputeSpec::Strcmp { mode, .. } => {
            let [a, b] = resolved.as_slice() else {
                return Err(format!("strcmp 需恰 2 输入,实得 {}", resolved.len()));
            };
            Ok(match mode {
                StrcmpMode::Equal => {
                    if a == b {
                        "equal"
                    } else {
                        "different"
                    }
                }
                StrcmpMode::Contains => {
                    if a.contains(b.as_str()) {
                        "contained"
                    } else {
                        "not_contained"
                    }
                }
            }
            .to_string())
        }
        ComputeSpec::NumericCmp {
            mode, threshold, ..
        } => {
            let parse = |s: &str| {
                s.trim().parse::<f64>().map_err(|_| {
                    format!("numeric_cmp 输入 '{s}' 数值解析失败(节点失败,无静默回退)")
                })
            };
            let (result, _) = match resolved.as_slice() {
                [a, b] => {
                    if threshold.is_some() {
                        return Err("numeric_cmp 双输入形态禁止携带 threshold".to_string());
                    }
                    let (x, y) = (parse(a)?, parse(b)?);
                    (
                        match mode {
                            NumericCmpMode::Lt => x < y,
                            NumericCmpMode::Le => x <= y,
                            NumericCmpMode::Gt => x > y,
                            NumericCmpMode::Ge => x >= y,
                            NumericCmpMode::Eq => x == y,
                        },
                        y,
                    )
                }
                [a] => {
                    let t = threshold
                        .ok_or_else(|| "numeric_cmp 单输入形态必带 threshold".to_string())?;
                    let x = parse(a)?;
                    (
                        match mode {
                            NumericCmpMode::Lt => x < t,
                            NumericCmpMode::Le => x <= t,
                            NumericCmpMode::Gt => x > t,
                            NumericCmpMode::Ge => x >= t,
                            NumericCmpMode::Eq => x == t,
                        },
                        t,
                    )
                }
                other => return Err(format!("numeric_cmp 需 1..=2 输入,实得 {}", other.len())),
            };
            Ok(if result { "true" } else { "false" }.to_string())
        }
        ComputeSpec::RegexMatch { pattern, .. } => {
            let [a] = resolved.as_slice() else {
                return Err(format!("regex_match 需恰 1 输入,实得 {}", resolved.len()));
            };
            // pattern 合法性加载期已校验(validate_compute_spec),此处编译失败兜底
            let re = regex::Regex::new(pattern)
                .map_err(|e| format!("regex pattern '{pattern}' 编译失败: {e}"))?;
            Ok(if re.is_match(a) { "match" } else { "no_match" }.to_string())
        }
        // ===== v1.3 算术（i64 域，checked 溢出=节点失败） =====
        ComputeSpec::Add { .. } => eval_i64_fold("add", &resolved, i64::checked_add),
        ComputeSpec::Sub { .. } => eval_i64_fold("sub", &resolved, i64::checked_sub),
        ComputeSpec::Mul { .. } => eval_i64_fold("mul", &resolved, i64::checked_mul),
        ComputeSpec::Min { .. } => eval_i64_fold("min", &resolved, |a, b| Some(a.min(b))),
        ComputeSpec::Max { .. } => eval_i64_fold("max", &resolved, |a, b| Some(a.max(b))),
        ComputeSpec::Div { .. } => {
            let [a, b] = resolved.as_slice() else {
                return Err(format!("div 需恰 2 输入,实得 {}", resolved.len()));
            };
            let (x, y) = (parse_i64_input("div", a)?, parse_i64_input("div", b)?);
            // checked_div 同时覆盖除零与 i64::MIN ÷ -1 两类失败
            let r = x.checked_div(y).ok_or_else(|| {
                if y == 0 {
                    "div 除数为 0(节点失败,无静默回退)".to_string()
                } else {
                    "div 溢出(i64::MIN ÷ -1)(节点失败,无静默回退)".to_string()
                }
            })?;
            Ok(r.to_string())
        }
        ComputeSpec::Abs { .. } => {
            let x = parse_i64_input("abs", one("abs")?)?;
            let r = x
                .checked_abs()
                .ok_or_else(|| "abs 溢出(i64::MIN 取绝对值)(节点失败,无静默回退)".to_string())?;
            Ok(r.to_string())
        }
        // ===== v1.3 字符串（Unicode char 计数/索引，字面操作非正则） =====
        ComputeSpec::Concat { .. } => {
            multi("concat")?;
            Ok(resolved.concat())
        }
        ComputeSpec::Length { .. } => Ok(one("length")?.chars().count().to_string()),
        ComputeSpec::Upper { .. } => Ok(one("upper")?.to_uppercase()),
        ComputeSpec::Lower { .. } => Ok(one("lower")?.to_lowercase()),
        ComputeSpec::Trim { .. } => Ok(one("trim")?.trim().to_string()),
        ComputeSpec::Replace {
            find, replacement, ..
        } => {
            let a = one("replace")?;
            if find.is_empty() {
                return Err("replace 查找串为空(节点失败,无静默回退)".to_string());
            }
            Ok(a.replace(find.as_str(), replacement))
        }
        ComputeSpec::SplitAt {
            separator, index, ..
        } => {
            let a = one("split_at")?;
            if separator.is_empty() {
                return Err("split_at 分隔符为空(节点失败,无静默回退)".to_string());
            }
            let idx = usize::try_from(*index)
                .map_err(|_| format!("split_at 段索引 {index} 越界(节点失败,无静默回退)"))?;
            a.split(separator.as_str())
                .nth(idx)
                .map(str::to_string)
                .ok_or_else(|| format!("split_at 段索引 {index} 越界(节点失败,无静默回退)"))
        }
        ComputeSpec::Substr { start, length, .. } => {
            let a = one("substr")?;
            let chars: Vec<char> = a.chars().collect();
            let count = chars.len() as u64;
            if *start > count {
                return Err(format!(
                    "substr 起始 {start} 越界(字符数 {count})(节点失败,无静默回退)"
                ));
            }
            let end = start.checked_add(*length).ok_or_else(|| {
                format!("substr 取值越界(start {start}+length {length})(节点失败,无静默回退)")
            })?;
            if end > count {
                return Err(format!(
                    "substr 取值越界(start {start}+length {length} > 字符数 {count})(节点失败,无静默回退)"
                ));
            }
            Ok(chars[*start as usize..end as usize].iter().collect())
        }
        ComputeSpec::Join { separator, .. } => {
            multi("join")?;
            if separator.is_empty() {
                return Err("join 分隔符为空(节点失败,无静默回退)".to_string());
            }
            Ok(resolved.join(separator.as_str()))
        }
        // ===== v1.3 日期（仅 UTC、仅显式入参、无当前时间函数——确定性红线） =====
        ComputeSpec::EpochToDate { .. } => {
            let e = parse_i64_input("epoch_to_date", one("epoch_to_date")?)?;
            let days = e.div_euclid(86_400);
            if !(DATE_DOMAIN_MIN_DAYS..=DATE_DOMAIN_MAX_DAYS).contains(&days) {
                return Err(format!(
                    "epoch_to_date 值 {e} 换算日期超出 0000-01-01..=9999-12-31(节点失败,无静默回退)"
                ));
            }
            let (y, m, d) = civil_from_days(days);
            Ok(format!("{y:04}-{m:02}-{d:02}"))
        }
        ComputeSpec::DateDiffDays { .. } => {
            let [a, b] = resolved.as_slice() else {
                return Err(format!(
                    "date_diff_days 需恰 2 输入,实得 {}",
                    resolved.len()
                ));
            };
            let (d0, d1) = (
                parse_date_str("date_diff_days", a)?,
                parse_date_str("date_diff_days", b)?,
            );
            let diff = d1
                .checked_sub(d0)
                .ok_or_else(|| "date_diff_days 整数溢出(节点失败,无静默回退)".to_string())?;
            Ok(diff.to_string())
        }
        ComputeSpec::EpochAddDays { days, .. } => {
            let e = parse_i64_input("epoch_add_days", one("epoch_add_days")?)?;
            let delta = days
                .checked_mul(86_400)
                .ok_or_else(|| "epoch_add_days 整数溢出(节点失败,无静默回退)".to_string())?;
            let out = e
                .checked_add(delta)
                .ok_or_else(|| "epoch_add_days 整数溢出(节点失败,无静默回退)".to_string())?;
            Ok(out.to_string())
        }
        // ===== v1.3 逻辑（封闭布尔词表 true|false，不隐式真值化） =====
        ComputeSpec::And { .. } => {
            multi("and")?;
            let vals: Result<Vec<bool>, String> = resolved
                .iter()
                .map(|v| parse_bool_vocab("and", v))
                .collect();
            Ok(if vals?.iter().all(|&b| b) {
                "true"
            } else {
                "false"
            }
            .to_string())
        }
        ComputeSpec::Or { .. } => {
            multi("or")?;
            let vals: Result<Vec<bool>, String> =
                resolved.iter().map(|v| parse_bool_vocab("or", v)).collect();
            Ok(if vals?.iter().any(|&b| b) {
                "true"
            } else {
                "false"
            }
            .to_string())
        }
        ComputeSpec::Not { .. } => {
            let v = parse_bool_vocab("not", one("not")?)?;
            Ok(if v { "false" } else { "true" }.to_string())
        }
        ComputeSpec::IfElse { .. } => {
            let [cond, t, f] = resolved.as_slice() else {
                return Err(format!("if_else 需恰 3 输入,实得 {}", resolved.len()));
            };
            // 选择器语义:条件取封闭布尔词表,分支结果原样字符串(无词表)
            if parse_bool_vocab("if_else", cond)? {
                Ok(t.clone())
            } else {
                Ok(f.clone())
            }
        }
        ComputeSpec::Clamp {
            min_val, max_val, ..
        } => {
            let x = parse_i64_input("clamp", one("clamp")?)?;
            // min_val > max_val 已在加载期拒载;此处用整体序运算防御直调 panic
            Ok(x.max(*min_val).min(*max_val).to_string())
        }
    }
}

/// compute 形态公共校验(封闭目录 27 函数;schema oneOf 已表达,代码层双保险)
///
/// 只做**形态**校验:输入数量区间、常量参数合法值(threshold 互斥/pattern 编译/
/// 非空分隔符与查找串/clamp 界序)。不含输入引用存在性(消费点各自持有 id_set)。
/// 消费点:[`validate_compute_spec`](加载校验)与 materializer::check_compute_spec
/// (物化器防御性再校验)——单一实现,禁复制第二份。
pub fn check_spec_shape(spec: &ComputeSpec) -> Result<(), String> {
    let count =
        |inputs: &[ComputeInput], fname: &str, lo: usize, hi: usize| -> Result<(), String> {
            if inputs.len() < lo || inputs.len() > hi {
                return Err(format!(
                    "{fname} requires {lo}..={hi} inputs, got {}",
                    inputs.len()
                ));
            }
            Ok(())
        };
    let exactly = |inputs: &[ComputeInput], fname: &str, n: usize| -> Result<(), String> {
        if inputs.len() != n {
            return Err(format!(
                "{fname} requires exactly {n} inputs, got {}",
                inputs.len()
            ));
        }
        Ok(())
    };
    match spec {
        // ===== v1.2 三函数 =====
        ComputeSpec::Strcmp { inputs, .. } => exactly(inputs, "strcmp", 2),
        ComputeSpec::NumericCmp {
            inputs, threshold, ..
        } => {
            count(inputs, "numeric_cmp", 1, 2)?;
            if inputs.len() == 1 && threshold.is_none() {
                return Err("numeric_cmp single-input form requires threshold".to_string());
            }
            if inputs.len() == 2 && threshold.is_some() {
                return Err("numeric_cmp two-input form forbids threshold".to_string());
            }
            Ok(())
        }
        ComputeSpec::RegexMatch { inputs, pattern } => {
            exactly(inputs, "regex_match", 1)?;
            if pattern.is_empty() {
                return Err("regex pattern must be non-empty".to_string());
            }
            regex::Regex::new(pattern)
                .map(|_| ())
                .map_err(|e| format!("invalid regex pattern '{pattern}': {e}"))
        }
        // ===== v1.3 算术 =====
        ComputeSpec::Add { inputs } => count(inputs, "add", 2, 8),
        ComputeSpec::Sub { inputs } => count(inputs, "sub", 2, 8),
        ComputeSpec::Mul { inputs } => count(inputs, "mul", 2, 8),
        ComputeSpec::Div { inputs } => exactly(inputs, "div", 2),
        ComputeSpec::Abs { inputs } => exactly(inputs, "abs", 1),
        ComputeSpec::Min { inputs } => count(inputs, "min", 2, 8),
        ComputeSpec::Max { inputs } => count(inputs, "max", 2, 8),
        // ===== v1.3 字符串 =====
        ComputeSpec::Concat { inputs } => count(inputs, "concat", 2, 8),
        ComputeSpec::Length { inputs } => exactly(inputs, "length", 1),
        ComputeSpec::Upper { inputs } => exactly(inputs, "upper", 1),
        ComputeSpec::Lower { inputs } => exactly(inputs, "lower", 1),
        ComputeSpec::Trim { inputs } => exactly(inputs, "trim", 1),
        ComputeSpec::Replace { inputs, find, .. } => {
            exactly(inputs, "replace", 1)?;
            if find.is_empty() {
                return Err("replace requires non-empty find".to_string());
            }
            Ok(())
        }
        ComputeSpec::SplitAt {
            inputs, separator, ..
        } => {
            exactly(inputs, "split_at", 1)?;
            if separator.is_empty() {
                return Err("split_at requires non-empty separator".to_string());
            }
            Ok(())
        }
        ComputeSpec::Substr { inputs, .. } => exactly(inputs, "substr", 1),
        ComputeSpec::Join { inputs, separator } => {
            count(inputs, "join", 2, 8)?;
            if separator.is_empty() {
                return Err("join requires non-empty separator".to_string());
            }
            Ok(())
        }
        // ===== v1.3 日期 =====
        ComputeSpec::EpochToDate { inputs } => exactly(inputs, "epoch_to_date", 1),
        ComputeSpec::DateDiffDays { inputs } => exactly(inputs, "date_diff_days", 2),
        ComputeSpec::EpochAddDays { inputs, .. } => exactly(inputs, "epoch_add_days", 1),
        // ===== v1.3 逻辑 =====
        ComputeSpec::And { inputs } => count(inputs, "and", 2, 8),
        ComputeSpec::Or { inputs } => count(inputs, "or", 2, 8),
        ComputeSpec::Not { inputs } => exactly(inputs, "not", 1),
        ComputeSpec::IfElse { inputs } => exactly(inputs, "if_else", 3),
        ComputeSpec::Clamp {
            inputs,
            min_val,
            max_val,
        } => {
            exactly(inputs, "clamp", 1)?;
            if min_val > max_val {
                return Err("clamp requires min_val <= max_val".to_string());
            }
            Ok(())
        }
    }
}

/// compute 节点校验(封闭目录代码层校验,交付物 4 §4.6 双保险——schema oneOf
/// 已表达 + 本校验兜底):形态(公共实现 [`check_spec_shape`],与物化器防御性
/// 再校验同源,禁复制第二份)+ inputs 引用存在性
fn validate_compute_spec(
    wf_id: &str,
    node_id: &str,
    spec: &ComputeSpec,
    id_set: &HashSet<&str>,
) -> Result<(), String> {
    check_spec_shape(spec)
        .map_err(|e| format!("workflow '{wf_id}': compute node '{node_id}' {e}"))?;
    for i in spec.inputs() {
        if let ComputeInput::Node(name) = i {
            if !id_set.contains(name.as_str()) {
                return Err(format!(
                    "workflow '{wf_id}': compute node '{node_id}' input references unknown node '{name}'"
                ));
            }
        }
    }
    Ok(())
}

/// 一层的执行/跳过分划(纯函数,决定论:同输入必同输出)
///
/// 规则:
/// 1. 节点声明了 `run_when` → 按条件求值决定去留(**豁免级联**:即使上游被跳过,
///    条件为真仍执行)
/// 2. 未声明 `run_when` → 任一直接依赖已被跳过即级联跳过
///
/// 层内规划不依赖层内执行结果(run_when 只允许引用更早拓扑层,见
/// `check_run_when_layers`),因此层内节点顺序不影响分划结果。
///
/// 返回 (本层待执行节点[保持原序], 新增跳过节点 id 列表)
/// 恢复种子:从 run 级账本回放重建的引擎入参状态(空值 = 全量跑)
#[derive(Debug, Clone, Default)]
pub struct NodeCheckpointReplay {
    /// 已完成节点结果(键=节点 id;恢复后供下游模板渲染,不重复执行)
    pub results: BTreeMap<String, String>,
    /// 已判定跳过的节点 id(恢复后不再复判、不重复落账)
    pub skipped: HashSet<String>,
}

/// 从 run 级账本回放粒级检查点,重建恢复种子(fail-closed:计划锚不匹配/
/// blob 全文缺失/hash 校验不过,一律 Err 拒绝恢复——宁可重跑不可错续)。
///
/// 语义注记:计划锚按严格单值校验——账内出现任一其它计划锚的检查点即
/// 拒绝(防错版本续跑的关键闸)。replan 多版本场景由外层驱动切片回放
/// (最新计划检查点之后的检查点尾段属当前版本),本函数不做切片。
pub fn replay_node_checkpoints(
    lines: &[JournalLine],
    plan_hash: &str,
) -> Result<NodeCheckpointReplay, String> {
    let mut out = NodeCheckpointReplay::default();
    // blob 全文检索表先建满(blob 载体在检查点之后落账,单遍消费会扑空),
    // 再逐检查点解析
    let mut blobs: BTreeMap<(String, String), String> = BTreeMap::new();
    for line in lines {
        if let JournalEvent::CheckpointBlob {
            node_id,
            hash,
            full_text,
            ..
        } = &line.event
        {
            blobs.insert((node_id.clone(), hash.clone()), full_text.clone());
        }
    }
    for line in lines {
        let JournalEvent::NodeCheckpointed {
            plan_hash: found,
            node_id,
            status,
            result_ref,
            ..
        } = &line.event
        else {
            continue;
        };
        if found != plan_hash {
            return Err(format!(
                "checkpoint plan hash mismatch (expected {plan_hash}, found {found}) \
                 - refusing recovery (rerun is the safe direction)"
            ));
        }
        let content = match &result_ref.inline {
            Some(text) => text.clone(),
            None => blobs
                .get(&(node_id.clone(), result_ref.hash.clone()))
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "checkpoint blob content unavailable for node '{node_id}' \
                         - refusing recovery"
                    )
                })?,
        };
        if evorule_digest(&content) != result_ref.hash {
            return Err(format!(
                "checkpoint content hash mismatch for node '{node_id}' \
                 - refusing recovery"
            ));
        }
        match status.as_str() {
            "completed" => {
                out.results.insert(node_id.clone(), content);
            }
            "skipped" => {
                out.skipped.insert(node_id.clone());
            }
            other => {
                return Err(format!(
                    "unknown checkpoint status '{other}' for node '{node_id}' \
                     - refusing recovery"
                ))
            }
        }
    }
    Ok(out)
}

fn plan_layer<'a>(
    layer: &[&'a WorkflowNode],
    results: &BTreeMap<String, String>,
    skipped: &HashSet<String>,
) -> (Vec<&'a WorkflowNode>, Vec<String>) {
    let mut to_run = Vec::new();
    let mut newly_skipped = Vec::new();
    for &node in layer {
        let run = match &node.run_when {
            Some(cond) => eval_condition(cond, results),
            None => !node.depends_on.iter().any(|d| skipped.contains(d.as_str())),
        };
        if run {
            to_run.push(node);
        } else {
            newly_skipped.push(node.id.clone());
        }
    }
    (to_run, newly_skipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::definition::AgentDefinitionManager;
    use crate::agent::delegate::DelegateContext;
    use crate::api::evorule_client::EvoruleApiClient;

    fn make_ctx() -> DelegateContext {
        let definitions = AgentDefinitionManager::with_default_dir();
        DelegateContext::new(
            "parent",
            definitions,
            EvoruleApiClient::new("http://localhost:8080"),
        )
    }

    fn node(id: &str, agent: &str, deps: &[&str]) -> WorkflowNode {
        WorkflowNode {
            id: id.to_string(),
            agent_type: agent.to_string(),
            task: format!("task for {}", id),
            task_template: None,
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            run_when: None,
            compute: None,
            output_schema: None,
            judge: None,
            atomic: false,
        }
    }

    // ===== M5-c:阶段前置裁决信号 =====

    #[test]
    fn m5c_phase_signal_shape_is_neutral_set() {
        // 中性信号形态：set meta_workflow.phase=<node_id>；引擎不含协作纪律知识
        let sig = phase_signal("n_due_diligence");
        assert_eq!(sig["type"], "set");
        assert_eq!(sig["params"]["attr"], "meta_workflow.phase");
        assert_eq!(sig["params"]["operation"], "set");
        assert_eq!(sig["params"]["value"], "n_due_diligence");
        // 同输入必同输出（确定性）
        assert_eq!(sig, phase_signal("n_due_diligence"));
    }

    // ===== 反序列化 =====

    #[test]
    fn test_workflow_deserialize_from_json() {
        let json = r#"{
            "workflow_id": "research_and_write",
            "description": "并行调研 + 串行写作",
            "nodes": [
                {"id": "research_rust", "agent_type": "researcher", "task": "调研 Rust", "depends_on": []},
                {"id": "research_python", "agent_type": "researcher", "task": "调研 Python", "depends_on": []},
                {"id": "write_report", "agent_type": "writer", "task_template": "Rust: {research_rust}\nPython: {research_python}", "depends_on": ["research_rust", "research_python"]}
            ],
            "output_node": "write_report"
        }"#;
        let wf: Workflow = serde_json::from_str(json).unwrap();
        assert_eq!(wf.workflow_id, "research_and_write");
        assert_eq!(wf.nodes.len(), 3);
        assert_eq!(wf.output_node, "write_report");
        assert!(wf.nodes[2].task_template.is_some());
        assert!(wf.nodes[2].task.is_empty()); // task 默认空
        assert_eq!(wf.nodes[0].depends_on, Vec::<String>::new());
    }

    #[test]
    fn test_workflow_deserialize_minimal() {
        let json = r#"{
            "workflow_id": "single",
            "nodes": [{"id": "only", "agent_type": "worker", "task": "do it"}],
            "output_node": "only"
        }"#;
        let wf: Workflow = serde_json::from_str(json).unwrap();
        assert_eq!(wf.nodes.len(), 1);
        assert_eq!(wf.description, ""); // default
    }

    // ===== compute 节点(交付物 4;D-03 拍板方案③,Phase 1-A T3) =====

    fn compute_node(id: &str, deps: &[&str], spec: ComputeSpec) -> WorkflowNode {
        WorkflowNode {
            id: id.to_string(),
            agent_type: String::new(), // compute 无 agent 语义
            task: String::new(),
            task_template: None,
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            run_when: None,
            compute: Some(spec),
            output_schema: None,
            judge: None,
            atomic: false,
        }
    }

    fn strcmp_equal(a: &str, b: &str) -> ComputeSpec {
        ComputeSpec::Strcmp {
            inputs: vec![
                ComputeInput::Node(a.to_string()),
                ComputeInput::Node(b.to_string()),
            ],
            mode: StrcmpMode::Equal,
        }
    }

    fn pure_compute_wf(nodes: Vec<WorkflowNode>, output: &str) -> Workflow {
        Workflow {
            workflow_id: "compute_wf".to_string(),
            description: String::new(),
            nodes,
            output_node: output.to_string(),
        }
    }

    // ===== 粒级检查点与恢复(run 级账本) =====

    fn checkpoint_chain_wf() -> Workflow {
        // 三粒全 compute 链:n1=""(空拼接) → n2=Length(n1)="0" →
        // n3=Replace(n2,"0"→"zero")="zero"——终值非平凡,链路贯通可证
        pure_compute_wf(
            vec![
                compute_node(
                    "n1",
                    &[],
                    ComputeSpec::Concat {
                        inputs: vec![ComputeInput::Empty, ComputeInput::Empty],
                    },
                ),
                compute_node(
                    "n2",
                    &["n1"],
                    ComputeSpec::Length {
                        inputs: vec![ComputeInput::Node("n1".to_string())],
                    },
                ),
                compute_node(
                    "n3",
                    &["n2"],
                    ComputeSpec::Replace {
                        inputs: vec![ComputeInput::Node("n2".to_string())],
                        find: "0".to_string(),
                        replacement: "zero".to_string(),
                    },
                ),
            ],
            "n3",
        )
    }

    fn checkpoint_test_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wf-ckpt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn find_planrun_sid(dir: &std::path::Path) -> Option<String> {
        std::fs::read_dir(dir)
            .ok()?
            .filter_map(|e| e.ok())
            .find_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.starts_with("planrun-")
                    .then(|| name.trim_end_matches(".jsonl").to_string())
            })
    }

    async fn run_chain_with_journal_dir(
        dir: &std::path::Path,
    ) -> (String, std::sync::Arc<WorkflowEngine>) {
        let ctx = make_ctx().with_journal_dir(dir.to_path_buf());
        let engine = std::sync::Arc::new(WorkflowEngine::new(ctx));
        let wf = checkpoint_chain_wf();
        let out = engine.execute(&wf).await.unwrap();
        (out, engine)
    }

    #[tokio::test]
    async fn checkpoint_resume_zero_reexecution_and_equivalent_result() {
        // 三粒链:连续跑出基线;截断账本到「粒 2 检查点后」模拟崩溃;恢复跑
        // 粒 1/2 零重执行(两代账本合并后每粒恰一条检查点)、粒 3 续跑、
        // 终结果与连续执行等价
        let dir = checkpoint_test_dir("resume");
        let (baseline, _) = run_chain_with_journal_dir(&dir).await;
        assert_eq!(baseline, "zero");
        let wf = checkpoint_chain_wf();
        let plan_hash = WorkflowEngine::workflow_plan_hash(&wf).unwrap();

        // 崩溃模拟:取首代账本,截断到 n3 检查点之前(=粒 2 完成后进程死亡)
        let sid = find_planrun_sid(&dir).expect("run ledger session must exist");
        let path = JournalWriter::path_for(&dir, &sid);
        let lines = crate::agent::journal::read_all(&path).unwrap();
        assert_eq!(lines.len(), 3, "连续跑:三粒各一条检查点");
        let cut = lines
            .iter()
            .position(
                |l| matches!(&l.event, JournalEvent::NodeCheckpointed { node_id, .. } if node_id == "n3"),
            )
            .expect("n3 checkpoint must exist in baseline journal");
        let truncated = &lines[..cut];

        // 恢复跑:同 ctx 域新引擎,种子=截断账本回放
        let replay = replay_node_checkpoints(truncated, &plan_hash).unwrap();
        assert_eq!(replay.results.len(), 2, "粒 1/2 从检查点重建");
        assert!(replay.skipped.is_empty());
        let ctx2 = make_ctx().with_journal_dir(dir.clone());
        let engine2 = std::sync::Arc::new(WorkflowEngine::new(ctx2));
        let out2 = engine2.execute_with_resume(&wf, replay).await.unwrap();
        assert_eq!(out2, baseline, "恢复跑终结果与连续执行等价");
        assert_eq!(
            engine2.executed_nodes(),
            1,
            "恢复进程计数器增量只含本进程新执行粒(粒 3)"
        );

        // 零重执行:恢复代账本只含粒 3 一条检查点(粒 1/2 未再执行)
        let sid2 = engine2.run_ledger_session_id().unwrap();
        let lines2 =
            crate::agent::journal::read_all(&JournalWriter::path_for(&dir, &sid2)).unwrap();
        assert_eq!(lines2.len(), 1, "恢复代只落粒 3 检查点=零重执行");
        match &lines2[0].event {
            JournalEvent::NodeCheckpointed { node_id, .. } => assert_eq!(node_id, "n3"),
            other => panic!("expected node_checkpointed, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn checkpoint_plan_hash_mismatch_refuses_recovery() {
        // 计划锚不匹配 = 拒绝恢复(负例;宁可重跑不可错续)
        let dir = checkpoint_test_dir("mismatch");
        let _ = run_chain_with_journal_dir(&dir).await;
        let sid = find_planrun_sid(&dir).unwrap();
        let lines = crate::agent::journal::read_all(&JournalWriter::path_for(&dir, &sid)).unwrap();
        let err = replay_node_checkpoints(&lines, "deadbeef-wrong-plan-hash").unwrap_err();
        assert!(err.contains("plan hash mismatch"), "拒绝恢复: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checkpoint_blob_rebuild_and_missing_refusal() {
        // 大结果走 blob 引用且可重建(hash 校验);blob 缺失/被改 = 拒绝恢复
        let dir = checkpoint_test_dir("blob");
        let big = "y".repeat(crate::agent::journal::CHECKPOINT_INLINE_LIMIT + 16);
        let writer = JournalWriter::open(&dir, "blob-sess").unwrap();
        let plan_hash = WorkflowEngine::workflow_plan_hash(&checkpoint_chain_wf()).unwrap();
        writer
            .node_checkpointed("wf", &plan_hash, "n_big", "completed", &big)
            .unwrap();
        drop(writer);
        let lines =
            crate::agent::journal::read_all(&JournalWriter::path_for(&dir, "blob-sess")).unwrap();
        let replay = replay_node_checkpoints(&lines, &plan_hash).unwrap();
        assert_eq!(
            replay.results.get("n_big").map(String::as_str),
            Some(big.as_str())
        );

        // blob 缺失:仅保留检查点行(全文载体被裁)→ 拒绝
        let without_blob: Vec<JournalLine> = lines
            .iter()
            .filter(|l| !matches!(l.event, JournalEvent::CheckpointBlob { .. }))
            .cloned()
            .collect();
        let err = replay_node_checkpoints(&without_blob, &plan_hash).unwrap_err();
        assert!(err.contains("blob content unavailable"), "拒绝恢复: {err}");

        // blob 被改:全文与引用 hash 不一致 → 拒绝
        let mut tampered = lines.clone();
        if let Some(line) = tampered
            .iter_mut()
            .find(|l| matches!(l.event, JournalEvent::CheckpointBlob { .. }))
        {
            if let JournalEvent::CheckpointBlob { full_text, .. } = &mut line.event {
                full_text.push('!');
            }
        }
        let err = replay_node_checkpoints(&tampered, &plan_hash).unwrap_err();
        assert!(err.contains("hash mismatch"), "拒绝恢复: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn checkpoint_disabled_and_journalless_paths_stay_ledger_free() {
        // 无账域(ctx 无 journal_dir)与显式关停:行为不变且零 run 账落盘
        let dir = checkpoint_test_dir("disabled");
        let ctx = make_ctx(); // 无 journal_dir
        let engine = WorkflowEngine::new(ctx);
        let out = engine.execute(&checkpoint_chain_wf()).await.unwrap();
        assert_eq!(out, "zero");
        assert!(engine.run_ledger_session_id().is_none(), "无账域零建账");

        let ctx2 = make_ctx().with_journal_dir(dir.clone());
        let engine2 = WorkflowEngine::new(ctx2).without_run_journal();
        let out2 = engine2.execute(&checkpoint_chain_wf()).await.unwrap();
        assert_eq!(out2, "zero");
        assert!(engine2.run_ledger_session_id().is_none(), "显式关停零建账");
        let stray: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("planrun-"))
            .collect();
        assert!(stray.is_empty(), "关停后不得出现 planrun 账本: {stray:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checkpoint_injected_session_id_readable() {
        // 显式注入句柄的会话 id 直读(恢复面回放入口)
        let dir = checkpoint_test_dir("bootstrap");
        let ctx = make_ctx().with_journal_dir(dir.clone());
        let writer = JournalWriter::open(&dir, "probe").unwrap();
        let engine = WorkflowEngine::new(ctx).with_run_journal(writer);
        assert_eq!(engine.run_ledger_session_id().as_deref(), Some("probe"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_eval_compute_result_vocabularies() {
        // strcmp/equal 词表 equal|different(交付物 4 §4.1;different 不含 equal 子串)
        let results: BTreeMap<String, String> = [
            ("x".to_string(), "abc".to_string()),
            ("y".to_string(), "abc".to_string()),
            ("z".to_string(), "abd".to_string()),
        ]
        .into_iter()
        .collect();
        let same = ComputeSpec::Strcmp {
            inputs: vec![
                ComputeInput::Node("x".into()),
                ComputeInput::Node("y".into()),
            ],
            mode: StrcmpMode::Equal,
        };
        assert_eq!(eval_compute(&same, &results).unwrap(), "equal");
        let diff = ComputeSpec::Strcmp {
            inputs: vec![
                ComputeInput::Node("x".into()),
                ComputeInput::Node("z".into()),
            ],
            mode: StrcmpMode::Equal,
        };
        assert_eq!(eval_compute(&diff, &results).unwrap(), "different");

        // strcmp/contains 词表 contained|not_contained
        let b_results: BTreeMap<String, String> = [
            ("b".to_string(), "bc".to_string()),
            ("x".to_string(), "abc".to_string()),
            ("z".to_string(), "abd".to_string()),
        ]
        .into_iter()
        .collect();
        let contains = ComputeSpec::Strcmp {
            inputs: vec![
                ComputeInput::Node("x".into()),
                ComputeInput::Node("b".into()),
            ],
            mode: StrcmpMode::Contains,
        };
        assert_eq!(eval_compute(&contains, &b_results).unwrap(), "contained");
        let not_contains = ComputeSpec::Strcmp {
            inputs: vec![
                ComputeInput::Node("z".into()),
                ComputeInput::Node("b".into()),
            ],
            mode: StrcmpMode::Contains,
        };
        assert_eq!(
            eval_compute(&not_contains, &b_results).unwrap(),
            "not_contained"
        );

        // numeric_cmp 词表 true|false(双输入 + 单输入 threshold 两形态)
        let num_results: BTreeMap<String, String> = [
            ("n1".to_string(), "3".to_string()),
            ("n2".to_string(), "5".to_string()),
        ]
        .into_iter()
        .collect();
        let two = ComputeSpec::NumericCmp {
            inputs: vec![
                ComputeInput::Node("n1".into()),
                ComputeInput::Node("n2".into()),
            ],
            mode: NumericCmpMode::Lt,
            threshold: None,
        };
        assert_eq!(eval_compute(&two, &num_results).unwrap(), "true");
        let single = ComputeSpec::NumericCmp {
            inputs: vec![ComputeInput::Node("n1".into())],
            mode: NumericCmpMode::Ge,
            threshold: Some(3.0),
        };
        assert_eq!(eval_compute(&single, &num_results).unwrap(), "true");

        // regex_match 词表 match|no_match
        let re = ComputeSpec::RegexMatch {
            inputs: vec![ComputeInput::Node("x".into())],
            pattern: "^a.c$".to_string(),
        };
        assert_eq!(eval_compute(&re, &results).unwrap(), "match");
        let re_no = ComputeSpec::RegexMatch {
            inputs: vec![ComputeInput::Node("z".into())],
            pattern: "^x".to_string(),
        };
        assert_eq!(eval_compute(&re_no, &results).unwrap(), "no_match");
    }

    #[test]
    fn test_eval_compute_deterministic_byte_identical() {
        // 同输入重复求值逐字节一致(交付物 4 §2.5-4)
        let results: BTreeMap<String, String> = [
            ("a".to_string(), "hello world".to_string()),
            ("b".to_string(), "world".to_string()),
        ]
        .into_iter()
        .collect();
        let spec = ComputeSpec::Strcmp {
            inputs: vec![
                ComputeInput::Node("a".into()),
                ComputeInput::Node("b".into()),
            ],
            mode: StrcmpMode::Contains,
        };
        let first = eval_compute(&spec, &results).unwrap();
        for _ in 0..10 {
            assert_eq!(eval_compute(&spec, &results).unwrap(), first);
        }
    }

    #[test]
    fn test_eval_compute_numeric_parse_failure_is_node_failure() {
        // 数值解析失败 = 节点失败(交付物 4 §4.5,无静默回退:不当 0/空串继续跑)
        let results: BTreeMap<String, String> = [
            ("bad".to_string(), "not-a-number".to_string()),
            ("n".to_string(), "2".to_string()),
        ]
        .into_iter()
        .collect();
        let spec = ComputeSpec::NumericCmp {
            inputs: vec![
                ComputeInput::Node("bad".into()),
                ComputeInput::Node("n".into()),
            ],
            mode: NumericCmpMode::Lt,
            threshold: None,
        };
        let err = eval_compute(&spec, &results).unwrap_err();
        assert!(err.contains("数值解析失败"), "{err}");
    }

    #[test]
    fn test_eval_compute_input_missing_is_empty_string() {
        // 缺失语义(交付物 4 §3.3):被跳过/无结果节点 → 空串;Empty 字面量 → 空串
        let results: BTreeMap<String, String> =
            [("a".to_string(), "v".to_string())].into_iter().collect();
        let spec = ComputeSpec::Strcmp {
            inputs: vec![ComputeInput::Node("missing".into()), ComputeInput::Empty],
            mode: StrcmpMode::Equal,
        };
        assert_eq!(eval_compute(&spec, &results).unwrap(), "equal");
    }

    #[test]
    fn test_validate_compute_form_rejections() {
        // 封闭目录代码层校验(交付物 4 §4.6 双保险):数量/threshold 互斥/pattern/引用
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let cases: Vec<(Workflow, &str)> = vec![
            // strcmp 1 输入
            (
                pure_compute_wf(
                    vec![compute_node(
                        "c",
                        &[],
                        ComputeSpec::Strcmp {
                            inputs: vec![ComputeInput::Node("c".into())],
                            mode: StrcmpMode::Equal,
                        },
                    )],
                    "c",
                ),
                "exactly 2 inputs",
            ),
            // numeric_cmp 双输入带 threshold(禁止)
            (
                pure_compute_wf(
                    vec![
                        node("a", "worker", &[]),
                        node("b", "worker", &[]),
                        WorkflowNode {
                            compute: Some(ComputeSpec::NumericCmp {
                                inputs: vec![
                                    ComputeInput::Node("a".into()),
                                    ComputeInput::Node("b".into()),
                                ],
                                mode: NumericCmpMode::Lt,
                                threshold: Some(1.0),
                            }),
                            ..compute_node(
                                "c",
                                &["a", "b"],
                                ComputeSpec::RegexMatch {
                                    inputs: vec![ComputeInput::Node("a".into())],
                                    pattern: "x".to_string(),
                                },
                            )
                        },
                    ],
                    "c",
                ),
                "forbids threshold",
            ),
            // numeric_cmp 单输入缺 threshold(必带)
            (
                pure_compute_wf(
                    vec![compute_node(
                        "c",
                        &[],
                        ComputeSpec::NumericCmp {
                            inputs: vec![ComputeInput::Node("c".into())],
                            mode: NumericCmpMode::Lt,
                            threshold: None,
                        },
                    )],
                    "c",
                ),
                "requires threshold",
            ),
            // regex 非法 pattern(加载期校验,交付物 4 §4.3)
            (
                pure_compute_wf(
                    vec![compute_node(
                        "c",
                        &[],
                        ComputeSpec::RegexMatch {
                            inputs: vec![ComputeInput::Node("c".into())],
                            pattern: "([unclosed".to_string(),
                        },
                    )],
                    "c",
                ),
                "invalid regex pattern",
            ),
            // inputs 引用未知节点
            (
                pure_compute_wf(
                    vec![compute_node("c", &[], strcmp_equal("ghost", "c"))],
                    "c",
                ),
                "unknown node 'ghost'",
            ),
        ];
        for (wf, expect) in cases {
            let err = engine.validate(&wf).unwrap_err();
            assert!(err.contains(expect), "expect '{expect}', got: {err}");
        }
    }

    #[tokio::test]
    async fn test_execute_compute_chain_with_skip_semantics() {
        // 全 compute 链集成(不经 delegate,无 LLM/无会话/无网络):
        // a(strcmp Empty 对)= "equal";b run_when 恒假 → skip;
        // c run_when 恒真(豁免级联)执行,输入含被跳过的 b → 空串(§3.3);
        // d regex("^$") 观察被跳过的 b → 空串 → "match"
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = pure_compute_wf(
            vec![
                WorkflowNode {
                    compute: Some(ComputeSpec::Strcmp {
                        inputs: vec![ComputeInput::Empty, ComputeInput::Empty],
                        mode: StrcmpMode::Equal,
                    }),
                    ..compute_node("a", &[], strcmp_equal("a", "a"))
                },
                WorkflowNode {
                    run_when: Some(RunWhen {
                        node: "a".to_string(),
                        op: ConditionOp::Equals,
                        value: "equal_never".to_string(), // 恒假 → b 被跳过
                    }),
                    ..compute_node("b", &["a"], strcmp_equal("a", "a"))
                },
                WorkflowNode {
                    run_when: Some(RunWhen {
                        node: "a".to_string(),
                        op: ConditionOp::Equals,
                        value: "equal".to_string(), // 恒真 → c 执行(豁免级联)
                    }),
                    ..compute_node("c", &["b"], strcmp_equal("b", "a"))
                },
                WorkflowNode {
                    compute: Some(ComputeSpec::RegexMatch {
                        inputs: vec![ComputeInput::Node("b".into())],
                        pattern: "^$".to_string(), // b 被跳过 → 空串 → match
                    }),
                    ..compute_node("d", &["c"], strcmp_equal("c", "c"))
                },
            ],
            "d",
        );
        let out = engine
            .execute(&wf)
            .await
            .expect("pure compute workflow must run");
        assert_eq!(out, "match");
        // 节点计数器（Phase 1-B）：执行 a/c/d = 3，被跳过的 b 不计（交付物 6 §4.1）
        assert_eq!(engine.executed_nodes(), 3);
        // 跨 execute 累加（同一引擎再跑一次 → 6）
        engine.execute(&wf).await.expect("second run");
        assert_eq!(engine.executed_nodes(), 6);
    }

    // ===== v1.3 新增 24 函数（词表/确定性/fail-fast/形态四测） =====

    fn rmap(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// 测试辅助：按名字构造节点引用（`_spec` 形参有意忽略——调用点以字面量
    /// 自注所构造的函数形态，提升用例可读性；运行时只用 names）
    fn nodes_of(_spec: &ComputeSpec, names: &[&str]) -> Vec<ComputeInput> {
        names
            .iter()
            .map(|n| ComputeInput::Node(n.to_string()))
            .collect()
    }

    #[test]
    fn test_eval_compute_v13_result_vocabularies() {
        // 算术族→十进制整数字符串(可带负号);div 向零取整
        let r = rmap(&[("a", "2"), ("b", "3"), ("n", "-4"), ("x", "10"), ("y", "7")]);
        let add = ComputeSpec::Add {
            inputs: nodes_of(&ComputeSpec::Add { inputs: vec![] }, &["a", "b"]),
        };
        assert_eq!(eval_compute(&add, &r).unwrap(), "5");
        let sub = ComputeSpec::Sub {
            inputs: nodes_of(&ComputeSpec::Sub { inputs: vec![] }, &["x", "a", "a"]),
        };
        assert_eq!(eval_compute(&sub, &r).unwrap(), "6");
        let mul = ComputeSpec::Mul {
            inputs: nodes_of(&ComputeSpec::Mul { inputs: vec![] }, &["a", "n"]),
        };
        assert_eq!(eval_compute(&mul, &r).unwrap(), "-8");
        let div = ComputeSpec::Div {
            inputs: nodes_of(&ComputeSpec::Div { inputs: vec![] }, &["y", "a"]),
        };
        assert_eq!(eval_compute(&div, &r).unwrap(), "3");
        let div_neg = ComputeSpec::Div {
            inputs: nodes_of(&ComputeSpec::Div { inputs: vec![] }, &["n", "a"]),
        };
        assert_eq!(eval_compute(&div_neg, &r).unwrap(), "-2");
        let abs = ComputeSpec::Abs {
            inputs: nodes_of(&ComputeSpec::Abs { inputs: vec![] }, &["n"]),
        };
        assert_eq!(eval_compute(&abs, &r).unwrap(), "4");
        let min = ComputeSpec::Min {
            inputs: nodes_of(&ComputeSpec::Min { inputs: vec![] }, &["x", "y", "n"]),
        };
        assert_eq!(eval_compute(&min, &r).unwrap(), "-4");
        let max = ComputeSpec::Max {
            inputs: nodes_of(&ComputeSpec::Max { inputs: vec![] }, &["x", "y", "n"]),
        };
        assert_eq!(eval_compute(&max, &r).unwrap(), "10");
        // clamp:越界截断到界内
        let clamp_hi = ComputeSpec::Clamp {
            inputs: nodes_of(
                &ComputeSpec::Clamp {
                    inputs: vec![],
                    min_val: 0,
                    max_val: 10,
                },
                &["x"],
            ),
            min_val: 0,
            max_val: 10,
        };
        assert_eq!(eval_compute(&clamp_hi, &r).unwrap(), "10");
        let clamp_lo = ComputeSpec::Clamp {
            inputs: nodes_of(
                &ComputeSpec::Clamp {
                    inputs: vec![],
                    min_val: 0,
                    max_val: 10,
                },
                &["n"],
            ),
            min_val: 0,
            max_val: 10,
        };
        assert_eq!(eval_compute(&clamp_lo, &r).unwrap(), "0");

        // 字符串族→原样字符串(Unicode char 计数/索引);length→整数字符串
        let s = rmap(&[
            ("h", "héllo"),
            ("sz", "ß"),
            ("pad", "  hi  "),
            ("csv", "a,b,c"),
        ]);
        let concat = ComputeSpec::Concat {
            inputs: nodes_of(&ComputeSpec::Concat { inputs: vec![] }, &["h", "h"]),
        };
        assert_eq!(eval_compute(&concat, &s).unwrap(), "héllohéllo");
        let length = ComputeSpec::Length {
            inputs: nodes_of(&ComputeSpec::Length { inputs: vec![] }, &["h"]),
        };
        assert_eq!(eval_compute(&length, &s).unwrap(), "5");
        let upper = ComputeSpec::Upper {
            inputs: nodes_of(&ComputeSpec::Upper { inputs: vec![] }, &["sz"]),
        };
        assert_eq!(eval_compute(&upper, &s).unwrap(), "SS");
        let lower = ComputeSpec::Lower {
            inputs: nodes_of(&ComputeSpec::Lower { inputs: vec![] }, &["h"]),
        };
        assert_eq!(eval_compute(&lower, &s).unwrap(), "héllo");
        let trim = ComputeSpec::Trim {
            inputs: nodes_of(&ComputeSpec::Trim { inputs: vec![] }, &["pad"]),
        };
        assert_eq!(eval_compute(&trim, &s).unwrap(), "hi");
        let replace = ComputeSpec::Replace {
            inputs: nodes_of(
                &ComputeSpec::Replace {
                    inputs: vec![],
                    find: String::new(),
                    replacement: String::new(),
                },
                &["h"],
            ),
            find: "l".to_string(),
            replacement: "L".to_string(),
        };
        assert_eq!(eval_compute(&replace, &s).unwrap(), "héLLo");
        let split = ComputeSpec::SplitAt {
            inputs: nodes_of(
                &ComputeSpec::SplitAt {
                    inputs: vec![],
                    separator: String::new(),
                    index: 1,
                },
                &["csv"],
            ),
            separator: ",".to_string(),
            index: 1,
        };
        assert_eq!(eval_compute(&split, &s).unwrap(), "b");
        let substr = ComputeSpec::Substr {
            inputs: nodes_of(
                &ComputeSpec::Substr {
                    inputs: vec![],
                    start: 1,
                    length: 3,
                },
                &["h"],
            ),
            start: 1,
            length: 3,
        };
        assert_eq!(eval_compute(&substr, &s).unwrap(), "éll");
        let join = ComputeSpec::Join {
            inputs: nodes_of(
                &ComputeSpec::Join {
                    inputs: vec![],
                    separator: String::new(),
                },
                &["a", "b", "x"],
            ),
            separator: "|".to_string(),
        };
        assert_eq!(eval_compute(&join, &r).unwrap(), "2|3|10");

        // 日期族:epoch_to_date→YYYY-MM-DD;date_diff_days→整数字符串(可负);epoch_add_days→epoch 秒
        let d = rmap(&[
            ("e", "1791331200"), // 2026-10-07T00:00:00Z
            ("d0", "2026-01-01"),
            ("d1", "2026-01-04"),
        ]);
        let to_date = ComputeSpec::EpochToDate {
            inputs: nodes_of(&ComputeSpec::EpochToDate { inputs: vec![] }, &["e"]),
        };
        assert_eq!(eval_compute(&to_date, &d).unwrap(), "2026-10-07");
        let diff = ComputeSpec::DateDiffDays {
            inputs: nodes_of(&ComputeSpec::DateDiffDays { inputs: vec![] }, &["d0", "d1"]),
        };
        assert_eq!(eval_compute(&diff, &d).unwrap(), "3");
        let diff_neg = ComputeSpec::DateDiffDays {
            inputs: nodes_of(&ComputeSpec::DateDiffDays { inputs: vec![] }, &["d1", "d0"]),
        };
        assert_eq!(eval_compute(&diff_neg, &d).unwrap(), "-3");
        let add_days = ComputeSpec::EpochAddDays {
            inputs: nodes_of(
                &ComputeSpec::EpochAddDays {
                    inputs: vec![],
                    days: 7,
                },
                &["e"],
            ),
            days: 7,
        };
        assert_eq!(eval_compute(&add_days, &d).unwrap(), "1791936000");
        let add_days_neg = ComputeSpec::EpochAddDays {
            inputs: nodes_of(
                &ComputeSpec::EpochAddDays {
                    inputs: vec![],
                    days: -1,
                },
                &["e"],
            ),
            days: -1,
        };
        assert_eq!(eval_compute(&add_days_neg, &d).unwrap(), "1791244800");

        // 逻辑族→true|false;if_else→分支原样字符串(选择器语义)
        let b = rmap(&[("t", "true"), ("f", "false"), ("s1", "yes"), ("s2", "no")]);
        let and = ComputeSpec::And {
            inputs: nodes_of(&ComputeSpec::And { inputs: vec![] }, &["t", "t"]),
        };
        assert_eq!(eval_compute(&and, &b).unwrap(), "true");
        let and_f = ComputeSpec::And {
            inputs: nodes_of(&ComputeSpec::And { inputs: vec![] }, &["t", "f"]),
        };
        assert_eq!(eval_compute(&and_f, &b).unwrap(), "false");
        let or = ComputeSpec::Or {
            inputs: nodes_of(&ComputeSpec::Or { inputs: vec![] }, &["f", "t"]),
        };
        assert_eq!(eval_compute(&or, &b).unwrap(), "true");
        let or_f = ComputeSpec::Or {
            inputs: nodes_of(&ComputeSpec::Or { inputs: vec![] }, &["f", "f"]),
        };
        assert_eq!(eval_compute(&or_f, &b).unwrap(), "false");
        let not = ComputeSpec::Not {
            inputs: nodes_of(&ComputeSpec::Not { inputs: vec![] }, &["f"]),
        };
        assert_eq!(eval_compute(&not, &b).unwrap(), "true");
        let if_t = ComputeSpec::IfElse {
            inputs: nodes_of(&ComputeSpec::IfElse { inputs: vec![] }, &["t", "s1", "s2"]),
        };
        assert_eq!(eval_compute(&if_t, &b).unwrap(), "yes");
        let if_f = ComputeSpec::IfElse {
            inputs: nodes_of(&ComputeSpec::IfElse { inputs: vec![] }, &["f", "s1", "s2"]),
        };
        assert_eq!(eval_compute(&if_f, &b).unwrap(), "no");
    }

    #[test]
    fn test_eval_compute_v13_deterministic_byte_identical() {
        // v1.3 函数同输入重复求值逐字节一致(确定性红线)
        let results = rmap(&[("e", "1791331200"), ("a", "6"), ("b", "7")]);
        let specs: Vec<ComputeSpec> = vec![
            ComputeSpec::EpochToDate {
                inputs: nodes_of(&ComputeSpec::EpochToDate { inputs: vec![] }, &["e"]),
            },
            ComputeSpec::Add {
                inputs: nodes_of(&ComputeSpec::Add { inputs: vec![] }, &["a", "b"]),
            },
            ComputeSpec::Mul {
                inputs: nodes_of(&ComputeSpec::Mul { inputs: vec![] }, &["a", "b"]),
            },
        ];
        for spec in &specs {
            let first = eval_compute(spec, &results).unwrap();
            for _ in 0..10 {
                assert_eq!(eval_compute(spec, &results).unwrap(), first);
            }
        }
    }

    #[test]
    fn test_eval_compute_v13_fail_fast_errors() {
        // 错误语义总纲(fail-fast,无静默回退):解析失败/除零/溢出/越界/域外/词表外/空串参数
        let r = rmap(&[
            ("max", "9223372036854775807"),
            ("min", "-9223372036854775808"),
            ("neg", "-1"),
            ("one", "1"),
            ("zero", "0"),
            ("float", "3.5"),
            ("csv", "a,b"),
            ("abc", "abc"),
            ("big", "9999999999999"),
            ("bad_date", "2026/01/01"),
            ("bad_month", "2026-13-01"),
            ("yes", "yes"),
            ("num", "42"),
        ]);
        let expect_err = |spec: &ComputeSpec, results: &BTreeMap<String, String>, needle: &str| {
            let err = eval_compute(spec, results).unwrap_err();
            assert!(err.contains(needle), "expect '{needle}', got: {err}");
        };
        // 整数解析失败(3.5 非整数形态)
        expect_err(
            &ComputeSpec::Add {
                inputs: nodes_of(&ComputeSpec::Add { inputs: vec![] }, &["one", "float"]),
            },
            &r,
            "整数解析失败",
        );
        // 除零
        expect_err(
            &ComputeSpec::Div {
                inputs: nodes_of(&ComputeSpec::Div { inputs: vec![] }, &["one", "zero"]),
            },
            &r,
            "除数为 0",
        );
        // 溢出:i64::MAX+1;i64::MIN 取绝对值;i64::MIN ÷ -1;epoch_add_days 乘法溢出
        expect_err(
            &ComputeSpec::Add {
                inputs: nodes_of(&ComputeSpec::Add { inputs: vec![] }, &["max", "one"]),
            },
            &r,
            "整数溢出",
        );
        expect_err(
            &ComputeSpec::Abs {
                inputs: nodes_of(&ComputeSpec::Abs { inputs: vec![] }, &["min"]),
            },
            &r,
            "溢出",
        );
        expect_err(
            &ComputeSpec::Div {
                inputs: nodes_of(&ComputeSpec::Div { inputs: vec![] }, &["min", "neg"]),
            },
            &r,
            "溢出",
        );
        expect_err(
            &ComputeSpec::EpochAddDays {
                inputs: nodes_of(
                    &ComputeSpec::EpochAddDays {
                        inputs: vec![],
                        days: 0,
                    },
                    &["max"],
                ),
                days: i64::MAX,
            },
            &r,
            "整数溢出",
        );
        // 索引越界:split_at 段越界;substr 越界(无静默截断)
        expect_err(
            &ComputeSpec::SplitAt {
                inputs: nodes_of(
                    &ComputeSpec::SplitAt {
                        inputs: vec![],
                        separator: ",".into(),
                        index: 5,
                    },
                    &["csv"],
                ),
                separator: ",".to_string(),
                index: 5,
            },
            &r,
            "越界",
        );
        expect_err(
            &ComputeSpec::Substr {
                inputs: nodes_of(
                    &ComputeSpec::Substr {
                        inputs: vec![],
                        start: 1,
                        length: 9,
                    },
                    &["abc"],
                ),
                start: 1,
                length: 9,
            },
            &r,
            "越界",
        );
        // 日期域外与形态不符
        expect_err(
            &ComputeSpec::EpochToDate {
                inputs: nodes_of(&ComputeSpec::EpochToDate { inputs: vec![] }, &["big"]),
            },
            &r,
            "超出 0000-01-01..=9999-12-31",
        );
        expect_err(
            &ComputeSpec::DateDiffDays {
                inputs: nodes_of(
                    &ComputeSpec::DateDiffDays { inputs: vec![] },
                    &["bad_date", "bad_month"],
                ),
            },
            &r,
            "形态不符",
        );
        // 布尔词表外(不隐式真值化)
        expect_err(
            &ComputeSpec::And {
                inputs: nodes_of(&ComputeSpec::And { inputs: vec![] }, &["yes", "yes"]),
            },
            &r,
            "布尔词表",
        );
        expect_err(
            &ComputeSpec::Not {
                inputs: nodes_of(&ComputeSpec::Not { inputs: vec![] }, &["num"]),
            },
            &r,
            "布尔词表",
        );
        // 加载期已拦、eval 兜底防御:空查找串/空分隔符(直接构造直调 eval)
        expect_err(
            &ComputeSpec::Replace {
                inputs: nodes_of(
                    &ComputeSpec::Replace {
                        inputs: vec![],
                        find: String::new(),
                        replacement: String::new(),
                    },
                    &["abc"],
                ),
                find: String::new(),
                replacement: "x".to_string(),
            },
            &r,
            "查找串为空",
        );
        expect_err(
            &ComputeSpec::Join {
                inputs: nodes_of(
                    &ComputeSpec::Join {
                        inputs: vec![],
                        separator: String::new(),
                    },
                    &["abc", "csv"],
                ),
                separator: String::new(),
            },
            &r,
            "分隔符为空",
        );
    }

    #[test]
    fn test_eval_compute_v13_input_whitespace_tolerance() {
        // 真实 LLM 输出常带首尾空白噪声（2026-10-07 agent 面交叉验收 round2 实测：
        // researcher 输出 "\n2026-10-14" 致 date_diff_days 拒收并连锁触发 replan 失败）。
        // parse 边界统一 trim：日期/整数/布尔在 trim 后仍须严格形态（拒绝消息保留原文）；
        // join/concat 等原样输出函数不在 parse 边界，输出保真不 trim。
        let r = rmap(&[
            ("d0", "2026-10-07"),
            ("d1_ws", "\n2026-10-14"),
            ("epoch_ws", "\t1791331200 "),
            ("num_ws", " 6\n"),
            ("bool_ws", " true "),
        ]);
        // date_diff_days[d0, d1_ws] = 2026-10-14 − 2026-10-07 = 7
        let diff = ComputeSpec::DateDiffDays {
            inputs: nodes_of(
                &ComputeSpec::DateDiffDays { inputs: vec![] },
                &["d0", "d1_ws"],
            ),
        };
        assert_eq!(eval_compute(&diff, &r).unwrap(), "7");
        // epoch_to_date[epoch_ws]（trim 后在日期域内）
        let to_date = ComputeSpec::EpochToDate {
            inputs: nodes_of(&ComputeSpec::EpochToDate { inputs: vec![] }, &["epoch_ws"]),
        };
        assert_eq!(eval_compute(&to_date, &r).unwrap(), "2026-10-07");
        // add[num_ws, num_ws] = 12
        let add_ws = ComputeSpec::Add {
            inputs: nodes_of(&ComputeSpec::Add { inputs: vec![] }, &["num_ws", "num_ws"]),
        };
        assert_eq!(eval_compute(&add_ws, &r).unwrap(), "12");
        // and[bool_ws, bool_ws]（trim 后在布尔词表内）
        let and_ws = ComputeSpec::And {
            inputs: nodes_of(
                &ComputeSpec::And { inputs: vec![] },
                &["bool_ws", "bool_ws"],
            ),
        };
        assert_eq!(eval_compute(&and_ws, &r).unwrap(), "true");
        // 输出保真：join 不在 parse 边界，原样拼接不 trim
        let joined = ComputeSpec::Join {
            inputs: nodes_of(
                &ComputeSpec::Join {
                    inputs: vec![],
                    separator: String::new(),
                },
                &["num_ws", "d0"],
            ),
            separator: "|".to_string(),
        };
        assert_eq!(eval_compute(&joined, &r).unwrap(), " 6\n|2026-10-07");
    }

    #[test]
    fn test_validate_compute_v13_shape_rejections() {
        // v1.3 形态校验(公共 check_spec_shape,与物化器同源):数量/常量参数/界序/引用
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let cases: Vec<(Workflow, &str)> = vec![
            // add 1 输入(需 2..=8)
            (
                pure_compute_wf(
                    vec![compute_node(
                        "c",
                        &[],
                        ComputeSpec::Add {
                            inputs: vec![ComputeInput::Node("c".into())],
                        },
                    )],
                    "c",
                ),
                "add requires 2..=8 inputs",
            ),
            // div 3 输入(需恰 2)
            (
                pure_compute_wf(
                    vec![compute_node(
                        "c",
                        &[],
                        ComputeSpec::Div {
                            inputs: vec![
                                ComputeInput::Node("c".into()),
                                ComputeInput::Node("c".into()),
                                ComputeInput::Node("c".into()),
                            ],
                        },
                    )],
                    "c",
                ),
                "div requires exactly 2 inputs",
            ),
            // if_else 2 输入(需恰 3)
            (
                pure_compute_wf(
                    vec![compute_node(
                        "c",
                        &[],
                        ComputeSpec::IfElse {
                            inputs: vec![
                                ComputeInput::Node("c".into()),
                                ComputeInput::Node("c".into()),
                            ],
                        },
                    )],
                    "c",
                ),
                "if_else requires exactly 3 inputs",
            ),
            // clamp min_val > max_val(拒载)
            (
                pure_compute_wf(
                    vec![compute_node(
                        "c",
                        &[],
                        ComputeSpec::Clamp {
                            inputs: vec![ComputeInput::Node("c".into())],
                            min_val: 10,
                            max_val: 0,
                        },
                    )],
                    "c",
                ),
                "clamp requires min_val <= max_val",
            ),
            // replace 空 find
            (
                pure_compute_wf(
                    vec![compute_node(
                        "c",
                        &[],
                        ComputeSpec::Replace {
                            inputs: vec![ComputeInput::Node("c".into())],
                            find: String::new(),
                            replacement: "x".to_string(),
                        },
                    )],
                    "c",
                ),
                "replace requires non-empty find",
            ),
            // join 空分隔符
            (
                pure_compute_wf(
                    vec![compute_node(
                        "c",
                        &[],
                        ComputeSpec::Join {
                            inputs: vec![
                                ComputeInput::Node("c".into()),
                                ComputeInput::Node("c".into()),
                            ],
                            separator: String::new(),
                        },
                    )],
                    "c",
                ),
                "join requires non-empty separator",
            ),
            // split_at 空分隔符
            (
                pure_compute_wf(
                    vec![compute_node(
                        "c",
                        &[],
                        ComputeSpec::SplitAt {
                            inputs: vec![ComputeInput::Node("c".into())],
                            separator: String::new(),
                            index: 0,
                        },
                    )],
                    "c",
                ),
                "split_at requires non-empty separator",
            ),
            // v1.3 函数引用未知节点
            (
                pure_compute_wf(
                    vec![compute_node(
                        "c",
                        &[],
                        ComputeSpec::Add {
                            inputs: vec![
                                ComputeInput::Node("ghost".into()),
                                ComputeInput::Node("c".into()),
                            ],
                        },
                    )],
                    "c",
                ),
                "unknown node 'ghost'",
            ),
        ];
        for (wf, expect) in cases {
            let err = engine.validate(&wf).unwrap_err();
            assert!(err.contains(expect), "expect '{expect}', got: {err}");
        }
    }

    // ===== validate =====

    #[test]
    fn test_validate_empty_nodes() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![],
            output_node: "x".to_string(),
        };
        let err = engine.validate(&wf).unwrap_err();
        assert!(err.contains("no nodes"));
    }

    #[test]
    fn test_validate_duplicate_id() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![node("a", "worker", &[]), node("a", "worker", &[])],
            output_node: "a".to_string(),
        };
        let err = engine.validate(&wf).unwrap_err();
        assert!(err.contains("duplicate node id"));
        assert!(err.contains("'a'"));
    }

    // ===== 门卫负向用例(P2-M7 前置补丁,2026-08-27) =====

    #[test]
    fn test_validate_node_id_rejects_non_identifier() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        // 含花括号的 id 会污染 task_template 占位符机制
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![node("bad{id}", "worker", &[])],
            output_node: "bad{id}".to_string(),
        };
        let err = engine.validate(&wf).unwrap_err();
        assert!(err.contains("not a valid identifier"), "got: {}", err);
    }

    #[test]
    fn test_validate_node_id_rejects_empty_and_space() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        for bad_id in ["", "has space", "dot.dot"] {
            let wf = Workflow {
                workflow_id: "w".to_string(),
                description: String::new(),
                nodes: vec![node(bad_id, "worker", &[])],
                output_node: bad_id.to_string(),
            };
            let err = engine.validate(&wf).unwrap_err();
            assert!(
                err.contains("not a valid identifier"),
                "id {:?} not rejected, got: {}",
                bad_id,
                err
            );
        }
    }

    #[test]
    fn test_validate_agent_type_rejects_path_traversal() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        for bad in ["../researcher", "a/b", "a\\b", ".."] {
            let wf = Workflow {
                workflow_id: "w".to_string(),
                description: String::new(),
                nodes: vec![node("n1", bad, &[])],
                output_node: "n1".to_string(),
            };
            let err = engine.validate(&wf).unwrap_err();
            assert!(
                err.contains("invalid agent_type") || err.contains("path traversal"),
                "agent_type {:?} not rejected, got: {}",
                bad,
                err
            );
        }
    }

    // ----- R6-T01 PT：compute-only 大工作流执行计时（#[ignore] 手动跑，不进 CI）-----

    /// PT 基线（R6-T01，收官遗留 B5）：512 节点 compute-only 长链执行计时。
    ///
    /// 形态 = 最深拓扑（512 节点单链，每节点 strcmp 引用前驱——层序检查的最
    /// 压迫形态：512 层逐层串行）。compute 不经 delegate（无会话/无 IO），
    /// 本 PT 不触网、确定性可重复。定位 = 可观测基线 + 数量级防回归；手动跑：
    /// `cargo test -p evo-agent --lib pt_compute_only_512 -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn pt_compute_only_512_node_chain_timing() {
        const N: usize = 512;
        let mut nodes = Vec::with_capacity(N);
        for i in 0..N {
            let deps: Vec<String> = if i == 0 {
                Vec::new()
            } else {
                vec![format!("c{}", i - 1)]
            };
            let compute = if i == 0 {
                // 首节点：空输入 strcmp（Empty 形态 = 物化器 iter0 同语义）
                crate::agent::workflow::ComputeSpec::Strcmp {
                    inputs: vec![
                        crate::agent::workflow::ComputeInput::Empty,
                        crate::agent::workflow::ComputeInput::Empty,
                    ],
                    mode: crate::agent::workflow::StrcmpMode::Equal,
                }
            } else {
                crate::agent::workflow::ComputeSpec::Strcmp {
                    inputs: vec![
                        crate::agent::workflow::ComputeInput::Node(format!("c{}", i - 1)),
                        crate::agent::workflow::ComputeInput::Empty,
                    ],
                    mode: crate::agent::workflow::StrcmpMode::Contains,
                }
            };
            nodes.push(WorkflowNode {
                id: format!("c{i}"),
                agent_type: "w".to_string(),
                task: "t".to_string(),
                task_template: None,
                depends_on: deps,
                run_when: None,
                compute: Some(compute),
                output_schema: None,
                judge: None,
                atomic: false,
            });
        }
        let wf = Workflow {
            workflow_id: "pt_compute_chain".to_string(),
            description: String::new(),
            nodes,
            output_node: format!("c{}", N - 1),
        };

        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let t0 = std::time::Instant::now();
        let result = engine.execute(&wf).await.expect("512 compute 链须成功");
        let dt = t0.elapsed();
        println!("PT: {N} compute nodes (deepest chain) in {dt:?}, output={result:?}");
        // 数量级防回归（纯函数内联求值应为亚秒级；宽放防环境抖动）
        assert!(
            dt < std::time::Duration::from_secs(30),
            "512 compute 链耗时 {dt:?} 异常（疑似执行层复杂度回归）"
        );
    }

    #[test]
    fn test_validate_accepts_valid_identifiers() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![
                node("alpha_1-beta", "general-2_x", &[]),
                node("n2", "researcher", &["alpha_1-beta"]),
            ],
            output_node: "n2".to_string(),
        };
        assert!(engine.validate(&wf).is_ok());
    }

    #[test]
    fn test_validate_output_node_missing() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![node("a", "worker", &[])],
            output_node: "nonexistent".to_string(),
        };
        let err = engine.validate(&wf).unwrap_err();
        assert!(err.contains("output_node"));
        assert!(err.contains("not found"));
    }

    #[test]
    fn test_validate_unknown_dependency() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![node("a", "worker", &["ghost"])],
            output_node: "a".to_string(),
        };
        let err = engine.validate(&wf).unwrap_err();
        assert!(err.contains("unknown node"));
        assert!(err.contains("'ghost'"));
    }

    #[test]
    fn test_validate_self_dependency() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![node("a", "worker", &["a"])],
            output_node: "a".to_string(),
        };
        let err = engine.validate(&wf).unwrap_err();
        assert!(err.contains("depends on itself"));
    }

    #[test]
    fn test_validate_valid_dag() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![
                node("a", "worker", &[]),
                node("b", "worker", &["a"]),
                node("c", "worker", &["a", "b"]),
            ],
            output_node: "c".to_string(),
        };
        assert!(engine.validate(&wf).is_ok());
    }

    // ===== topological_sort =====

    #[test]
    fn test_topo_sort_linear_chain() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let nodes = vec![
            node("a", "w", &[]),
            node("b", "w", &["a"]),
            node("c", "w", &["b"]),
        ];
        let layers = engine.topological_sort(&nodes).unwrap();
        assert_eq!(layers.len(), 3);
        assert_eq!(layers[0][0].id, "a");
        assert_eq!(layers[1][0].id, "b");
        assert_eq!(layers[2][0].id, "c");
    }

    #[test]
    fn test_topo_sort_parallel_layer() {
        // a, b 无依赖 → 同层;c 依赖两者 → 下一层
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let nodes = vec![
            node("a", "w", &[]),
            node("b", "w", &[]),
            node("c", "w", &["a", "b"]),
        ];
        let layers = engine.topological_sort(&nodes).unwrap();
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].len(), 2); // a, b 同层
        assert_eq!(layers[1].len(), 1); // c 单独
                                        // 第一层包含 a 和 b
        let layer0_ids: Vec<&str> = layers[0].iter().map(|n| n.id.as_str()).collect();
        assert!(layer0_ids.contains(&"a"));
        assert!(layer0_ids.contains(&"b"));
        assert_eq!(layers[1][0].id, "c");
    }

    #[test]
    fn test_topo_sort_diamond() {
        // a → b, a → c, b → d, c → d
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let nodes = vec![
            node("a", "w", &[]),
            node("b", "w", &["a"]),
            node("c", "w", &["a"]),
            node("d", "w", &["b", "c"]),
        ];
        let layers = engine.topological_sort(&nodes).unwrap();
        assert_eq!(layers.len(), 3);
        assert_eq!(layers[0].len(), 1); // a
        assert_eq!(layers[1].len(), 2); // b, c
        assert_eq!(layers[2].len(), 1); // d
    }

    #[test]
    fn test_topo_sort_cycle_detected() {
        // a → b → a(环)
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let nodes = vec![node("a", "w", &["b"]), node("b", "w", &["a"])];
        let err = engine.topological_sort(&nodes).unwrap_err();
        assert!(err.contains("cycle detected"));
        assert!(err.contains("a"));
        assert!(err.contains("b"));
    }

    #[test]
    fn test_topo_sort_self_loop_via_dep() {
        // validate 拦截自依赖,但 topological_sort 直接调用时应能处理
        // (自依赖会被环检测捕获,因为节点永远无法满足"依赖已处理")
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let nodes = vec![node("a", "w", &["a"])];
        let err = engine.topological_sort(&nodes).unwrap_err();
        assert!(err.contains("cycle detected"));
    }

    #[test]
    fn test_topo_sort_single_node() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let nodes = vec![node("a", "w", &[])];
        let layers = engine.topological_sort(&nodes).unwrap();
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].len(), 1);
    }

    // ===== render_task =====

    #[test]
    fn test_render_task_with_template() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let n = WorkflowNode {
            id: "c".to_string(),
            agent_type: "writer".to_string(),
            task: String::new(),
            task_template: Some("Rust: {a}\nPython: {b}".to_string()),
            depends_on: vec!["a".to_string(), "b".to_string()],
            run_when: None,
            compute: None,
            output_schema: None,
            judge: None,
            atomic: false,
        };
        let mut results = BTreeMap::new();
        results.insert("a".to_string(), "rust-result".to_string());
        results.insert("b".to_string(), "python-result".to_string());
        let rendered = engine.render_task(&n, &results, &HashSet::new());
        assert_eq!(rendered, "Rust: rust-result\nPython: python-result");
    }

    #[test]
    fn test_render_task_without_template_uses_task() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let n = WorkflowNode {
            id: "a".to_string(),
            agent_type: "researcher".to_string(),
            task: "调研 Rust".to_string(),
            task_template: None,
            depends_on: vec![],
            run_when: None,
            compute: None,
            output_schema: None,
            judge: None,
            atomic: false,
        };
        let results = BTreeMap::new();
        let rendered = engine.render_task(&n, &results, &HashSet::new());
        assert_eq!(rendered, "调研 Rust");
    }

    #[test]
    fn test_render_task_template_prefers_over_task() {
        // 同时提供 task 和 task_template → 用 template
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let n = WorkflowNode {
            id: "a".to_string(),
            agent_type: "w".to_string(),
            task: "plain task".to_string(),
            task_template: Some("template {x}".to_string()),
            depends_on: vec![],
            run_when: None,
            compute: None,
            output_schema: None,
            judge: None,
            atomic: false,
        };
        let mut results = BTreeMap::new();
        results.insert("x".to_string(), "VAL".to_string());
        let rendered = engine.render_task(&n, &results, &HashSet::new());
        assert_eq!(rendered, "template VAL");
    }

    #[test]
    fn test_render_task_unresolved_placeholder_preserved() {
        // 占位符无对应结果时保留(拓扑排序保证不会发生,但行为应可预测)
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let n = WorkflowNode {
            id: "c".to_string(),
            agent_type: "w".to_string(),
            task: String::new(),
            task_template: Some("{a} and {b}".to_string()),
            depends_on: vec!["a".to_string()],
            run_when: None,
            compute: None,
            output_schema: None,
            judge: None,
            atomic: false,
        };
        let mut results = BTreeMap::new();
        results.insert("a".to_string(), "A".to_string());
        // b 未就绪
        let rendered = engine.render_task(&n, &results, &HashSet::new());
        assert_eq!(rendered, "A and {b}");
    }

    #[test]
    fn test_render_task_skipped_upstream_replaced_with_empty() {
        // 被跳过的上游无结果:占位符替换为空字符串(不把 {id} 字面量漏进下游任务)
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let n = WorkflowNode {
            id: "c".to_string(),
            agent_type: "w".to_string(),
            task: String::new(),
            task_template: Some("review: {b}done".to_string()),
            depends_on: vec!["b".to_string()],
            run_when: None,
            compute: None,
            output_schema: None,
            judge: None,
            atomic: false,
        };
        let mut results = BTreeMap::new();
        results.insert("a".to_string(), "A".to_string());
        let skipped: HashSet<String> = ["b".to_string()].into_iter().collect();
        let rendered = engine.render_task(&n, &results, &skipped);
        assert_eq!(rendered, "review: done");
    }

    // ===== execute(用深度超限避免实际 evorule 调用)=====

    #[tokio::test]
    async fn test_execute_validates_before_running() {
        // output_node 不存在 → execute 在校验阶段就返回 Err,不触达 delegate
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![node("a", "worker", &[])],
            output_node: "missing".to_string(),
        };
        let err = engine.execute(&wf).await.unwrap_err();
        assert!(err.contains("output_node"));
    }

    #[tokio::test]
    async fn test_execute_first_layer_node_failure_aborts() {
        // max_depth=0:第一层子任务 depth=1 >= 0 → 全部失败
        // → execute 在第一个节点失败时终止,返回 Err
        let ctx = make_ctx().with_max_depth(0);
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![node("a", "worker", &[])],
            output_node: "a".to_string(),
        };
        let err = engine.execute(&wf).await.unwrap_err();
        assert!(err.contains("node 'a' failed"));
        assert!(err.contains("max delegate depth exceeded"));
    }

    #[tokio::test]
    async fn test_execute_cycle_returns_err() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![node("a", "w", &["b"]), node("b", "w", &["a"])],
            output_node: "a".to_string(),
        };
        // validate 通过(无自依赖、依赖均存在),topological_sort 检测到环
        let err = engine.execute(&wf).await.unwrap_err();
        assert!(err.contains("cycle detected"));
    }

    #[tokio::test]
    async fn test_execute_empty_nodes() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![],
            output_node: "x".to_string(),
        };
        let err = engine.execute(&wf).await.unwrap_err();
        assert!(err.contains("no nodes"));
    }

    // ===== run_when 条件分支(workflow_dag v1.1)=====

    fn cond(node_id: &str, op: ConditionOp, value: &str) -> RunWhen {
        RunWhen {
            node: node_id.to_string(),
            op,
            value: value.to_string(),
        }
    }

    fn with_run_when(mut n: WorkflowNode, c: RunWhen) -> WorkflowNode {
        n.run_when = Some(c);
        n
    }

    // ----- 反序列化 -----

    #[test]
    fn test_workflow_deserialize_run_when() {
        let json = r#"{
            "workflow_id": "conditional_publish",
            "nodes": [
                {"id": "review", "agent_type": "reviewer", "task": "r"},
                {"id": "publish", "agent_type": "publisher", "task": "p",
                 "depends_on": ["review"],
                 "run_when": {"node": "review", "op": "contains", "value": "APPROVE"}}
            ],
            "output_node": "publish"
        }"#;
        let wf: Workflow = serde_json::from_str(json).unwrap();
        let rw = wf.nodes[1].run_when.as_ref().unwrap();
        assert_eq!(rw.node, "review");
        assert_eq!(rw.op, ConditionOp::Contains);
        assert_eq!(rw.value, "APPROVE");
        // 不含 run_when 的节点默认 None(v1.0 兼容)
        assert!(wf.nodes[0].run_when.is_none());
    }

    #[test]
    fn test_workflow_without_run_when_serializes_identically() {
        // 语义等价实证:v1.0 形态往返后不含 run_when 键
        let wf: Workflow = serde_json::from_str(
            r#"{"workflow_id":"w","nodes":[{"id":"a","agent_type":"x","task":"t"}],"output_node":"a"}"#,
        )
        .unwrap();
        let s = serde_json::to_string(&wf).unwrap();
        assert!(!s.contains("run_when"));
    }

    // ----- validate -----

    #[test]
    fn test_validate_run_when_unknown_node() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![with_run_when(
                node("a", "w", &[]),
                cond("ghost", ConditionOp::Contains, "X"),
            )],
            output_node: "a".to_string(),
        };
        let err = engine.validate(&wf).unwrap_err();
        assert!(
            err.contains("run_when references unknown node"),
            "got: {}",
            err
        );
    }

    #[test]
    fn test_validate_run_when_self_reference() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![with_run_when(
                node("a", "w", &[]),
                cond("a", ConditionOp::Contains, "X"),
            )],
            output_node: "a".to_string(),
        };
        let err = engine.validate(&wf).unwrap_err();
        assert!(err.contains("run_when references itself"), "got: {}", err);
    }

    #[tokio::test]
    async fn test_execute_rejects_same_layer_run_when_reference() {
        // run_when 引用同层节点 → 校验期拒绝(其结果在规划期不可得),
        // execute 在触达 delegate 前返回 Err
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![
                with_run_when(node("a", "w", &[]), cond("b", ConditionOp::Contains, "X")),
                with_run_when(node("b", "w", &[]), cond("a", ConditionOp::Contains, "Y")),
            ],
            output_node: "a".to_string(),
        };
        let err = engine.execute(&wf).await.unwrap_err();
        assert!(err.contains("earlier layer"), "got: {}", err);
    }

    #[test]
    fn test_run_when_coexists_with_cycle_detection() {
        // 环检测共存:带 run_when 的图仍能检出环(validate 放行,拓扑排序拒绝)
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![
                with_run_when(
                    node("a", "w", &["b"]),
                    cond("b", ConditionOp::Contains, "X"),
                ),
                with_run_when(
                    node("b", "w", &["a"]),
                    cond("a", ConditionOp::Contains, "Y"),
                ),
            ],
            output_node: "a".to_string(),
        };
        assert!(engine.validate(&wf).is_ok());
        let err = engine.topological_sort(&wf.nodes).unwrap_err();
        assert!(err.contains("cycle detected"));
    }

    // ----- eval_condition(纯函数)-----

    #[test]
    fn test_eval_condition_ops() {
        let mut results = BTreeMap::new();
        results.insert("review".to_string(), "APPROVE: looks good".to_string());
        assert!(eval_condition(
            &cond("review", ConditionOp::Contains, "APPROVE"),
            &results
        ));
        assert!(!eval_condition(
            &cond("review", ConditionOp::Contains, "REJECT"),
            &results
        ));
        assert!(eval_condition(
            &cond("review", ConditionOp::Equals, "APPROVE: looks good"),
            &results
        ));
        assert!(!eval_condition(
            &cond("review", ConditionOp::Equals, "approve"),
            &results
        ));
        assert!(eval_condition(
            &cond("review", ConditionOp::NotContains, "REJECT"),
            &results
        ));
        assert!(!eval_condition(
            &cond("review", ConditionOp::NotContains, "APPROVE"),
            &results
        ));
    }

    #[test]
    fn test_eval_condition_missing_node_treated_as_empty() {
        // 被观察节点被跳过/无结果 → 视作空字符串(确定性语义)
        let results = BTreeMap::new();
        assert!(eval_condition(
            &cond("gone", ConditionOp::Equals, ""),
            &results
        ));
        assert!(eval_condition(
            &cond("gone", ConditionOp::NotContains, "X"),
            &results
        ));
        assert!(!eval_condition(
            &cond("gone", ConditionOp::Contains, "X"),
            &results
        ));
        assert!(!eval_condition(
            &cond("gone", ConditionOp::Equals, "X"),
            &results
        ));
    }

    #[test]
    fn test_eval_condition_deterministic() {
        // 确定性实证:同一输入重复求值 N 次,结果一致
        let mut results = BTreeMap::new();
        results.insert("a".to_string(), "hello world".to_string());
        let c = cond("a", ConditionOp::Contains, "world");
        let expected = eval_condition(&c, &results);
        for _ in 0..100 {
            assert_eq!(eval_condition(&c, &results), expected);
        }
    }

    // ----- plan_layer(纯函数:跳过/级联/豁免/混合)-----

    #[test]
    fn test_plan_layer_run_when_false_skips() {
        // 混合层:无条件的 review 执行,条件为假的 publish 跳过
        let nodes = [
            node("review", "reviewer", &[]),
            with_run_when(
                node("publish", "publisher", &["review"]),
                cond("review", ConditionOp::Contains, "APPROVE"),
            ),
        ];
        let layer: Vec<&WorkflowNode> = nodes.iter().collect();
        let mut results = BTreeMap::new();
        results.insert("review".to_string(), "REJECT: bad".to_string());
        let (to_run, newly_skipped) = plan_layer(&layer, &results, &HashSet::new());
        assert_eq!(to_run.len(), 1);
        assert_eq!(to_run[0].id, "review");
        assert_eq!(newly_skipped, vec!["publish".to_string()]);
    }

    #[test]
    fn test_plan_layer_run_when_true_runs() {
        let nodes = [
            node("review", "reviewer", &[]),
            with_run_when(
                node("publish", "publisher", &["review"]),
                cond("review", ConditionOp::Contains, "APPROVE"),
            ),
        ];
        let layer: Vec<&WorkflowNode> = nodes.iter().collect();
        let mut results = BTreeMap::new();
        results.insert("review".to_string(), "APPROVE: ok".to_string());
        let (to_run, newly_skipped) = plan_layer(&layer, &results, &HashSet::new());
        assert!(newly_skipped.is_empty());
        assert_eq!(to_run.len(), 2);
    }

    #[test]
    fn test_plan_layer_cascades_to_downstream() {
        // 级联:b 已被跳过 → 无 run_when 的下游 c 级联跳过;
        // 豁免:d 有自己的 run_when 且条件为真 → 照常执行
        let nodes = [
            node("c", "w", &["b"]),
            with_run_when(
                node("d", "w", &["b"]),
                cond("a", ConditionOp::Contains, "GO"),
            ),
        ];
        let layer: Vec<&WorkflowNode> = nodes.iter().collect();
        let skipped: HashSet<String> = ["b".to_string()].into_iter().collect();
        let mut results = BTreeMap::new();
        results.insert("a".to_string(), "GO".to_string());
        let (to_run, newly_skipped) = plan_layer(&layer, &results, &skipped);
        assert_eq!(to_run.len(), 1);
        assert_eq!(to_run[0].id, "d");
        assert_eq!(newly_skipped, vec!["c".to_string()]);
    }

    #[test]
    fn test_plan_layer_no_run_when_v10_behavior_identical() {
        // 语义等价实证:无 run_when 且无跳过 → 分划与 v1.0 逐层执行完全一致
        // (全执行、保持原序、跳过集为空)
        let nodes = [node("a", "w", &[]), node("b", "w", &[])];
        let layer: Vec<&WorkflowNode> = nodes.iter().collect();
        let (to_run, newly_skipped) = plan_layer(&layer, &BTreeMap::new(), &HashSet::new());
        assert!(newly_skipped.is_empty());
        let ids: Vec<&str> = to_run.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn test_plan_layer_deterministic() {
        // 确定性实证:同一含条件层重复规划 N 次,分划一致
        let nodes = [with_run_when(
            node("notify", "w", &["publish"]),
            cond("review", ConditionOp::Contains, "APPROVE"),
        )];
        let layer: Vec<&WorkflowNode> = nodes.iter().collect();
        let skipped: HashSet<String> = ["publish".to_string()].into_iter().collect();
        let mut results = BTreeMap::new();
        results.insert("review".to_string(), "APPROVE".to_string());
        let first = plan_layer(&layer, &results, &skipped);
        assert_eq!(first.0.len(), 1); // 豁免级联:review 含 APPROVE → notify 照常执行
        assert_eq!(first.0[0].id, "notify");
        for _ in 0..100 {
            let again = plan_layer(&layer, &results, &skipped);
            assert_eq!(first.0.len(), again.0.len());
            assert_eq!(first.0[0].id, again.0[0].id);
            assert_eq!(first.1, again.1);
        }
    }

    // ===== 粒间契约（契约 v0）=====

    #[test]
    fn test_extract_template_refs_edges() {
        assert!(extract_template_refs("no placeholders").is_empty());
        assert_eq!(extract_template_refs("a {x} b {y} {x}"), vec!["x", "y"]);
        // JSON 字面量 / 路径式 / 含空格 / 空 / 未闭合：均不算引用
        assert!(extract_template_refs(r#"{"k": 1}"#).is_empty());
        assert!(extract_template_refs("{prev.X} {a b} {} {unclosed").is_empty());
        assert_eq!(extract_template_refs("{a_1}-{B-2}"), vec!["a_1", "B-2"]);
    }

    #[test]
    fn test_validate_granule_contracts_non_contracted_ok() {
        // 存量形态：无任何 output_schema → 零影响放行
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![node("a", "w", &[]), node("b", "w", &["a"])],
            output_node: "b".to_string(),
        };
        assert!(validate_granule_contracts(&wf).is_ok());
    }

    #[test]
    fn test_validate_granule_contracts_accepts_complete() {
        let schema = serde_json::json!({"type": "object"});
        let mut a = node("a", "w", &[]);
        a.output_schema = Some(schema.clone());
        let mut b = node("b", "w", &["a"]);
        b.task_template = Some("based on: {a}".to_string());
        b.output_schema = Some(schema);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![a, b],
            output_node: "b".to_string(),
        };
        assert!(validate_granule_contracts(&wf).is_ok());
    }

    #[test]
    fn test_validate_granule_contracts_rejects_missing_upstream_schema() {
        let mut b = node("b", "w", &["a"]);
        b.task_template = Some("based on: {a}".to_string());
        b.output_schema = Some(serde_json::json!({"type": "object"}));
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![node("a", "w", &[]), b],
            output_node: "b".to_string(),
        };
        let errs = validate_granule_contracts(&wf).expect_err("缺契约不上阵须拒载");
        assert!(
            errs.iter()
                .any(|e| e.contains("契约态拒载") && e.contains("'a'")),
            "错误须指明缺口与被引用节点: {errs:?}"
        );
    }

    #[test]
    fn test_validate_granule_contracts_rejects_invalid_schema() {
        let mut a = node("a", "w", &[]);
        // jsonschema 应拒绝的非法 schema（type 值非法）
        a.output_schema = Some(serde_json::json!({"type": 42}));
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![a],
            output_node: "a".to_string(),
        };
        let errs = validate_granule_contracts(&wf).expect_err("非法 schema 须拒载");
        assert!(errs[0].contains("output_schema 编译失败"), "{errs:?}");
    }

    #[test]
    fn test_validate_node_output_against_schema() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["verdict"],
            "properties": {"verdict": {"type": "string"}}
        });
        // 合法 JSON 且过校验
        assert!(validate_node_output_against_schema(&schema, r#"{"verdict": "ok"}"#).is_ok());
        // 非 JSON
        let err = validate_node_output_against_schema(&schema, "not json").unwrap_err();
        assert!(err.contains("not valid JSON"), "{err}");
        // JSON 但类型违规：消息含实例路径
        let err = validate_node_output_against_schema(&schema, r#"{"verdict": 1}"#).unwrap_err();
        assert!(err.contains("verdict"), "{err}");
        // 缺必填字段
        let err = validate_node_output_against_schema(&schema, r#"{}"#).unwrap_err();
        assert!(err.contains("verdict"), "{err}");
    }

    #[test]
    fn test_interface_contract_error_wraps_parseable_prefix() {
        let text = interface_contract_error("s", "boom");
        assert!(text.contains(INTERFACE_CONTRACT_MARKER), "{text}");
        assert_eq!(
            crate::agent::replan::parse_failed_node_id(&text).as_deref(),
            Some("s"),
            "包裹形态须可被外层失败解析"
        );
    }

    // ===== 节点判据（判据 v0 第二级）=====

    #[test]
    fn test_judge_signal_shape_pass_and_fail() {
        // 过：acceptance_passed=true 恒带；无 detail 字段
        let ok = judge_signal("n_writer", true, "ignored");
        assert_eq!(ok["type"], "set");
        assert_eq!(ok["params"]["attr"], "meta_signal.judge");
        assert_eq!(ok["params"]["operation"], "set");
        assert_eq!(ok["params"]["value"], "n_writer");
        assert_eq!(ok["params"]["acceptance_passed"], true);
        assert!(ok["params"].get("acceptance_detail").is_none());
        // 不过：acceptance_passed=false + detail 强制注入（引擎报告事实，
        // 处置知识在规则层——不采信 LLM 自报的机制同源）
        let bad = judge_signal("n_writer", false, "exit=1 stdout_tail=boom");
        assert_eq!(bad["params"]["acceptance_passed"], false);
        assert_eq!(
            bad["params"]["acceptance_detail"],
            "exit=1 stdout_tail=boom"
        );
        // 确定性：同输入必同输出
        assert_eq!(ok, judge_signal("n_writer", true, "ignored"));
        assert_eq!(
            bad,
            judge_signal("n_writer", false, "exit=1 stdout_tail=boom")
        );
    }

    fn judge_probe_output(cmd_args: &[&str]) -> std::process::Output {
        #[cfg(windows)]
        let mut c = std::process::Command::new("cmd");
        #[cfg(windows)]
        let c = c.args(["/C"]).args(cmd_args);
        #[cfg(not(windows))]
        let mut c = std::process::Command::new("sh");
        #[cfg(not(windows))]
        let c = c.args(["-c"]).args(cmd_args);
        c.output().expect("判据裁决探针命令须可执行")
    }

    #[test]
    fn test_judge_verdict_exit_and_stdout() {
        // exit 7 + stdout "hello"：expect_exit=7 判过；expect_exit=0 判不过
        let output = judge_probe_output(&["echo hello & exit 7"]);
        let (passed, detail) = judge_verdict(
            &JudgeSpec {
                command: "probe".into(),
                expect_exit: 7,
                expect_stdout: None,
            },
            &output,
        );
        assert!(passed, "exit 匹配判过: {detail}");
        assert!(detail.contains("exit=7"), "detail 含退出码审计: {detail}");
        assert!(
            detail.contains("hello"),
            "detail 含 stdout 尾审计: {detail}"
        );

        let (passed, _) = judge_verdict(
            &JudgeSpec {
                command: "probe".into(),
                expect_exit: 0,
                expect_stdout: None,
            },
            &output,
        );
        assert!(!passed, "exit 不匹配判不过");

        // expect_stdout contains 语义：声明子串在 stdout=过；不在=不过
        let output = judge_probe_output(&["echo JUDGE-ANCHOR-OK-12345"]);
        let (passed, _) = judge_verdict(
            &JudgeSpec {
                command: "probe".into(),
                expect_exit: 0,
                expect_stdout: Some("JUDGE-ANCHOR-OK".into()),
            },
            &output,
        );
        assert!(passed, "stdout 含期望子串判过");
        let (passed, _) = judge_verdict(
            &JudgeSpec {
                command: "probe".into(),
                expect_exit: 0,
                expect_stdout: Some("MISSING-NEEDLE".into()),
            },
            &output,
        );
        assert!(!passed, "stdout 不含期望子串判不过（exit 匹配也不行）");
    }

    #[tokio::test]
    async fn test_run_judge_command_host_direct() {
        // 宿主直执行（None）：exit 0 / 非零透传语义（H2 门卫后:探针命令须在
        // 白名单——统一探针用 git rev-parse --verify 不存在 ref:白名单内
        // （只读）、双平台确定 exit 128、无引号无括号、不依赖 PATH 有无
        // sh/python。引号搅碎缺陷已由 raw_arg 直传修复（Windows 引号保真
        // 回归见下一测试）；本探针保持无引号只为跨平台极简）
        let out = run_judge_command("echo ok", None)
            .await
            .expect("echo 须成功 spawn");
        assert_eq!(out.status.code(), Some(0));
        assert!(String::from_utf8_lossy(&out.stdout).contains("ok"));
        let out = run_judge_command("git rev-parse --verify no-such-ref-9q8z7", None)
            .await
            .expect("git 须成功 spawn");
        assert_eq!(out.status.code(), Some(128));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn test_run_judge_command_host_direct_quote_preserved() {
        // Windows 宿主直执行引号保真回归：判据串内层双引号必须原样抵达
        // 子进程。此前 arg() 转义 × cmd 不认转义把引号搅碎（同探针实测返
        // 0/返 1），raw_arg 直传修复后应得真实退出码 7（python 在 PATH 的
        // Windows 宿主上确定成立）
        let out = run_judge_command("python -c \"exit(7)\"", None)
            .await
            .expect("python 须成功 spawn");
        assert_eq!(out.status.code(), Some(7));
    }

    #[tokio::test]
    async fn test_run_judge_command_container_name_validated() {
        // 容器名过白名单校验（防 argv 注入）：非法名 = Err 携带原因（判据不过路径）
        // H2 门卫后:探针命令须在白名单内,否则错误文本混入 H2 拒绝(测不到容器名闸)
        let err = run_judge_command("echo probe", Some("bad name with spaces"))
            .await
            .expect_err("非法容器名须 Err");
        assert!(
            err.contains("judge container name invalid"),
            "错误须指明容器名非法: {err}"
        );
    }

    #[test]
    fn test_workflow_deserialize_judge_spec() {
        // 带 judge：解析为 Some(JudgeSpec)，expect_exit 缺省 0
        let json = r#"{
            "workflow_id": "judged",
            "nodes": [
                {"id": "build", "agent_type": "writer", "task": "t",
                 "judge": {"command": "cargo build", "expect_stdout": "Finished"}}
            ],
            "output_node": "build"
        }"#;
        let wf: Workflow = serde_json::from_str(json).unwrap();
        assert_eq!(
            wf.nodes[0].judge,
            Some(JudgeSpec {
                command: "cargo build".to_string(),
                expect_exit: 0,
                expect_stdout: Some("Finished".to_string()),
            })
        );
        // 不带 judge（存量形态）：default = None，零影响
        let json = r#"{
            "workflow_id": "plain",
            "nodes": [{"id": "only", "agent_type": "writer", "task": "t"}],
            "output_node": "only"
        }"#;
        let wf: Workflow = serde_json::from_str(json).unwrap();
        assert!(wf.nodes[0].judge.is_none());
    }

    #[test]
    fn test_workflow_node_judge_serde_roundtrip() {
        // judge=None 序列化不产生 "judge" 键（skip_serializing_if）
        let n = node("a", "w", &[]);
        let json = serde_json::to_value(&n).unwrap();
        assert!(
            json.get("judge").is_none(),
            "None judge 不得出现在序列化产物"
        );
        // judge=Some 往返保真
        let mut n = node("a", "w", &[]);
        n.judge = Some(JudgeSpec {
            command: "findstr needle data.txt".to_string(),
            expect_exit: 0,
            expect_stdout: None,
        });
        let back: WorkflowNode = serde_json::from_value(serde_json::to_value(&n).unwrap()).unwrap();
        assert_eq!(back.judge, n.judge);
    }

    #[test]
    fn test_validate_compute_and_judge_mutually_exclusive() {
        // 互斥门卫：compute 与 judge 同节点声明 = 拒载
        let mut n = node("a", "w", &[]);
        n.compute = Some(ComputeSpec::Strcmp {
            inputs: vec![ComputeInput::Node("a".to_string()), ComputeInput::Empty],
            mode: StrcmpMode::Equal,
        });
        n.judge = Some(JudgeSpec {
            command: "exit 0".to_string(),
            expect_exit: 0,
            expect_stdout: None,
        });
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![n],
            output_node: "a".to_string(),
        };
        let engine = WorkflowEngine::new(make_ctx());
        let err = engine.validate(&wf).expect_err("compute/judge 互斥须拒载");
        assert!(
            err.contains("cannot declare both compute and judge"),
            "错误须指明互斥: {err}"
        );
    }
}
