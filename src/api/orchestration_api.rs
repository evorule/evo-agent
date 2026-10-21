// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! B25 编排可视化数据出口(谱系只读端点)
//!
//! 编排引擎(delegate / session_spawn)既有,本模块零引擎触碰:只读 journal
//! 落账(`data/sessions/{sid}.jsonl`),聚合为工作台编排面板与会话列表消费
//! 的谱系投影。
//!
//! - `GET /api/orchestration/sessions?session_id=X`:X 会话的委托/派生树
//!   (journal 为权威谱系;根节点 + 子节点 + 边,子节点状态由其 journal
//!   尾部 `turn_ended` 推断,v1 允许 unknown);
//! - `GET /api/orchestration/sessions`(缺省 session_id):全量子会话归属链
//!   (会话列表「委托自」徽标数据源;单遍扫描全部 journal,坏账跳过);
//! - `GET /api/orchestration/agents`:可委托 agent 清单(名称 + 描述 +
//!   tools 声明里是否含 `delegate`,供图例/下拉)。
//!
//! 只读面:零写盘;journal 读取统一 [`read_all_tolerant`](journal 容忍尾部
//! 半行——活会话在写/强杀尾部半行是自然产物;中间损坏 fail-visible,树端点
//! 上抛 404/500,全局链端点跳过该文件不拖垮整表)。

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::agent::journal::{self, JournalEvent, JournalLine, JournalWriter};
use crate::api::agent_api::AgentApiState;

/// 会话目录约定(与 runner 注入 journal_dir 同源,见 get_atif 同款装配)
fn journal_dir_of(state: &AgentApiState) -> std::path::PathBuf {
    state.workdir().join("data").join("sessions")
}

// ---------------------------------------------------------------------------
// 响应形状
// ---------------------------------------------------------------------------

/// 谱系树节点(根节点 parent/kind 缺省;agent_type 由事件锚或消费方回填)
#[derive(Debug, Serialize)]
pub struct OrchestrationNode {
    /// 会话 id(临时/历史子会话均以此为主键)
    pub session_id: String,
    /// 代理类型(根节点可空,消费方以当前会话口径回填)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// 父会话 id(根节点无)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// 派生方式:"delegate" | "spawn"(根节点无)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// 深度(根 = 0;口径与 journal 落账一致)
    pub depth: u32,
    /// 派生落账时间(unix ms;根节点无)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spawned_at: Option<u64>,
    /// 状态:done | failed | unknown(journal 尾部推断,v1 允许 unknown)
    pub status: String,
}

/// 谱系边(父 → 子,携带派生方式)
#[derive(Debug, Serialize)]
pub struct OrchestrationEdge {
    /// 父会话 id
    pub parent: String,
    /// 子会话 id
    pub child: String,
    /// 派生方式:"delegate" | "spawn"
    pub kind: String,
}

/// `GET /api/orchestration/sessions?session_id=X` 响应
#[derive(Debug, Serialize)]
pub struct OrchestrationTree {
    /// 被追溯的会话 id
    pub session_id: String,
    /// 谱系节点列表(含根与全部祖先/子孙)
    pub nodes: Vec<OrchestrationNode>,
    /// 谱系边列表(父 → 子,携带派生方式)
    pub edges: Vec<OrchestrationEdge>,
}

/// 全局归属链单条(子 → 父;会话列表「委托自」徽标数据源)
#[derive(Debug, Serialize)]
pub struct OrchestrationLink {
    /// 父会话 id(journal 归属会话或 SessionSpawned.parent)
    pub parent: String,
    /// 子会话 id
    pub child: String,
    /// 子代理类型(spawn 锚为空串)
    pub agent_type: String,
    /// 派生方式:"delegate" | "spawn"
    pub kind: String,
    /// 深度(根 = 0)
    pub depth: u32,
}

/// `GET /api/orchestration/sessions`(全局模式)响应
#[derive(Debug, Serialize)]
pub struct OrchestrationLinks {
    /// 全局归属链列表(子 → 父)
    pub links: Vec<OrchestrationLink>,
}

