// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 共享记忆前缀检索的有序键索引（纯性能层）。
//!
//! 定位与边界：
//! - 索引记录 = 每个命名空间一条 payload 记录，持有该命名空间共享分区
//!   当前可见键的有序（字节序升序、去重）列表 + 建册时的共享账本版本号。
//! - 前缀查询先读索引：O(log n) 区间选择（字节序二分）后仅对区间内键做
//!   定向读取；索引缺失/损坏/版本漂移/一致性失效时回退既有全量扫描路径
//!   并从扫描结果重建索引。fail-soft 铁律：索引任何故障都不得改变既有
//!   正确结果，性能层允许失效。
//! - 索引即事实随账持久化：经会话 payload 端点写入（落 FactsLog 审计链）。
//!   存储落点取 `__memory__.{ns}.__prefix_index__.shared`——命名空间层段的
//!   第三段固定为 `__prefix_index__`，而会话键一律嵌在 `session_{sid}.` 之
//!   下、消息键在 `.messages.` 之下、共享键在 `shared.` 根之下，故任何用
//!   户记忆键在结构上都不可达该路径（零碰撞）；且该路径不经共享账本，
//!   索引写不会 bump 共享版本号，版本守卫因此可用。
//! - 覆盖面：仅共享分区前缀扫描。会话分区前缀扫描保持既有全量路径——
//!   会话 payload 无版本号原语，且存在管理器外写者（事件库等），正索引
//!   无法保证不漏新键；共享分区存在账本版本号原语，任何旁路写者（工具
//!   面/API 面）都会 bump 版本触发重建，不漏键可被机器保证。

/// 索引记录的 kind 标识（sanity 校验用；外来数据占据索引路径时判定损坏）
pub(crate) const PREFIX_INDEX_KIND: &str = "memory_prefix_index";

/// 索引记录 scope 标识（本模块只覆盖共享分区）
pub(crate) const PREFIX_INDEX_SCOPE_SHARED: &str = "shared";

/// 定向读取数上限：区间超过此长度时回退既有前缀扫描（避免大区间退化为
/// 逐键多次往返，性能层在不适用的形态下主动让位）。
pub(crate) const PREFIX_INDEX_MAX_TARGETED_READS: usize = 64;

/// 索引记录路径（命名空间层段，结构上与任何用户记忆键无碰撞）。
pub(crate) fn shared_index_path(namespace: &str) -> String {
    format!("__memory__.{namespace}.__prefix_index__.shared")
}

/// 有序键索引记录载荷（serde 形态即存储形态）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct PrefixIndexPayload {
    /// 种类标识（恒 [`PREFIX_INDEX_KIND`]；他值=损坏）
    pub kind: String,
    /// 分区标识（恒 [`PREFIX_INDEX_SCOPE_SHARED`]）
    pub scope: String,
    /// 载荷形态版本
    pub v: u32,
    /// 有序键列表（字节序升序、去重）
    pub keys: Vec<String>,
    /// 建册时的共享账本版本号（新鲜度守卫；缺失=无法守卫=损坏）
    #[serde(default)]
    pub shared_version: Option<u64>,
}

impl PrefixIndexPayload {
    /// 构建共享分区索引载荷（防御性排序去重，保证写出即合规）
    pub(crate) fn new_shared(mut keys: Vec<String>, shared_version: u64) -> Self {
        keys.sort();
        keys.dedup();
        Self {
            kind: PREFIX_INDEX_KIND.to_string(),
            scope: PREFIX_INDEX_SCOPE_SHARED.to_string(),
            v: 1,
            keys,
            shared_version: Some(shared_version),
        }
    }

    /// 内部一致性校验（种类/分区/形态版本/键有序且去重/版本号在场）
    pub(crate) fn is_sane(&self) -> bool {
        // 严格升序 ⟺ 有序且去重（String Ord = 字节序，与写入面 sort 一致）
        self.kind == PREFIX_INDEX_KIND
            && self.scope == PREFIX_INDEX_SCOPE_SHARED
            && self.v == 1
            && self.shared_version.is_some()
            && self.keys.windows(2).all(|w| w[0] < w[1])
    }

