// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 代码库语义索引（B14，结构面 v1 + 语义面 v1.x）—— 对工作区源码建符号图谱
//! 与 chunk 向量面，并提供统一检索入口 `/api/index/*`（search/status/rebuild）。
//!
//! 模块六件（PR-5 语义面，与 [`crate::mcp`] 独立子系统先例同款组织）：
//! - [`store`]：SQLite 持久层（files/symbols/idents/refs/chunks/vectors，单写者纪律）；
//! - [`chunker`]：工作区遍历（双层 ignore）+ 内容哈希；
//! - [`symbols`]：tree-sitter 符号抽取 + chunk 切分；
//! - [`graph`]：PageRank 图谱检索排序；
//! - [`embedder`]：语义面文本嵌入器抽象（fastembed 实现按 `semantic` feature
//!   按需编译，缺省构建不编译）；
//! - [`search_api`]：HTTP 处理器；本文件承载 [`IndexService`] 编排层。
//!
//! 治理面：serve 本地后台行为，无 agent 工具执行、无 shell/写
//! 工作区动作——无审批接点；落盘只存哈希+符号+元数据（D8，源码明文不落索引
//! 库）；v1 零外发；`index.enabled` 设置键（B6 schema 单源注册）关闭时
//! `/api/index/*` 返回 503（服务未注入即关，随 PR-4 接入设置键）。
//!
//! 索引库为可丢弃派生缓存（`rebuild` 随时重建），不是第二真相源。
//!
//! ## 增量口径说明（v1 残差声明）
//!
//! 全量遍历/对账走 [`chunker::collect_workspace_files`]（双层 ignore 完整口径）；
//! watcher 增量批次走 [`is_incremental_candidate`] 轻过滤——语言扩展名 +
//! hidden/默认排除目录组件 + 大小护栏。轻过滤与全量口径的残差仅在用户自定义
//! `.evoignore`/`.evoindexingignore` 排除的源码文件被增量收录（符号与哈希入
//! 库，非源码明文），下次全量对账自动校正；扩展名黑名单与 hidden 口径在两侧
//! 等价生效（SKIP_EXTS 与密钥类扩展均不在语言路由表内，天然双拦）。

pub mod chunker;
pub mod embedder;
pub mod graph;
pub mod search_api;
pub mod store;
pub mod symbols;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use tracing::{info, warn};

use crate::index::chunker::{FileEntry, MAX_FILE_BYTES};
use crate::index::embedder::TextEmbedder;
use crate::index::graph::SymbolGraph;
use crate::index::search_api::{rrf_fuse, HybridResult};
use crate::index::store::{ChunkRow, IndexError};

pub use chunker::{collect_workspace_files, DEFAULT_EXCLUDES, SKIP_EXTS};
pub use store::{ChunkVectorRow, FileHash, IndexStore, RefRow, SymbolMin, SymbolRow};

/// watcher 去抖窗口（毫秒；fs_watch 400ms 的编辑场景近似值，索引侧收敛略缓）
pub const INDEX_DEBOUNCE_MS: u64 = 500;
/// 增量更新并发批上限（超出即丢弃当批——store 单写者队列防积压；
/// 丢批由下次全量对账兜底校正）
const MAX_IN_FLIGHT: u64 = 4;
/// hybrid 词法侧 grep 结果数上限（文件聚合前的行级命中预算；B2 REST 面同域量级）
const HYBRID_LEXICAL_MAX_RESULTS: usize = 1000;
/// hybrid 词法侧参与融合的文件数上限（RRF 输入排位深度；融合输出再按 limit 截断）
const HYBRID_LEXICAL_MAX_FILES: usize = 200;

/// 索引落盘目录：`<用户主目录>/.evo-agent/index/<workspace-hash16>/`
///
/// workspace 哈希 = workdir 规范化路径 blake3 前 16 hex——同一工作区跨启动
/// 稳定，不同工作区互不串库。
pub fn index_dir(workdir: &Path) -> Result<PathBuf, IndexError> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map_err(|_| IndexError("user home dir not found (USERPROFILE/HOME)".into()))?;
    let canonical = workdir
        .canonicalize()
        .map_err(|e| IndexError(format!("canonicalize workdir: {e}")))?;
    let hex = blake3::hash(canonical.to_string_lossy().as_bytes()).to_hex();
    Ok(PathBuf::from(home)
        .join(".evo-agent")
        .join("index")
        .join(&hex[..16]))
}

/// 索引库文件路径：[`index_dir`] 下的 `index.db`
pub fn index_db_path(workdir: &Path) -> Result<PathBuf, IndexError> {
    Ok(index_dir(workdir)?.join("index.db"))
}

/// 索引服务状态快照（status 端点响应体）
#[derive(Debug, Clone, serde::Serialize)]
pub struct IndexStatusSnapshot {
    /// 已索引文件数
    pub files: u64,
    /// 符号总数
    pub symbols: u64,
    /// 引用边数
    pub refs: u64,
    /// chunk 总数（边界元数据；语义面就绪度输入）
    pub chunks: u64,
    /// 已产出向量数
    pub vectors: u64,
    /// 重建进行中
    pub building: bool,
    /// 图谱已就绪（可检索）
    pub ready: bool,
    /// 最近一次派生刷新时刻（UNIX 秒）
    pub last_build_epoch: Option<u64>,
    /// 最近一次重建错误（fail-soft 留痕；成功后清除）
    pub last_error: Option<String>,
    /// 语义面是否启用（嵌入器已装配）
    pub semantic_enabled: bool,
    /// 语义面模型名（未启用不下发）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic_model: Option<String>,
}