/// 可委托 agent 清单单条
#[derive(Debug, Serialize)]
pub struct OrchestrationAgent {
    /// agent 类型名(定义文件名去扩展)
    pub agent_type: String,
    /// agent 定义一句话描述(definition.description)
    pub description: String,
    /// 定义 tools 列表是否声明 `delegate`(即该 agent 自身可再委托)
    pub can_delegate: bool,
}

/// `GET /api/orchestration/agents` 响应
#[derive(Debug, Serialize)]
pub struct OrchestrationAgents {
    /// 可委托 agent 清单(名称升序)
    pub agents: Vec<OrchestrationAgent>,
}

/// 编排面查询参数
#[derive(Debug, Deserialize)]
pub struct OrchestrationQuery {
    /// 目标会话 id;缺省 = 全局归属链模式
    #[serde(default)]
    pub session_id: Option<String>,
}

// ---------------------------------------------------------------------------
// 聚合逻辑(纯函数,单测覆盖)
// ---------------------------------------------------------------------------

/// 单条派生锚:父 journal 内 DelegateSpawned / SessionSpawned 归一形态
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SpawnAnchor {
    pub parent: String,
    pub child: String,
    pub agent_type: String,
    /// "delegate" | "spawn"
    pub kind: &'static str,
    pub depth: u32,
    /// 落账时间(unix ms)
    pub ts: u64,
    /// 父 journal 内 seq(时序)
    pub seq: u64,
}

/// 扫描单份 journal 内全部派生锚(按 seq 升序;parent 即 journal 归属会话)
pub(crate) fn scan_spawn_anchors(journal: &[JournalLine], parent_sid: &str) -> Vec<SpawnAnchor> {
    journal
        .iter()
        .filter_map(|line| match &line.event {
            JournalEvent::DelegateSpawned {
                child_session_id,
                agent_type,
                depth,
                ..
            } => Some(SpawnAnchor {
                parent: parent_sid.to_string(),
                child: child_session_id.clone(),
                agent_type: agent_type.clone(),
                kind: "delegate",
                depth: u32::try_from(*depth).unwrap_or(u32::MAX),
                ts: line.ts,
                seq: line.seq,
            }),
            JournalEvent::SessionSpawned {
                parent,
                child,
                depth,
            } => Some(SpawnAnchor {
                parent: parent.clone(),
                child: child.clone(),
                agent_type: String::new(),
                kind: "spawn",
                depth: *depth,
                ts: line.ts,
                seq: line.seq,
            }),
            _ => None,
        })
        .collect()
}

/// 子节点状态推断:末次 `turn_ended` 决定(success→done;error/cancelled/
/// aborted→failed;其余/无轮终态→unknown——活会话在跑或无轮次均归 unknown,
/// v1 口径不区分 running)
pub(crate) fn infer_status(journal: &[JournalLine]) -> &'static str {
    for line in journal.iter().rev() {
        if let JournalEvent::TurnEnded { status, .. } = &line.event {
            return match status.as_str() {
                "success" => "done",
                "error" | "cancelled" | "aborted" => "failed",
                _ => "unknown",
            };
        }
    }
    "unknown"
}