    /// 有序插入一键（幂等；保持字节序升序且去重）
    pub(crate) fn insert_key(&mut self, key: &str) {
        let idx = self.keys.partition_point(|k| k.as_str() < key);
        if self
            .keys
            .get(idx)
            .map(|k| k.as_str() == key)
            .unwrap_or(false)
        {
            return; // 已在场：幂等无操作
        }
        self.keys.insert(idx, key.to_string());
    }

    /// 有序移除一键（不存在时幂等无操作）
    pub(crate) fn remove_key(&mut self, key: &str) {
        let idx = self.keys.partition_point(|k| k.as_str() < key);
        if self
            .keys
            .get(idx)
            .map(|k| k.as_str() == key)
            .unwrap_or(false)
        {
            self.keys.remove(idx);
        }
    }
}

/// 在按字节序升序排列的去重键列表上做 starts_with 前缀区间选择。
///
/// 返回半开区间 `[start, end)`；语义与键 `k.starts_with(prefix)` 精确等价
/// （服务端按 path 前缀过滤、path = 固定区前缀 + 键，两者同构）。
/// 空前缀覆盖全表；无匹配返回空区间。纯二分，O(log n)，不依赖字节算术
/// 上界（规避 UTF-8 增量边界问题）。
pub(crate) fn prefix_key_range(keys: &[String], prefix: &str) -> (usize, usize) {
    // 起点：首个 >= prefix 的键（全部键 < prefix 时落到表尾）
    let start = keys.partition_point(|k| k.as_str() < prefix);
    // 终点：起点起首个不再以 prefix 开头的键。纯二分、无字节算术上界，
    // 对任意 UTF-8 前缀（含多字节字符）语义精确等价 starts_with
    let end = start + keys[start..].partition_point(|k| k.starts_with(prefix));
    (start, end)
}

/// 区间长度是否值得走定向读取形态
pub(crate) fn range_worth_indexing(range_len: usize) -> bool {
    // 空区间由查询面直接 Hit(空) 短路，不经此判断；超上限的区间逐键
    // 往返劣化，回退既有全量扫描
    range_len > 0 && range_len <= PREFIX_INDEX_MAX_TARGETED_READS
}

/// 前缀索引快路径的探测结果（fail-soft 语义的三态出口）
pub(crate) enum PrefixIndexProbe {
    /// 索引可用且一致：区间定向读取的权威结果
    Hit(Vec<crate::agent::memory::MemoryRecord>),
    /// 回退既有扫描路径，且允许从扫描结果重建索引（修复）
    MissRebuildable,
    /// 回退既有扫描路径，但不要写索引（server 读失败/版本端点失败——
    /// 避免故障窗口内的写风暴；索引保持原样）
    MissQuiet,
}

/// 索引维护操作（写入面）
pub(crate) enum PrefixIndexOp {
    /// 键已成功持久化 → 入册
    Insert,
    /// 墓碑已成功落账 → 出册
    Remove,
}

/// 索引进程内状态（跨 manager 克隆共享；两枚软标志，fail-soft 的状态机面）
pub(crate) struct PrefixIndexState {
    /// 索引维护写失败 → false（本实例粘性禁用：查询只走既有扫描，零索引写）
    pub(crate) enabled: std::sync::atomic::AtomicBool,
    /// 索引维护被跳过（读失败/册缺席）→ true：索引可能滞后于账面，查询
    /// 回退全量扫描并从扫描结果重建（重建写成功即清除）
    pub(crate) dirty: std::sync::atomic::AtomicBool,
}

impl Default for PrefixIndexState {
    fn default() -> Self {
        Self {
            enabled: std::sync::atomic::AtomicBool::new(true),
            dirty: std::sync::atomic::AtomicBool::new(false),
        }
    }
}
