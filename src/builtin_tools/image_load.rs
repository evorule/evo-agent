// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `image_load` —— 读取图片文件并以旁路 images 形态返回（B1 多模态上行触发源）
//!
//! ## 安全模型（与 file_read 同族）
//! - 路径必须**相对** workdir（绝对路径直接拒）
//! - 拒绝 `..` 路径段；canonicalize 后必须仍在 workdir 内（防 symlink 攻击）
//! - 只读常规文件（拒绝目录、socket、设备等）
//! - 文件大小上限（原始字节 ≤ 1.5 MB——与 runner 侧单图 base64 上限
//!   2 MB 字符同口径：1.5 MB 原始字节 base64 后 ≈ 2.0 MB 字符，恰在门内）
//!
//! ## mime 判定
//! 按魔数头判定（不信任扩展名）：PNG / JPEG / GIF / WEBP 四种，非图文件
//! 拒绝（fail-visible 错误信息）。
//!
//! ## 返回形态
//! `{"path","size","mime_type","images":[{"mime_type","data_base64"}]}`——
//! 顶层 `images` 键即 runner 侧 `extract_images_from_raw` 的提取协议触发键，
//! 工具结果回喂时图片自动进旁路上行通道（content 面只剩指纹）。

use std::path::PathBuf;

use serde_json::Value;

use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;

/// 最大文件大小（原始字节，1.5 MB；base64 后约 2.0 MB 字符，恰在 runner
/// 侧单图预算门 B1_MAX_IMAGE_B64_CHARS=2MB 内）
pub const IMAGE_LOAD_MAX_BYTES: u64 = 1_572_864;

/// `image_load` 工具
///
/// 通过 `new()` 构造时绑定工作目录，实例化后**只能读取 workdir 内的图片**。
#[derive(Clone)]
pub struct ImageLoadTool {
    workdir: PathBuf,
    max_bytes: u64,
}

impl ImageLoadTool {
    /// 创建绑定到 `workdir` 的 image_load 工具
    pub fn new(workdir: PathBuf) -> Self {
        Self {
            workdir,
            max_bytes: IMAGE_LOAD_MAX_BYTES,
        }
    }

    /// 按魔数头判定 mime；非图返回 None
    fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some("image/png")
        } else if bytes.starts_with(b"\xFF\xD8\xFF") {
            Some("image/jpeg")
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some("image/gif")
        } else if bytes.len() >= 12
            && &bytes[0..4] == b"RIFF"
            && &bytes[8..12] == b"WEBP"
        {
            Some("image/webp")
        } else {
            None
        }
    }
}

#[async_trait::async_trait]
impl ToolFunction for ImageLoadTool {
    /// G13:async 入口 — 用 spawn_blocking 包装同步 fs 操作
    async fn call(&self, args: &Value) -> IoResult {
        let tool = self.clone();
        let args = args.clone();
        tokio::task::spawn_blocking(move || tool.call_sync(&args))
            .await
            .map_err(|e| format!("image_load tool panicked: {}", e))?
    }
}

impl ImageLoadTool {
    /// 同步实现(供 spawn_blocking 调用)
    fn call_sync(&self, args: &Value) -> IoResult {
        use base64::Engine as _;

        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required arg: path (string)".to_string())?;

        let safe_path =
            crate::builtin_tools::fs_safety::resolve_existing(&self.workdir, path)?;

        let metadata = std::fs::metadata(&safe_path).map_err(|e| format!("stat failed: {}", e))?;

        if !metadata.is_file() {
            return Err(format!("not a regular file: '{}'", path));
        }

        if metadata.len() > self.max_bytes {
            return Err(format!(
                "file too large: {} bytes (max {} bytes for multimodal uplink)",
                metadata.len(),
                self.max_bytes
            ));
        }

        let bytes = std::fs::read(&safe_path)
            .map_err(|e| format!("read failed: {}", e))?;

        let mime = Self::sniff_mime(&bytes).ok_or_else(|| {
            "not a recognized image (magic check failed; supported: PNG/JPEG/GIF/WEBP)".to_string()
        })?;

        let mut map = serde_json::Map::new();
        map.insert(
            "path".to_string(),
            Value::from(safe_path.display().to_string()),
        );
        map.insert("size".to_string(), Value::from(metadata.len() as i64));
        map.insert("mime_type".to_string(), Value::from(mime));
        map.insert(
            "images".to_string(),
            serde_json::json!([{
                "mime_type": mime,
                "data_base64": base64::engine::general_purpose::STANDARD.encode(&bytes),
            }]),
        );
        Ok(Value::Object(map))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;

    fn temp_workdir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let _ = dir.path().canonicalize();
        dir
    }