/// 锚集合 → 谱系树。`status_of` 由调用方装配(子 journal 尾读);节点按
/// (depth, seq) 稳定排序,根节点恒列首。
pub(crate) fn build_tree(
    root_sid: &str,
    root_status: &str,
    anchors: &[SpawnAnchor],
    status_of: &dyn Fn(&str) -> String,
) -> OrchestrationTree {
    let mut nodes = vec![OrchestrationNode {
        session_id: root_sid.to_string(),
        agent_type: None,
        parent: None,
        kind: None,
        depth: 0,
        spawned_at: None,
        status: root_status.to_string(),
    }];
    let mut sorted: Vec<&SpawnAnchor> = anchors.iter().collect();
    sorted.sort_by_key(|a| (a.depth, a.seq));
    let mut edges = Vec::with_capacity(sorted.len());
    for a in sorted {
        nodes.push(OrchestrationNode {
            session_id: a.child.clone(),
            agent_type: if a.agent_type.is_empty() {
                None
            } else {
                Some(a.agent_type.clone())
            },
            parent: Some(a.parent.clone()),
            kind: Some(a.kind.to_string()),
            depth: a.depth,
            spawned_at: Some(a.ts),
            status: status_of(&a.child),
        });
        edges.push(OrchestrationEdge {
            parent: a.parent.clone(),
            child: a.child.clone(),
            kind: a.kind.to_string(),
        });
    }
    OrchestrationTree {
        session_id: root_sid.to_string(),
        nodes,
        edges,
    }
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

/// `GET /api/orchestration/sessions`
///
/// 带 `session_id` → 该会话谱系树;journal 缺失/为空 → 404(与 get_atif 同款
/// fail-visible)。缺省 → 全局归属链(坏账 journal 跳过,不拖垮整表)。
pub async fn list_orchestration_sessions(
    State(state): State<AgentApiState>,
    Query(q): Query<OrchestrationQuery>,
) -> Result<Response, (StatusCode, String)> {
    let dir = journal_dir_of(&state);
    match q.session_id.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(sid) => {
            let path = JournalWriter::path_for(&dir, sid);
            let journal = journal::read_all_tolerant(&path).map_err(|e| {
                (
                    StatusCode::NOT_FOUND,
                    format!("orchestration: journal unavailable for session '{sid}': {e}"),
                )
            })?;
            if journal.is_empty() {
                return Err((
                    StatusCode::NOT_FOUND,
                    format!("orchestration: journal is empty for session '{sid}'"),
                ));
            }
            let anchors = scan_spawn_anchors(&journal, sid);
            // 子 journal 尾读装配(缺文件/坏账 → unknown,不拖垮整树)
            let status_of = |child: &str| -> String {
                journal::read_all_tolerant(&JournalWriter::path_for(&dir, child))
                    .map(|lines| infer_status(&lines).to_string())
                    .unwrap_or_else(|_| "unknown".to_string())
            };
            let tree = build_tree(sid, infer_status(&journal), &anchors, &status_of);
            Ok(Json(tree).into_response())
        }
        None => {
            let mut links: Vec<OrchestrationLink> = Vec::new();
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => return Ok(Json(OrchestrationLinks { links }).into_response()),
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !name.ends_with(".jsonl") || name.ends_with(".wire-archive.jsonl") {
                    continue; // 只消费主 journal,绕开 wire-archive sidecar
                }
                let parent_sid = name.trim_end_matches(".jsonl").to_string();
                let Ok(lines) = journal::read_all_tolerant(&path) else {
                    continue; // 单文件坏账:跳过该父,不拖垮整表
                };
                for a in scan_spawn_anchors(&lines, &parent_sid) {
                    links.push(OrchestrationLink {
                        parent: a.parent,
                        child: a.child,
                        agent_type: a.agent_type,
                        kind: a.kind.to_string(),
                        depth: a.depth,
                    });
                }
            }
            Ok(Json(OrchestrationLinks { links }).into_response())
        }
    }
}

/// `GET /api/orchestration/agents` —— 可委托 agent 清单
///
/// 定义清单枚举失败 → 空列表(与 list_agents 同款 fail-soft;徽标是辅助面)。
pub async fn list_orchestration_agents(
    State(state): State<AgentApiState>,
) -> Json<OrchestrationAgents> {
    let mut agents = Vec::new();
    if let Ok(types) = state.definitions().list_types() {
        for t in types {
            if let Ok(def) = state.definitions().load(&t) {
                agents.push(OrchestrationAgent {
                    agent_type: def.agent_type,
                    description: def.description,
                    can_delegate: def.tools.iter().any(|tool| tool == "delegate"),
                });
            }
        }
        agents.sort_by(|a, b| a.agent_type.cmp(&b.agent_type));
    }
    Json(OrchestrationAgents { agents })
}

