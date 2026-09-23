//! serve 启动期 `.env` 自动加载(零第三方依赖的最小实现)
//!
//! 背景(登记册 O-095):serve 不读 `.env`,重启漏注入环境变量导致 LLM 密钥丢失、
//! agent 会话 LLM 调用 401「假性失联」。本模块在 serve 启动最早期把工作目录或
//! 可执行文件目录下的 `.env` 注入进程环境,规则:
//! - 只加载**第一个**存在的候选文件(优先 `workdir/.env`,其次 exe 同目录 `.env`)
//! - **已设置的环境变量优先**,`.env` 不覆盖(显式注入/系统环境 > 文件)
//! - 值可带成对单/双引号(剥掉);支持可选 `export ` 前缀;`#` 开头行与空行跳过;
//!   不处理行内注释与值内转义(最小实现,见 `parse_env_contents` 单测口径)

use std::path::{Path, PathBuf};

/// 解析 `.env` 文本为键值对。畸形行(无 `=`、空键)跳过;键名非法字符不校验
/// (交由环境变量语义兜底),重复键以先者为准(与常见 dotenv 语义一致)。
pub fn parse_env_contents(content: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let mut value = value.trim();
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = &value[1..value.len() - 1];
        }
        pairs.push((key.to_string(), value.to_string()));
    }
    pairs
}

/// 把键值对注入进程环境(不覆盖已存在的变量),返回实际注入的条数。
pub fn apply_env_pairs(pairs: &[(String, String)]) -> usize {
    let mut applied = 0;
    for (key, value) in pairs {
        if std::env::var(key).is_err() {
            std::env::set_var(key, value);
            applied += 1;
        }
    }
    applied
}

/// serve 启动期加载 `.env`:依次尝试 `workdir/.env` 与 `exe 同目录/.env`,
/// 只加载第一个存在的文件。返回 (加载的文件路径, 注入条数);无文件返回 None。
pub fn load_dotenv_for(workdir: &Path) -> Option<(PathBuf, usize)> {
    let mut candidates = vec![workdir.join(".env")];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(".env"));
        }
    }
    for path in candidates {
        if let Ok(content) = std::fs::read_to_string(&path) {
            let pairs = parse_env_contents(&content);
            let applied = apply_env_pairs(&pairs);
            return Some((path, applied));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skips_comments_blanks_and_malformed() {
        let content = "# comment\n\n\nA=1\nNO_EQUALS_SIGN\n=empty_key\n  B = spaced \n";
        let pairs = parse_env_contents(content);
        assert_eq!(
            pairs,
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "spaced".to_string()),
            ]
        );
    }

    #[test]
    fn parse_strips_matching_quotes_and_export_prefix() {
        let content = "K=\"v1\"\nK2='v2'\nK3=\"unmatched\nexport K4=v4\nEMPTY=\n";
        let pairs = parse_env_contents(content);
        assert_eq!(pairs[0], ("K".to_string(), "v1".to_string()));
        assert_eq!(pairs[1], ("K2".to_string(), "v2".to_string()));
        assert_eq!(pairs[2], ("K3".to_string(), "\"unmatched".to_string()));
        assert_eq!(pairs[3], ("K4".to_string(), "v4".to_string()));
        assert_eq!(pairs[4], ("EMPTY".to_string(), String::new()));
    }

    #[test]
    fn parse_keeps_value_with_equals_sign() {
        let pairs = parse_env_contents("CONN=host=127.0.0.1;port=5432\n");
        assert_eq!(
            pairs,
            vec![("CONN".to_string(), "host=127.0.0.1;port=5432".to_string())]
        );
    }

    #[test]
    fn apply_does_not_override_existing() {
        let unique = "EVO_AGENT_DOTENV_TEST_UNIQUE_KEY";
        std::env::set_var(unique, "original");
        let applied = apply_env_pairs(&[(unique.to_string(), "replacement".to_string())]);
        assert_eq!(applied, 0);
        assert_eq!(std::env::var(unique).unwrap(), "original");
        std::env::remove_var(unique);
    }
}