    fn make_tool(dir: &Path) -> ImageLoadTool {
        ImageLoadTool::new(dir.to_path_buf())
    }

    /// 最小合法 PNG 头(魔数 8 字节)
    fn png_bytes() -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(b"fake-png-body");
        v
    }

    #[test]
    fn test_load_png_returns_images_shape() {
        let dir = temp_workdir();
        std::fs::write(dir.path().join("img.png"), png_bytes()).unwrap();

        let tool = make_tool(dir.path());
        let result = tool.call_sync(&serde_json::json!({"path": "img.png"}));
        let v = result.expect("load should succeed");
        assert_eq!(v.get("mime_type").unwrap().as_str().unwrap(), "image/png");
        let images = v.get("images").unwrap().as_array().unwrap();
        assert_eq!(images.len(), 1);
        let data = images[0].get("data_base64").unwrap().as_str().unwrap();
        assert!(!data.is_empty());
        // base64 解码回原字节(往返一致)
        use base64::Engine as _;
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .unwrap(),
            png_bytes()
        );
    }

    #[test]
    fn test_reject_non_image_magic() {
        let dir = temp_workdir();
        std::fs::write(dir.path().join("fake.png"), b"this is not an image").unwrap();

        let tool = make_tool(dir.path());
        let result = tool.call_sync(&serde_json::json!({"path": "fake.png"}));
        let err = result.unwrap_err();
        assert!(err.contains("magic check failed"), "got: {}", err);
    }

    #[test]
    fn test_reject_file_too_large() {
        let dir = temp_workdir();
        let big = dir.path().join("big.png");
        // 稀疏文件:合法 PNG 头 + set_len 撑到 4 MB(size 门先于魔数判定)
        let mut f = std::fs::File::create(&big).unwrap();
        use std::io::Write as _;
        f.write_all(&png_bytes()).unwrap();
        f.set_len(4 * 1024 * 1024).unwrap();
        drop(f);

        let tool = make_tool(dir.path());
        let result = tool.call_sync(&serde_json::json!({"path": "big.png"}));
        let err = result.unwrap_err();
        assert!(err.contains("too large"), "got: {}", err);
    }

    #[test]
    fn test_reject_absolute_path() {
        let dir = temp_workdir();
        let tool = make_tool(dir.path());
        let abs = if cfg!(windows) {
            "C:\\Windows\\System32"
        } else {
            "/etc/passwd"
        };
        let result = tool.call_sync(&serde_json::json!({"path": abs}));
        assert!(result.unwrap_err().contains("absolute path not allowed"));
    }

    #[test]
    fn test_reject_parent_dir_traversal() {
        let dir = temp_workdir();
        let tool = make_tool(dir.path());
        let result = tool.call_sync(&serde_json::json!({"path": "../etc/passwd"}));
        assert!(result.unwrap_err().contains("parent dir (..) not allowed"));
    }

    #[test]
    fn test_reject_directory_and_missing() {
        let dir = temp_workdir();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let tool = make_tool(dir.path());
        let result = tool.call_sync(&serde_json::json!({"path": "sub"}));
        assert!(result.unwrap_err().contains("not a regular file"));

        let result = tool.call_sync(&serde_json::json!({"path": "nope.png"}));
        assert!(result.is_err());

        let result = tool.call_sync(&Value::Object(serde_json::Map::new()));
        assert!(result.unwrap_err().contains("missing required arg"));
    }
}