// ---------------------------------------------------------------------------
// 单测(纯函数面)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造 journal 行(裸事件 + 自增 seq/ts)
    fn line(seq: u64, ts: u64, event: JournalEvent) -> JournalLine {
        JournalLine {
            seq,
            ts,
            event,
            hashes: None,
        }
    }

    fn anchor(parent: &str, child: &str, kind: &'static str, depth: u32, seq: u64) -> SpawnAnchor {
        SpawnAnchor {
            parent: parent.to_string(),
            child: child.to_string(),
            agent_type: if kind == "spawn" {
                String::new()
            } else {
                format!("{kind}-agent")
            },
            kind,
            depth,
            ts: 1000 + seq,
            seq,
        }
    }

    #[test]
    fn scan_spawn_anchors_finds_delegate_and_spawn_in_order() {
        let journal = vec![
            line(
                1,
                10,
                JournalEvent::TurnStarted {
                    goal: "g".into(),
                    turn_seq: 1,
                },
            ),
            line(
                2,
                20,
                JournalEvent::DelegateSpawned {
                    child_session_id: "c1".into(),
                    agent_type: "researcher".into(),
                    depth: 1,
                    task_digest: "d1".into(),
                },
            ),
            line(
                3,
                30,
                JournalEvent::SessionSpawned {
                    parent: "root".into(),
                    child: "c2".into(),
                    depth: 2,
                },
            ),
            line(
                4,
                40,
                JournalEvent::TurnEnded {
                    status: "success".into(),
                    steps: 3,
                    duration_ms: 9,
                },
            ),
        ];
        let anchors = scan_spawn_anchors(&journal, "root");
        assert_eq!(anchors.len(), 2);
        assert_eq!(
            anchors[0],
            SpawnAnchor {
                parent: "root".into(),
                child: "c1".into(),
                agent_type: "researcher".into(),
                kind: "delegate",
                depth: 1,
                ts: 20,
                seq: 2,
            }
        );
        assert_eq!(anchors[1].child, "c2");
        assert_eq!(anchors[1].kind, "spawn");
        assert_eq!(anchors[1].agent_type, ""); // spawn 锚不带 agent_type
    }

    #[test]
    fn infer_status_last_turn_ended_wins() {
        let journal = vec![
            line(
                1,
                10,
                JournalEvent::TurnEnded {
                    status: "success".into(),
                    steps: 1,
                    duration_ms: 1,
                },
            ),
            line(
                2,
                20,
                JournalEvent::TurnEnded {
                    status: "error".into(),
                    steps: 2,
                    duration_ms: 2,
                },
            ),
        ];
        assert_eq!(infer_status(&journal), "failed");
    }

    #[test]
    fn infer_status_no_turn_ended_is_unknown() {
        let journal = vec![line(
            1,
            10,
            JournalEvent::TurnStarted {
                goal: "g".into(),
                turn_seq: 1,
            },
        )];
        assert_eq!(infer_status(&journal), "unknown");
        assert_eq!(infer_status(&[]), "unknown");
    }

    #[test]
    fn build_tree_root_first_and_edges_carry_kind() {
        let anchors = vec![
            anchor("root", "d1", "delegate", 1, 2),
            anchor("root", "s1", "spawn", 2, 3),
            anchor("root", "d2", "delegate", 1, 1),
        ];
        let tree = build_tree("root", "done", &anchors, &|_| "done".to_string());
        assert_eq!(tree.session_id, "root");
        assert_eq!(tree.nodes.len(), 4);
        // 根节点恒列首,parent/kind 缺省
        assert_eq!(tree.nodes[0].session_id, "root");
        assert_eq!(tree.nodes[0].parent, None);
        assert_eq!(tree.nodes[0].kind, None);
        assert_eq!(tree.nodes[0].status, "done");
        // 同深度按 seq 排序:d2(seq1) 先于 d1(seq2)
        assert_eq!(tree.nodes[1].session_id, "d2");
        assert_eq!(tree.nodes[1].agent_type.as_deref(), Some("delegate-agent"));
        assert_eq!(tree.nodes[1].depth, 1);
        assert_eq!(tree.nodes[2].session_id, "d1");
        assert_eq!(tree.nodes[3].session_id, "s1");
        assert_eq!(tree.nodes[3].agent_type, None); // spawn 锚 agent_type 缺省
        assert_eq!(tree.edges.len(), 3);
        assert_eq!(tree.edges[0].kind, "delegate");
        assert_eq!(tree.edges[2].kind, "spawn");
        // status_of 闭包消费(逐子节点)
        assert_eq!(tree.nodes[1].status, "done");
    }
}
