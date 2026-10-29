//! 进展停滞检测(F2 空转克星)。
//!
//! 观察(工具名, 参数, 结果)三元组摘要的连续重复:同一调用+同一结果
//! 连续出现即空转信号。
//! - 连续 `WARN_AFTER_REPEATS` 次 → [`StagnationVerdict::Warning`]:
//!   调用方在 tool 结果上追加换策略指令(fail-visible,LLM 下一轮可见);
//! - Warning 后仍重复,累计 `EXHAUST_AFTER_WARNINGS` 次警告 →
//!   [`StagnationVerdict::Exhausted`]:按 H2 不可恢复阻塞收尾
//!   (诚实退出优于空转烧预算)。
//!
//! 任一出现新 digest → 计数全部重置(有真实进展即不是空转)。

/// 连续重复多少次后发出换策略警告
pub const WARN_AFTER_REPEATS: u32 = 3;
/// 发出警告后仍重复多少次即判定耗尽(H2)
pub const EXHAUST_AFTER_WARNINGS: u32 = 2;

/// 停滞检测结论
pub enum StagnationVerdict {
    /// 有进展或未达阈值
    Normal,
    /// 连续重复达阈值:在 tool 结果上追加换策略指令
    Warning {
        /// 触发本次警告时的连续重复次数
        repeat_count: u32,
    },
    /// 换策略后仍重复:按 H2 不可恢复阻塞收尾
    Exhausted,
}

/// 停滞检测器(每个运行实例一个,跨轮持续观察)
#[derive(Default)]
pub struct StagnationDetector {
    last_digest: Option<u64>,
    repeat_count: u32,
    warning_count: u32,
}

impl StagnationDetector {
    /// 创建检测器（无历史 digest,首轮观察即为有进展）
    pub fn new() -> Self {
        Self::default()
    }

    /// 观察一次工具执行(参数与结果取全文摘要,确定性哈希)
    pub fn observe(&mut self, tool_name: &str, args: &str, result: &str) -> StagnationVerdict {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (tool_name, args, result).hash(&mut h);
        let digest = h.finish();

        if self.last_digest != Some(digest) {
            self.last_digest = Some(digest);
            self.repeat_count = 1;
            self.warning_count = 0;
            return StagnationVerdict::Normal;
        }
        self.repeat_count += 1;
        if self.repeat_count < WARN_AFTER_REPEATS {
            return StagnationVerdict::Normal;
        }
        self.warning_count += 1;
        if self.warning_count >= EXHAUST_AFTER_WARNINGS {
            StagnationVerdict::Exhausted
        } else {
            StagnationVerdict::Warning {
                repeat_count: self.repeat_count,
            }
        }
    }
}

/// Warning 级追加到 tool 结果的换策略指令(fail-visible,LLM 下一轮可见)
pub const STAGNATION_WARNING_MARK: &str =
    "[停滞警告:同一调用+同一结果已连续多次,判据不会因重复而改变——请更换策略(replan)]";

/// Exhausted 级追加到 tool 结果的阻塞收尾指令(H2 诚实退出)
pub const STAGNATION_EXHAUSTED_MARK: &str =
    "[停滞耗尽:换策略后仍连续无进展,按不可恢复阻塞收尾——请提交 task_blocked 并附 reason 分类]";

/// 轮级序列循环检测器（补 F2 停滞检测三盲区：结果抖动/ABA 交替/轮级组合
/// 重复）。指纹 result-free——本轮全部 (工具名, 参数) 的规范序列串，直接
/// 字符串比较（不引入哈希域）；结果不入指纹，错误文本带时间戳等抖动免疫。
/// 滑动窗=2：立即重复与隔轮交替（A,B,A）均可命中；Warning/Exhausted 阈值
/// 与语义复用本模块两级软指引（同款标记注入，终止行为零变化）。
#[derive(Default)]
pub struct SequenceLoopDetector {
    /// 最近 2 轮指纹（滑动窗）
    window: std::collections::VecDeque<String>,
    repeat_count: u32,
    warning_count: u32,
}

impl SequenceLoopDetector {
    /// 创建检测器（空窗,首轮观察即为有进展）
    pub fn new() -> Self {
        Self::default()
    }

    /// 观察一轮工具调用序列（轮内工具执行前调用）
    pub fn observe_round(&mut self, fingerprint: &str) -> StagnationVerdict {
        // 命中滑动窗（立即重复或隔轮交替）= 无进展
        if self.window.iter().any(|f| f == fingerprint) {
            self.repeat_count += 1;
            if self.repeat_count < WARN_AFTER_REPEATS {
                return StagnationVerdict::Normal;
            }
            self.warning_count += 1;
            if self.warning_count >= EXHAUST_AFTER_WARNINGS {
                return StagnationVerdict::Exhausted;
            }
            return StagnationVerdict::Warning {
                repeat_count: self.repeat_count,
            };
        }
        // 新指纹=有进展：计数全重置并推进滑动窗（保留最近 2 轮）
        self.window.push_back(fingerprint.to_string());
        while self.window.len() > 2 {
            self.window.pop_front();
        }
        self.repeat_count = 1;
        self.warning_count = 0;
        StagnationVerdict::Normal
    }
}