/// 语义检索单命中（chunk 粒度，cosine 降序；search 端点 mode=semantic 直出）
#[derive(Debug, Clone, serde::Serialize)]
pub struct SemanticHit {
    /// 所属文件（相对路径，`/` 分隔）
    pub file: String,
    /// 文件内 chunk 序号
    pub ord: u32,
    /// 所属符号名
    pub symbol: String,
    /// 符号类别
    pub kind: String,
    /// 起始行（1 起，含）
    pub start_line: u32,
    /// 结束行（含）
    pub end_line: u32,
    /// cosine 相似度
    pub score: f32,
}

/// cosine 相似度（dim 不符 → None 防御性丢弃，与 blob 守卫同口径；零范数 → 0.0）
fn cosine(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() {
        return None;
    }
    let (dot, na, nb) = a
        .iter()
        .zip(b)
        .fold((0.0f32, 0.0f32, 0.0f32), |(d, x, y), (&p, &q)| {
            (d + p * q, x + p * p, y + q * q)
        });
    if na == 0.0 || nb == 0.0 {
        return Some(0.0);
    }
    Some(dot / (na.sqrt() * nb.sqrt()))
}

/// 全量重建结果（rebuild 端点三态：完成 / 在建冲突 / 失败）
#[derive(Debug)]
pub enum RebuildOutcome {
    /// 重建完成，返回最新状态快照
    Completed(IndexStatusSnapshot),
    /// 已有重建在进行（并发护栏命中）
    AlreadyBuilding,
    /// 重建失败（错误留痕到 status.last_error）
    Failed(IndexError),
}

/// 索引服务编排层：持库 + 图谱快照 + 后台增量（watcher）
///
/// 生命周期与 serve 进程同寿；`Arc<IndexService>` 注入 [`AgentApiState`]。
pub struct IndexService {
    workdir: PathBuf,
    store: IndexStore,
    /// 图谱不可变快照（重建即整体替换；读侧克隆 `Arc` 无锁滞留）
    graph: RwLock<Arc<SymbolGraph>>,
    /// 语义面向量快照（刷新派生时整体重载；读侧克隆 `Arc` 无锁滞留——graph 同款纪律）
    vectors: RwLock<Arc<Vec<ChunkVectorRow>>>,
    /// 语义面嵌入器（None = 语义面关闭：chunk 照常落库，向量留空）
    embedder: Option<Arc<dyn TextEmbedder>>,
    /// 全量重建进行中（并发护栏）
    building: AtomicBool,
    /// 在途增量批计数（防事件风暴下线程堆积）
    in_flight: AtomicU64,
    /// 慢变状态（最近构建时刻 / 最近错误）
    slow: Mutex<SlowState>,
}

#[derive(Default)]
struct SlowState {
    last_build_epoch: Option<u64>,
    last_error: Option<String>,
}

/// 手动 `Debug`（字段含锁与图谱快照，不逐字段展开；状态经 status 端点读）
impl std::fmt::Debug for IndexService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexService")
            .field("workdir", &self.workdir)
            .finish_non_exhaustive()
    }
}

impl IndexService {
    /// 打开（或创建）工作区索引服务
    pub fn open(workdir: &Path) -> Result<Self, IndexError> {
        let db = index_db_path(workdir)?;
        let store = IndexStore::open(&db)?;
        Ok(Self {
            workdir: workdir.to_path_buf(),
            store,
            graph: RwLock::new(Arc::new(SymbolGraph::build(&[], &[], &HashMap::new()))),
            vectors: RwLock::new(Arc::new(Vec::new())),
            embedder: None,
            building: AtomicBool::new(false),
            in_flight: AtomicU64::new(0),
            slow: Mutex::new(SlowState::default()),
        })
    }

