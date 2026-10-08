//! 工具执行重试分类面:装载期静态表把工具名映射到幂等三分类,运行期配合
//! 错误形态判别决定执行阶段内是否自动重试。
//!
//! 设计约束:
//! - 缺省 non-retryable(未知工具/新工具一律不自动重试)——盲重试写操作
//!   是漂移风险源:宁可失败显式回喂,不可错误重放副作用;
//! - 瞬态判别只认连接/超时/限流/5xx 类错误形态(与 LLM 传输层重试口径
//!   同源:429/5xx/网络错误),参数校验/权限/策略类失败一律非瞬态;
//! - 静态表刻意保持小而保守:只登记明确安全的成员,新工具落缺省类。

/// 幂等三分类(装载期静态判)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryClass {
    /// 只读查询:自动重试安全
    IdempotentRead,
    /// 幂等写:全量覆写语义(重放收敛,或显式报"已存在",无重复副作用)
    IdempotentWrite,
    /// 非幂等(执行/发送/删除/移动/创建/提交/派生类):不自动重试,
    /// 失败显式回喂
    NonRetryable,
}

/// 幂等类工具遇瞬态故障的自动重试次数上限(首次执行之外;指数回退,
/// 与 LLM 传输层重试面同参数量级)
pub const MAX_TOOL_RETRIES: u32 = 2;

/// 装载期静态表:工具名 → 幂等类。未登记工具一律 NonRetryable(fail-safe)。
pub fn retry_class(tool: &str) -> RetryClass {
    match tool {
        // 只读查询族(本地文件面/账面/技能面/交接读面/版本面/只读查询面)
        "file_read"
        | "file_list"
        | "grep_files"
        | "search_files"
        | "query_journal"
        | "query_trace"
        | "read_skill"
        | "handover_read"
        | "git_status"
        | "git_log"
        | "git_diff"
        | "diff_runs"
        | "audit_get"
        | "knowledge_search"
        | "knowledge_entry_get"
        | "rule_list"
        | "rule_get"
        | "dataset_list"
        | "sandbox_list"
        | "sandbox_get"
        | "env_state"
        | "ws_list" => RetryClass::IdempotentRead,
        // 幂等写族:全量覆写语义(重放收敛,或显式"已存在"报错,无重复副作用)
        "file_write" | "handover_write" => RetryClass::IdempotentWrite,
        // 其余(执行/发送/删除/移动/创建/提交/派生/服务端状态变更族)缺省不重试
        _ => RetryClass::NonRetryable,
    }
}

/// 错误形态瞬态判别:连接/超时/限流/5xx 类字样 = 瞬态(与 LLM 传输层
/// 可重试口径同源);参数校验/权限/策略类失败一律非瞬态。
pub fn is_transient_error(err: &str) -> bool {
    let e = err.to_lowercase();
    const MARKS: [&str; 11] = [
        "connection",
        "timed out",
        "timeout",
        "unreachable",
        "broken pipe",
        "reset by peer",
        "temporarily unavailable",
        "service unavailable",
        "bad gateway",
        "internal server error",
        "too many requests",
    ];
    MARKS.iter().any(|m| e.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_class_routes_three_families() {
        // 三族路由各验代表成员;未知工具落缺省非幂等(fail-safe)
        assert_eq!(retry_class("grep_files"), RetryClass::IdempotentRead);
        assert_eq!(retry_class("query_journal"), RetryClass::IdempotentRead);
        assert_eq!(retry_class("file_read"), RetryClass::IdempotentRead);
        assert_eq!(retry_class("file_write"), RetryClass::IdempotentWrite);
        assert_eq!(retry_class("handover_write"), RetryClass::IdempotentWrite);
        assert_eq!(retry_class("shell_exec"), RetryClass::NonRetryable);
        assert_eq!(retry_class("file_delete"), RetryClass::NonRetryable);
        assert_eq!(retry_class("session_spawn"), RetryClass::NonRetryable);
        assert_eq!(retry_class("http_get"), RetryClass::NonRetryable);
        assert_eq!(
            retry_class("no_such_tool"),
            RetryClass::NonRetryable,
            "未知工具缺省不重试"
        );
    }

    #[test]
    fn transient_discrimination() {
        // 瞬态:连接/超时/限流/5xx 文本形态;非瞬态:参数/权限/策略形态
        for s in [
            "error sending request: connection refused",
            "operation timed out",
            "service unavailable",
            "too many requests (429)",
            "internal server error (500)",
        ] {
            assert!(is_transient_error(s), "{s} 应判瞬态");
        }
        for s in [
            "invalid arguments: missing field `path`",
            "permission denied",
            "tool not found: no_such_tool",
            "adjudication denied by policy",
        ] {
            assert!(!is_transient_error(s), "{s} 不应判瞬态");
        }
    }
}
