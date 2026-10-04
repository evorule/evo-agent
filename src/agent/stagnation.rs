//! 进展停滞检测(F2 空转克星)。
//!
//! 观察(工具名, 参数, 结果)三元组摘要的连续重复:同一调用+同一结果
//! 连续出现即空转信号。
//! - 连续 `WARN_AFTER_REPEATS` 次 → [`StagnationVerdict::Warning`]:
//!   调用方在 tool 结果上追加换策略指令(fail-visible,LLM 下一轮可见);
//! - Warning 后仍重复,累计 `EXHAUST_AFTER_WARNINGS` 次警告 →
//!   [`StagnationVerdict::Exhausted`]:按 H2 不可恢复阻塞收尾
//!   (诚实退出优于空转烧预算)。
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
    Warning { repeat_count: u32 },
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
}