    /// 注入语义面嵌入器（开启语义面；随 serve 启动按设置键装配）
    pub fn with_embedder(mut self, embedder: Arc<dyn TextEmbedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// 语义面嵌入器（None = 语义面关闭）
    pub fn embedder(&self) -> Option<&Arc<dyn TextEmbedder>> {
        self.embedder.as_ref()
    }

    /// 持久层引用（对账/计数复用）
    pub fn store(&self) -> &IndexStore {
        &self.store
    }

    /// 工作区根
    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// 当前图谱快照（检索排序用；空图谱 = 未就绪）
    pub fn graph(&self) -> Arc<SymbolGraph> {
        self.graph.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// 当前向量快照（语义检索暴力扫描输入；空 = 语义面未灌量）
    pub fn vectors(&self) -> Arc<Vec<ChunkVectorRow>> {
        self.vectors
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// 状态快照（status 端点）
    pub fn status(&self) -> IndexStatusSnapshot {
        let (files, symbols, refs) = self.store.counts().unwrap_or((0, 0, 0));
        let slow = self.slow.lock().unwrap_or_else(|p| p.into_inner());
        IndexStatusSnapshot {
            files,
            symbols,
            refs,
            chunks: self.store.chunk_count().unwrap_or(0),
            vectors: self.store.vector_count().unwrap_or(0),
            building: self.building.load(Ordering::SeqCst),
            ready: !self.graph().is_empty(),
            last_build_epoch: slow.last_build_epoch,
            last_error: slow.last_error.clone(),
            semantic_enabled: self.embedder.is_some(),
            semantic_model: self.embedder().map(|e| e.model_name()),
        }
    }

    /// 单文件入库：读源码 → 抽取 → chunk 切分（语义面嵌入）→ 整文件替换
    ///
    /// 返回 `Ok(false)` = 不入库（语言不路由 / 文件已消失）；
    /// `Ok(true)` = 已入库（调用方须随后 [`Self::refresh_derived`]）。
    fn ingest_file(&self, entry: &FileEntry) -> Result<bool, IndexError> {
        let Some(lang) = symbols::lang_for_path(&entry.rel_path) else {
            return Ok(false);
        };
        let Ok(bytes) = std::fs::read(&entry.abs_path) else {
            return Ok(false);
        };
        let src = String::from_utf8_lossy(&bytes);
        let extracted = symbols::extract(lang, &src);
        let rows: Vec<SymbolRow> = extracted.symbols.iter().map(|s| s.row.clone()).collect();
        // chunk 面（D8：文本不落库——落边界元数据 + 完整富化文本哈希）
        let built = symbols::build_chunks(
            &entry.rel_path,
            &src,
            &extracted.symbols,
            &extracted.imports,
        );
        let mut chunk_rows: Vec<ChunkRow> = Vec::with_capacity(built.len());
        let mut text_by_hash: HashMap<String, String> = HashMap::new();
        for (i, c) in built.iter().enumerate() {
            let hash = blake3::hash(c.text.as_bytes()).to_hex().to_string();
            text_by_hash
                .entry(hash.clone())
                .or_insert_with(|| c.text.clone());
            chunk_rows.push(ChunkRow {
                ord: i as u32,
                symbol: c.symbol.clone(),
                kind: c.kind.clone(),
                start_line: c.start_line,
                end_line: c.end_line,
                chunk_hash: hash,
            });
        }
        // 向量面（D6 缓存）：旧向量哈希相同者复用，仅缺量新嵌入；语义面关闭
        // （无嵌入器）时向量留空、vectorized=0，补嵌入由对账判据驱动
        let old_vectors: HashMap<String, Vec<f32>> = self
            .store
            .file_vectors(&entry.rel_path)?
            .into_iter()
            .collect();
        let mut vectors: Vec<(String, Vec<f32>)> = Vec::new();
        let mut vectorized = false;
        if let Some(embedder) = &self.embedder {
            // 维度守卫:复用只认维度与当前模型一致的旧向量——换模型后残维度
            // 缓存一律视为缺量重嵌（否则该 chunk 永久不可见于语义检索）。
            // dim 未可知（本进程首嵌前）时保守全量重嵌，首嵌后自愈。
            let dim = embedder.dim();
            let mut reused: Vec<(String, Vec<f32>)> = Vec::new();
            let mut missing: Vec<String> = Vec::new();
            for c in &chunk_rows {
                let reusable = old_vectors
                    .get(&c.chunk_hash)
                    .map(|v| dim == Some(v.len()))
                    .unwrap_or(false);
                if reusable {
                    let v = old_vectors.get(&c.chunk_hash).unwrap();
                    if !reused.iter().any(|(h, _)| h == &c.chunk_hash) {
                        reused.push((c.chunk_hash.clone(), v.clone()));
                    }
                } else if !missing.contains(&c.chunk_hash) {
                    missing.push(c.chunk_hash.clone());
                }
            }
            if !missing.is_empty() {
                let texts: Vec<String> = missing.iter().map(|h| text_by_hash[h].clone()).collect();
                let embeddings = embedder.embed(&texts)?;
                if embeddings.len() != missing.len() {
                    return Err(IndexError(format!(
                        "embedder returned {} vectors for {} chunks",
                        embeddings.len(),
                        missing.len()
                    )));
                }
                vectors = reused;
                vectors.extend(missing.into_iter().zip(embeddings));
            } else {
                vectors = reused;
            }
            vectorized = true;
        }
        self.store.replace_file(
            &entry.rel_path,
            &entry.content_hash,
            entry.mtime,
            entry.size as i64,
            &rows,
            &extracted.ident_counts,
            &chunk_rows,
            &vectors,
            vectorized,
        )?;
        Ok(true)
    }

    /// 重刷派生面（refs 物化 + 图谱快照 + 向量快照 + 慢变状态）
    fn refresh_derived(&self) -> Result<(), IndexError> {
        self.store.refresh_refs()?;
        let syms = self.store.all_symbols_min()?;
        let refs = self.store.all_refs()?;
        let graph = SymbolGraph::build(&syms, &refs, &HashMap::new());
        *self.graph.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(graph);
        let vecs = self.store.all_vectors()?;
        *self.vectors.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(vecs);
        let mut slow = self.slow.lock().unwrap_or_else(|p| p.into_inner());
        slow.last_build_epoch = Some(now_epoch());
        slow.last_error = None;
        Ok(())
    }

    /// 全量重建：clear → 磁盘对账式重灌 → 重刷派生（rebuild 端点 / 首次构建）
    ///
    /// 原子护栏：已在建时返回 [`RebuildOutcome::AlreadyBuilding`]（防双重建
    /// 互踩——clear 与他批 ingest 交错会损坏库内容）。
    pub fn rebuild_sync(&self) -> RebuildOutcome {
        if self
            .building
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return RebuildOutcome::AlreadyBuilding;
        }
        let result = self.reconcile_after_clear();
        self.building.store(false, Ordering::SeqCst);
        match result {
            Ok(()) => RebuildOutcome::Completed(self.status()),
            Err(e) => {
                let mut slow = self.slow.lock().unwrap_or_else(|p| p.into_inner());
                slow.last_error = Some(e.0.clone());
                RebuildOutcome::Failed(e)
            }
        }
    }

    /// 清库后全量重灌（rebuild 内核；对账逻辑与增量对账共用 [`Self::reconcile_sync`])
    fn reconcile_after_clear(&self) -> Result<(), IndexError> {
        self.store.clear()?;
        self.reconcile_sync().map(|_| ())
    }

    /// 磁盘对账：磁盘实况 vs 库内清单，差集决定增/改/删 → 重刷派生
    ///
    /// 返回 `(added, updated, removed)`。启动时与 rebuild 内核调用；
    /// 单文件 ingest 失败 fail-soft（warn 留痕跳过，不毁整批）。
    pub fn reconcile_sync(&self) -> Result<(usize, usize, usize), IndexError> {
        let disk = collect_workspace_files(&self.workdir)?;
        let db_hashes: HashMap<String, FileHash> = self
            .store
            .file_hashes()?
            .into_iter()
            .map(|h| (h.path.clone(), h))
            .collect();
        let mut added = 0usize;
        let mut updated = 0usize;
        let mut dirty = false;
        for entry in &disk {
            let stale = match db_hashes.get(&entry.rel_path) {
                None => {
                    added += 1;
                    true
                }
                Some(h) if h.content_hash != entry.content_hash => {
                    updated += 1;
                    true
                }
                // 内容未变但语义面开启且向量未齐 → 补嵌入重灌（开关后自愈路径）
                Some(h) if self.embedder.is_some() && !h.vectorized => {
                    updated += 1;
                    true
                }
                _ => false,
            };
            if stale {
                match self.ingest_file(entry) {
                    Ok(_) => dirty = true,
                    Err(e) => {
                        warn!(file = %entry.rel_path, error = %e, "index: ingest failed, skipped")
                    }
                }
            }
        }
        let disk_set: HashSet<&str> = disk.iter().map(|e| e.rel_path.as_str()).collect();
        let mut removed = 0usize;
        for path in db_hashes.keys() {
            if !disk_set.contains(path.as_str()) {
                self.store.remove_file(path)?;
                removed += 1;
                dirty = true;
            }
        }
        if dirty {
            self.refresh_derived()?;
        }
        Ok((added, updated, removed))
    }

    /// 增量更新（watcher 批次）：批次路径轻过滤 → 重索引/移除 → 重刷派生
    ///
    /// 返回实际入库文件数。与库内容哈希比对，未变化的文件跳过解析
    /// （watcher 事件含非内容性触碰）；批次内失败 fail-soft 逐文件告警。
    pub fn update_files_sync(&self, rel_paths: &[String]) -> Result<usize, IndexError> {
        if rel_paths.is_empty() {
            return Ok(0);
        }
        let db_hashes: HashMap<String, FileHash> = self
            .store
            .file_hashes()?
            .into_iter()
            .map(|h| (h.path.clone(), h))
            .collect();
        let mut n = 0usize;
        let mut dirty = false;
        for rel in rel_paths {
            let rel = rel.replace('\\', "/");
            if !is_incremental_candidate(&rel) {
                continue;
            }
            let abs = self.workdir.join(&rel);
            let Ok(bytes) = std::fs::read(&abs) else {
                // 文件消失（删除/改名离开）：从索引移除
                if db_hashes.contains_key(&rel) {
                    self.store.remove_file(&rel)?;
                    dirty = true;
                }
                continue;
            };
            if bytes.len() as u64 > MAX_FILE_BYTES {
                continue;
            }
            let content_hash = blake3::hash(&bytes).to_hex().to_string();
            if db_hashes.get(&rel).is_some_and(|h| {
                h.content_hash == content_hash && (self.embedder.is_none() || h.vectorized)
            }) {
                continue; // 内容未变且向量已齐（语义面关闭时不判向量）——非内容性触碰零成本
            }
            let meta = std::fs::metadata(&abs).ok();
            let entry = FileEntry {
                rel_path: rel,
                abs_path: abs,
                size: bytes.len() as u64,
                mtime: meta
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0),
                content_hash,
            };
            match self.ingest_file(&entry) {
                Ok(true) => {
                    n += 1;
                    dirty = true;
                }
                Ok(false) => {}
                Err(e) => {
                    warn!(file = %entry.rel_path, error = %e, "index: incremental ingest failed")
                }
            }
        }
        if dirty {
            self.refresh_derived()?;
        }
        Ok(n)
    }

    /// 启动文件监听（[`INDEX_DEBOUNCE_MS`] 去抖）→ 增量更新；serve 启动时调用一次
    ///
    /// debouncer 显式 [`notify_debouncer_full::NoCache`]（fs_watch 同款纪律：
    /// `RecommendedCache` 建根递归预扫在巨型子树下分钟级不返回）；debouncer 以
    /// `std::mem::forget` 主动泄漏保活——serve 进程生命周期即 watcher 生命周期。
    /// 失败 fail-soft 返回 `Err`（调用方告警降级：索引仍有启动对账 + 手动
    /// rebuild 兜底）。增量批超出 [`MAX_IN_FLIGHT`] 时整批丢弃，由全量对账兜底。
    pub fn spawn_watcher(self: &Arc<Self>) -> Result<(), String> {
        use notify::RecursiveMode;
        use notify_debouncer_full::{new_debouncer_opt, DebounceEventResult, NoCache};
        let wd = std::fs::canonicalize(&self.workdir)
            .map_err(|e| format!("index watcher: resolve workdir: {e}"))?;
        // notify Windows 后端以 canonicalize 形态回投事件路径（fs_watch 同款前置解析）
        let wd_for_events = wd.clone();
        let svc = Arc::clone(self);
        let mut debouncer = new_debouncer_opt::<_, notify::RecommendedWatcher, _>(
            std::time::Duration::from_millis(INDEX_DEBOUNCE_MS),
            None, // tick_rate 缺省：取 timeout/4 轮询
            move |result: DebounceEventResult| {
                let events = match result {
                    Ok(events) => events,
                    Err(errors) => {
                        warn!(
                            count = errors.len(),
                            "index watcher: watcher errors (batch skipped)"
                        );
                        return;
                    }
                };
                let mut dirty: Vec<String> = Vec::new();
                for ev in &events {
                    for p in &ev.paths {
                        if let Ok(rel) = p.strip_prefix(&wd_for_events) {
                            let rel = rel.to_string_lossy().replace('\\', "/");
                            if !dirty.contains(&rel) {
                                dirty.push(rel);
                            }
                        }
                    }
                }
                if dirty.is_empty() {
                    return;
                }
                // 在途批护栏：超限整批丢弃（store 单写者队列防积压）
                if svc.in_flight.fetch_add(1, Ordering::SeqCst) >= MAX_IN_FLIGHT {
                    svc.in_flight.fetch_sub(1, Ordering::SeqCst);
                    warn!(
                        paths = dirty.len(),
                        "index watcher: batch dropped (in-flight cap)"
                    );
                    return;
                }
                let svc_thread = Arc::clone(&svc);
                // 增量解析为 CPU 密集（tree-sitter）：专用阻塞线程，不占 tokio worker
                let spawned = std::thread::Builder::new()
                    .name("index-incremental".into())
                    .spawn(move || {
                        if let Err(e) = svc_thread.update_files_sync(&dirty) {
                            warn!(error = %e, "index: incremental update failed");
                        }
                        svc_thread.in_flight.fetch_sub(1, Ordering::SeqCst);
                    });
                if spawned.is_err() {
                    svc.in_flight.fetch_sub(1, Ordering::SeqCst);
                    warn!("index watcher: failed to spawn incremental thread");
                }
            },
            NoCache,
            notify::Config::default(),
        )
        .map_err(|e| format!("index watcher: create: {e}"))?;
        debouncer
            .watch(&wd, RecursiveMode::Recursive)
            .map_err(|e| format!("index watcher: watch {}: {e}", wd.display()))?;
        std::mem::forget(debouncer);
        info!("index watcher started (debounce {}ms)", INDEX_DEBOUNCE_MS);
        Ok(())
    }

    /// 语义检索：查询文本嵌入 + 向量快照暴力精确扫描（cosine 降序取 top-limit）
    ///
    /// CPU 密集（嵌入 + 全量点积）：调用方须在 spawn_blocking 执行。语义面未
    /// 装配（无嵌入器）返回 `Err`——端点侧先查 [`Self::embedder`] 分流禁用态
    /// （C-3 口径：200 空结果 + `semantic:"disabled"` 标注）。
    pub fn semantic_search_sync(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SemanticHit>, IndexError> {
        let Some(embedder) = &self.embedder else {
            return Err(IndexError("semantic disabled".into()));
        };
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let texts = vec![query.to_string()];
        let qv = embedder
            .embed(&texts)?
            .into_iter()
            .next()
            .ok_or_else(|| IndexError("embedder returned no vector for query".into()))?;
        let snap = self.vectors();
        let mut hits: Vec<SemanticHit> = snap
            .iter()
            .filter_map(|row| {
                cosine(&qv, &row.vector).map(|score| SemanticHit {
                    file: row.file.clone(),
                    ord: row.ord,
                    symbol: row.symbol.clone(),
                    kind: row.kind.clone(),
                    start_line: row.start_line,
                    end_line: row.end_line,
                    score,
                })
            })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(limit);
        Ok(hits)
    }

    /// 词法侧文件排位（hybrid RRF 输入）：grep_core 命中按文件聚合，
    /// 命中数降序、路径升序，取前 [`HYBRID_LEXICAL_MAX_FILES`] 文件
    ///
    /// 词法侧 fail-soft：grep 失败返回空排位（融合仅剩语义侧，不阻断）。
    fn lexical_file_rank(&self, query: &str) -> Vec<(String, usize)> {
        let params = crate::builtin_tools::grep_files::GrepParams {
            query: query.to_string(),
            is_regex: false,
            case_sensitive: false,
            whole_word: false,
            smart_case: true, // B2 缺省口径（search.smartCase 出厂 true）
            dir: ".".into(),
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
            use_ignore_files: true,
            max_results: HYBRID_LEXICAL_MAX_RESULTS,
        };
        let Ok(value) = crate::builtin_tools::grep_files::grep_core(&self.workdir, &params, None)
        else {
            return Vec::new();
        };
        let mut files: Vec<(String, usize)> = value
            .get("groups")
            .and_then(|g| g.as_array())
            .map(|groups| {
                groups
                    .iter()
                    .filter_map(|g| {
                        let path = g.get("path")?.as_str()?.to_string();
                        let hits = g
                            .get("hits")
                            .and_then(|h| h.as_array())
                            .map_or(0, |a| a.len());
                        Some((path, hits))
                    })
                    .collect()
            })
            .unwrap_or_default();
        files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        files.truncate(HYBRID_LEXICAL_MAX_FILES);
        files
    }

    /// 混合检索：词法侧（grep_core 聚合排位）+ 语义侧（嵌入暴力扫描）→ RRF 融合
    ///
    /// 语义面未装配 → 自动降级纯词法（C-3 口径：`semantic_active=false`，
    /// 词法排位直接成形，不报错不阻断）。CPU 密集：调用方 spawn_blocking。
    pub fn hybrid_search_sync(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<HybridResult, IndexError> {
        let lexical = if query.trim().is_empty() {
            Vec::new()
        } else {
            self.lexical_file_rank(query)
        };
        if self.embedder.is_none() {
            return Ok(HybridResult {
                semantic_active: false,
                results: rrf_fuse(&lexical, &[], limit),
            });
        }
        let semantic = self.semantic_search_sync(query, limit)?;
        Ok(HybridResult {
            semantic_active: true,
            results: rrf_fuse(&lexical, &semantic, limit),
        })
    }
}

/// 增量批次路径轻过滤（口径与残差声明见模块文档）
///
/// - 任一路径段以 `.` 开头 → 排除（hidden 口径：`.git`/`.env`/点目录）；
/// - 任一路径段命中 [`DEFAULT_EXCLUDES`] 目录名 → 排除（`target/**` 取目录段比对）；
/// - 扩展名命中 [`SKIP_EXTS`] → 排除（二进制/密钥/锁文件）；
/// - 语言路由不识别（非 rs/ts/tsx/mts/cts）→ 排除。
pub fn is_incremental_candidate(rel_path: &str) -> bool {
    let p = rel_path.replace('\\', "/");
    for comp in p.split('/') {
        if comp.starts_with('.') {
            return false;
        }
        if DEFAULT_EXCLUDES
            .iter()
            .any(|d| d.trim_end_matches("/**") == comp)
        {
            return false;
        }
    }
    let ext = p.rsplit('.').next().unwrap_or("");
    if SKIP_EXTS.contains(&ext) {
        return false;
    }
    symbols::lang_for_path(&p).is_some()
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn index_db_path_is_scoped_by_workspace_hash() {
        let td = tempfile::tempdir().unwrap();
        let a = index_db_path(td.path()).unwrap();
        assert!(a.ends_with("index.db"));
        let dir_name = a.parent().unwrap().file_name().unwrap().to_str().unwrap();
        assert_eq!(dir_name.len(), 16);
        assert!(dir_name.chars().all(|c| c.is_ascii_hexdigit()));
        // 路径含 .evo-agent/index 两级
        assert!(a
            .components()
            .any(|c| c.as_os_str().to_str() == Some(".evo-agent")));
        // 同一工作区稳定；不同工作区不同库
        let a2 = index_db_path(td.path()).unwrap();
        assert_eq!(a, a2);
        let td2 = tempfile::tempdir().unwrap();
        let b = index_db_path(td2.path()).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn incremental_candidate_filters_hidden_and_excluded() {
        assert!(is_incremental_candidate("src/main.rs"));
        assert!(is_incremental_candidate("web/src/lib/util.ts"));
        assert!(is_incremental_candidate("app.tsx"));
        // hidden 口径（任意段）
        assert!(!is_incremental_candidate(".env"));
        assert!(!is_incremental_candidate("sub/.hidden.rs"));
        assert!(!is_incremental_candidate(".git/config.rs"));
        // 默认排除目录
        assert!(!is_incremental_candidate("target/debug/out.rs"));
        assert!(!is_incremental_candidate("node_modules/pkg/x.ts"));
        // 扩展黑名单
        assert!(!is_incremental_candidate("Cargo.lock"));
        // 语言路由不识别
        assert!(!is_incremental_candidate("README.md"));
        assert!(!is_incremental_candidate("script.py"));
    }

    #[test]
    fn rebuild_reconcile_and_search_roundtrip() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        std::fs::write(root.join("a.rs"), "fn alpha() { beta(); }").unwrap();
        std::fs::write(root.join("b.rs"), "fn beta() {}").unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("target/x.rs"), "fn noise() {}").unwrap();

        let svc = IndexService::open(root).unwrap();
        let status = match svc.rebuild_sync() {
            RebuildOutcome::Completed(s) => s,
            other => panic!("expected completed, got {other:?}"),
        };
        assert_eq!(status.files, 2);
        assert!(status.ready);
        assert!(status.last_error.is_none());

        // 检索：alpha 命中 a.rs；target 噪声不入库
        let graph = svc.graph();
        let hits = graph.match_symbols("alpha", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0.file, "a.rs");
        assert!(graph.match_symbols("noise", 10).is_empty());
        // 引用边：a.rs 引用 beta（b.rs 定义）→ 1 条边
        assert_eq!(status.refs, 1);
    }

    #[test]
    fn reconcile_detects_add_update_remove() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        std::fs::write(root.join("a.rs"), "fn one() {}").unwrap();
        let svc = IndexService::open(root).unwrap();
        match svc.rebuild_sync() {
            RebuildOutcome::Completed(_) => {}
            other => panic!("expected completed, got {other:?}"),
        }
        // 新增 + 修改
        std::fs::write(root.join("b.rs"), "fn two() {}").unwrap();
        std::fs::write(root.join("a.rs"), "fn one_changed() {}").unwrap();
        let (added, updated, removed) = svc.reconcile_sync().unwrap();
        assert_eq!((added, updated, removed), (1, 1, 0));
        assert_eq!(svc.status().files, 2);
        // 删除
        std::fs::remove_file(root.join("b.rs")).unwrap();
        let (added, updated, removed) = svc.reconcile_sync().unwrap();
        assert_eq!((added, updated, removed), (0, 0, 1));
        assert_eq!(svc.status().files, 1);
    }

    #[test]
    fn update_files_sync_ingests_and_removes() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        std::fs::write(root.join("a.rs"), "fn alpha() {}").unwrap();
        let svc = IndexService::open(root).unwrap();
        match svc.rebuild_sync() {
            RebuildOutcome::Completed(_) => {}
            other => panic!("expected completed, got {other:?}"),
        }
        // 新文件入库
        std::fs::write(root.join("c.rs"), "fn gamma() {}").unwrap();
        let n = svc
            .update_files_sync(&["c.rs".to_string(), "nope.rs".to_string()])
            .unwrap();
        assert_eq!(n, 1);
        assert!(svc.graph().match_symbols("gamma", 10).len() == 1);
        // 文件删除 → 移除出索引
        std::fs::remove_file(root.join("c.rs")).unwrap();
        let n = svc.update_files_sync(&["c.rs".to_string()]).unwrap();
        assert_eq!(n, 0);
        assert!(svc.graph().match_symbols("gamma", 10).is_empty());
        // 排除口径路径不入
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("target/h.rs"), "fn hidden() {}").unwrap();
        let n = svc.update_files_sync(&["target/h.rs".to_string()]).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn rebuild_twice_is_guarded_but_sequential_ok() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        std::fs::write(root.join("a.rs"), "fn a() {}").unwrap();
        let svc = IndexService::open(root).unwrap();
        // 串行两次重建均成功
        match svc.rebuild_sync() {
            RebuildOutcome::Completed(s) => assert_eq!(s.files, 1),
            other => panic!("expected completed, got {other:?}"),
        }
        match svc.rebuild_sync() {
            RebuildOutcome::Completed(s) => assert_eq!(s.files, 1),
            other => panic!("expected completed, got {other:?}"),
        }
    }

    /// 假嵌入器（确定性向量=文本哈希派生；计数调用与嵌入文本量，供复用断言）
    struct FakeEmbedder {
        dim: usize,
        calls: AtomicUsize,
        embedded: AtomicUsize,
    }

    impl FakeEmbedder {
        fn new(dim: usize) -> Self {
            Self {
                dim,
                calls: AtomicUsize::new(0),
                embedded: AtomicUsize::new(0),
            }
        }
        fn embedded(&self) -> usize {
            self.embedded.load(Ordering::SeqCst)
        }
    }

    impl TextEmbedder for FakeEmbedder {
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, IndexError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.embedded.fetch_add(texts.len(), Ordering::SeqCst);
            Ok(texts
                .iter()
                .map(|t| {
                    let h = blake3::hash(t.as_bytes());
                    (0..self.dim)
                        .map(|i| h.as_bytes()[i % h.as_bytes().len()] as i8 as f32 / 128.0)
                        .collect()
                })
                .collect())
        }
        fn model_name(&self) -> String {
            "fake-test-model".into()
        }
        fn dim(&self) -> Option<usize> {
            Some(self.dim)
        }
    }

    /// 关键词嵌入器（检索测试）：向量 = 关键词命中 0/1 维 + 常亮偏置维，
    /// 相似度可控（同关键词 1.0 / 异关键词 0.5 / 无关 0.0）
    struct KeywordEmbedder;

    impl TextEmbedder for KeywordEmbedder {
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, IndexError> {
            Ok(texts
                .iter()
                .map(|t| {
                    vec![
                        if t.contains("alpha") { 1.0 } else { 0.0 },
                        if t.contains("beta") { 1.0 } else { 0.0 },
                        if t.contains("gamma") { 1.0 } else { 0.0 },
                        1.0,
                    ]
                })
                .collect())
        }
        fn model_name(&self) -> String {
            "keyword-test-model".into()
        }
        fn dim(&self) -> Option<usize> {
            Some(4)
        }
    }

    #[test]
    fn semantic_search_ranks_by_cosine_against_snapshot() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        std::fs::write(root.join("a.rs"), "fn alpha_lookup() { }").unwrap();
        std::fs::write(root.join("b.rs"), "fn beta_thing() { }").unwrap();
        let svc = IndexService::open(root)
            .unwrap()
            .with_embedder(Arc::new(KeywordEmbedder));
        match svc.rebuild_sync() {
            RebuildOutcome::Completed(s) => assert!(s.vectors > 0),
            other => panic!("expected completed, got {other:?}"),
        }
        // 空白查询 → 空结果（不嵌入不扫描）
        assert!(svc.semantic_search_sync("  ", 10).unwrap().is_empty());
        let hits = svc.semantic_search_sync("alpha", 10).unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].symbol.contains("alpha"));
        assert!(hits[0].score > 0.99); // 同关键词命中 → cosine≈1
                                       // 异文件 chunk（beta）相似度显著更低（公共偏置维 → 0.5）
        if hits.len() > 1 {
            assert!(hits[1].score < hits[0].score);
        }
    }

    #[test]
    fn semantic_search_without_embedder_errors() {
        let td = tempfile::tempdir().unwrap();
        let svc = IndexService::open(td.path()).unwrap();
        assert!(svc.semantic_search_sync("x", 5).is_err());
    }

    #[test]
    fn hybrid_degrades_to_lexical_without_embedder() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        std::fs::write(root.join("a.rs"), "fn alpha_lookup() { }").unwrap();
        let svc = IndexService::open(root).unwrap();
        match svc.rebuild_sync() {
            RebuildOutcome::Completed(_) => {}
            other => panic!("expected completed, got {other:?}"),
        }
        // C-3 降级口径：无嵌入器 → 纯词法成形，语义位 None，不报错
        let r = svc.hybrid_search_sync("alpha", 10).unwrap();
        assert!(!r.semantic_active);
        assert!(r
            .results
            .iter()
            .any(|h| h.file == "a.rs" && h.lexical_hits >= 1));
        assert!(r.results.iter().all(|h| h.semantic_best.is_none()));
    }

    #[test]
    fn hybrid_fuses_lexical_and_semantic_with_embedder() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        std::fs::write(root.join("a.rs"), "fn alpha_lookup() { }").unwrap();
        std::fs::write(root.join("b.rs"), "fn beta_thing() { }").unwrap();
        let svc = IndexService::open(root)
            .unwrap()
            .with_embedder(Arc::new(KeywordEmbedder));
        match svc.rebuild_sync() {
            RebuildOutcome::Completed(_) => {}
            other => panic!("expected completed, got {other:?}"),
        }
        let r = svc.hybrid_search_sync("alpha", 10).unwrap();
        assert!(r.semantic_active);
        // a.rs 双侧命中（词法 grep + 语义 cosine 1.0）→ RRF 必登顶
        assert_eq!(r.results[0].file, "a.rs");
        assert!(r.results[0].lexical_hits >= 1);
        assert!(r.results[0].semantic_best.is_some());
    }

    #[test]
    fn semantic_ingest_embeds_and_reuses_unchanged_chunks() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        std::fs::write(root.join("a.rs"), "fn alpha() { beta(); }").unwrap();
        let emb = Arc::new(FakeEmbedder::new(8));
        let svc = IndexService::open(root).unwrap().with_embedder(emb.clone());
        match svc.rebuild_sync() {
            RebuildOutcome::Completed(s) => {
                assert!(s.chunks > 0);
                assert_eq!(s.vectors, s.chunks);
            }
            other => panic!("expected completed, got {other:?}"),
        }
        let first = emb.embedded();
        assert!(first > 0);
        // 追加新函数：alpha 的 chunk 文本未变 → 仅新增 chunk 重嵌入（D6 复用）
        std::fs::write(
            root.join("a.rs"),
            "fn alpha() { beta(); }\nfn beta() {}\nfn gamma() {}\n",
        )
        .unwrap();
        let n = svc.update_files_sync(&["a.rs".to_string()]).unwrap();
        assert_eq!(n, 1);
        assert_eq!(svc.status().vectors, svc.status().chunks);
        let second_call_texts = emb.embedded() - first;
        assert!(
            second_call_texts < 3,
            "expected chunk reuse, embedded {second_call_texts}"
        );
        // 快照读出：元数据齐 + 维度一致
        let all = svc.store().all_vectors().unwrap();
        assert!(all.len() >= 3);
        assert!(all.iter().all(|c| c.vector.len() == 8));
    }

    #[test]
    fn dim_mismatch_vectors_are_reembedded_not_reused() {
        // 维度守卫：换模型场景——库中 8 维旧向量 + 4 维新嵌入器,
        // 同 hash 缓存不得复用（复用即该 chunk 永久不可见于语义检索）,
        // 守卫判维度不符 → 全量重嵌自愈,库内维度归一
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        std::fs::write(root.join("a.rs"), "fn alpha() { beta(); }").unwrap();
        let old_emb = Arc::new(FakeEmbedder::new(8));
        let svc = IndexService::open(root).unwrap().with_embedder(old_emb);
        match svc.rebuild_sync() {
            RebuildOutcome::Completed(s) => assert!(s.vectors > 0),
            other => panic!("expected completed, got {other:?}"),
        }
        let new_emb = Arc::new(FakeEmbedder::new(4));
        let svc2 = IndexService::open(root)
            .unwrap()
            .with_embedder(new_emb.clone());
        // 追加换行:content_hash 变化触发重摄入,chunk 文本不变 → chunk_hash
        // 不变 → 命中旧 8 维缓存,守卫须判维度不符重嵌
        std::fs::write(root.join("a.rs"), "fn alpha() { beta(); }\n").unwrap();
        svc2.update_files_sync(&["a.rs".to_string()]).unwrap();
        assert!(
            new_emb.embedded() > 0,
            "dim-mismatched cache must be re-embedded, not reused"
        );
        let all = svc2.store().all_vectors().unwrap();
        assert!(!all.is_empty());
        assert!(all.iter().all(|c| c.vector.len() == 4));
    }

    #[test]
    fn semantic_disabled_keeps_vectors_empty_but_chunks_stored() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        std::fs::write(root.join("a.rs"), "fn alpha() {}").unwrap();
        let svc = IndexService::open(root).unwrap();
        match svc.rebuild_sync() {
            RebuildOutcome::Completed(s) => {
                assert!(s.chunks > 0);
                assert_eq!(s.vectors, 0);
            }
            other => panic!("expected completed, got {other:?}"),
        }
        assert!(svc.embedder().is_none());
    }

    #[test]
    fn backfill_reingests_unvectorized_files_when_embedder_present() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        std::fs::write(root.join("a.rs"), "fn alpha() {}").unwrap();
        // 关闭态先建库：chunk 在档、向量空
        {
            let svc = IndexService::open(root).unwrap();
            match svc.rebuild_sync() {
                RebuildOutcome::Completed(s) => assert_eq!(s.vectors, 0),
                other => panic!("expected completed, got {other:?}"),
            }
        }
        // 开启态重开同一库 → 对账补嵌入
        let emb = Arc::new(FakeEmbedder::new(4));
        let svc = IndexService::open(root).unwrap().with_embedder(emb.clone());
        let (added, updated, removed) = svc.reconcile_sync().unwrap();
        assert_eq!((added, updated, removed), (0, 1, 0));
        assert!(emb.embedded() > 0);
        assert_eq!(svc.status().vectors, svc.status().chunks);
        // 再对账：向量已齐 → 零重灌
        let before = emb.embedded();
        let (added, updated, removed) = svc.reconcile_sync().unwrap();
        assert_eq!((added, updated, removed), (0, 0, 0));
        assert_eq!(emb.embedded(), before);
    }
}