/// 轮指纹构造：全部 (工具名, 参数) 的规范序列串。参数 Value 序列化键序由
/// serde_json BTreeMap 规范（preserve_order 关），调用顺序参与指纹。
pub fn round_fingerprint(calls: &[(String, serde_json::Value)]) -> String {
    let items: Vec<serde_json::Value> = calls
        .iter()
        .map(|(name, args)| serde_json::json!({ "tool": name, "args": args }))
        .collect();
    serde_json::to_string(&items).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_result_resets_counters() {
        let mut d = StagnationDetector::new();
        for _ in 0..(WARN_AFTER_REPEATS + 1) {
            d.observe("read", "a", "same");
        }
        assert!(matches!(
            d.observe("read", "a", "same"),
            StagnationVerdict::Exhausted
        ));
        // 出现新结果 → 重置
        assert!(matches!(
            d.observe("read", "a", "changed"),
            StagnationVerdict::Normal
        ));
        assert!(matches!(
            d.observe("read", "a", "changed"),
            StagnationVerdict::Normal
        ));
    }

    #[test]
    fn test_warning_then_exhausted() {
        let mut d = StagnationDetector::new();
        assert!(matches!(
            d.observe("sh", "x", "r1"),
            StagnationVerdict::Normal
        ));
        assert!(matches!(
            d.observe("sh", "x", "r1"),
            StagnationVerdict::Normal
        ));
        match d.observe("sh", "x", "r1") {
            StagnationVerdict::Warning { repeat_count } => {
                assert_eq!(repeat_count, WARN_AFTER_REPEATS)
            }
            _ => panic!("3rd identical should warn"),
        }
        assert!(matches!(
            d.observe("sh", "x", "r1"),
            StagnationVerdict::Exhausted
        ));
    }

    #[test]
    fn test_different_args_are_not_stagnation() {
        let mut d = StagnationDetector::new();
        assert!(matches!(
            d.observe("read", "a", "r"),
            StagnationVerdict::Normal
        ));
        assert!(matches!(
            d.observe("read", "b", "r"),
            StagnationVerdict::Normal
        ));
        assert!(matches!(
            d.observe("read", "c", "r"),
            StagnationVerdict::Normal
        ));
    }

    // ---------- 轮级序列循环检测（补 F2 三盲区：结果抖动/ABA 交替/轮级组合） ----------

    #[test]
    fn test_sequence_immediate_repeat_warns_then_exhausts() {
        let mut d = SequenceLoopDetector::new();
        assert!(matches!(d.observe_round("A"), StagnationVerdict::Normal));
        assert!(matches!(d.observe_round("A"), StagnationVerdict::Normal));
        match d.observe_round("A") {
            StagnationVerdict::Warning { repeat_count } => assert_eq!(repeat_count, 3),
            _ => panic!("3rd identical round should warn"),
        }
        assert!(matches!(d.observe_round("A"), StagnationVerdict::Exhausted));
    }

    #[test]
    fn test_sequence_aba_alternation_detected() {
        // 交替循环 A,B,A,B,A：现有单调用连续重复检测不可见,序列级滑动窗命中
        let mut d = SequenceLoopDetector::new();
        assert!(matches!(d.observe_round("A"), StagnationVerdict::Normal));
        assert!(matches!(d.observe_round("B"), StagnationVerdict::Normal));
        assert!(matches!(d.observe_round("A"), StagnationVerdict::Normal));
        assert!(matches!(
            d.observe_round("B"),
            StagnationVerdict::Warning { .. }
        ));
        assert!(matches!(d.observe_round("A"), StagnationVerdict::Exhausted));
    }

    #[test]
    fn test_sequence_new_fingerprint_resets() {
        let mut d = SequenceLoopDetector::new();
        assert!(matches!(d.observe_round("A"), StagnationVerdict::Normal));
        assert!(matches!(d.observe_round("A"), StagnationVerdict::Normal));
        assert!(matches!(
            d.observe_round("A"),
            StagnationVerdict::Warning { .. }
        ));
        // 新指纹=有进展:计数全重置
        assert!(matches!(d.observe_round("B"), StagnationVerdict::Normal));
        assert!(matches!(d.observe_round("B"), StagnationVerdict::Normal));
        assert!(matches!(
            d.observe_round("B"),
            StagnationVerdict::Warning { .. }
        ));
    }

    #[test]
    fn test_round_fingerprint_canonical_and_result_free() {
        // 同调用同指纹(指纹 result-free,免疫结果抖动)
        let a = vec![("file_read".to_string(), serde_json::json!({"path": "x"}))];
        let b = vec![("file_read".to_string(), serde_json::json!({"path": "x"}))];
        assert_eq!(round_fingerprint(&a), round_fingerprint(&b));
        // 参数不同指纹不同
        let c = vec![("file_read".to_string(), serde_json::json!({"path": "y"}))];
        assert_ne!(round_fingerprint(&a), round_fingerprint(&c));
        // 键序规范化(serde_json BTreeMap 键序,preserve_order 关)
        let d = vec![("file_read".to_string(), serde_json::json!({"a": 1, "b": 2}))];
        let e = vec![("file_read".to_string(), serde_json::json!({"b": 2, "a": 1}))];
        assert_eq!(round_fingerprint(&d), round_fingerprint(&e), "键序规范化");
        // 多调用组合序列有序
        let f = vec![
            ("a".to_string(), serde_json::json!(1)),
            ("b".to_string(), serde_json::json!(2)),
        ];
        let g = vec![
            ("b".to_string(), serde_json::json!(2)),
            ("a".to_string(), serde_json::json!(1)),
        ];
        assert_ne!(
            round_fingerprint(&f),
            round_fingerprint(&g),
            "调用顺序参与指纹"
        );
        // 同参不同结果不影响指纹(结果不入指纹)
        assert_eq!(round_fingerprint(&a), round_fingerprint(&b));
    }
}
