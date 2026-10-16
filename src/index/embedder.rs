// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 文本嵌入器抽象（B14 v1.x 语义面，PR-5）
//!
//! 解耦设计：编排层（[`crate::index::IndexService`]）只依赖 [`TextEmbedder`]
//! trait；fastembed 本地模型实现随 PR-5b 落地，测试以假实现驱动（无网安全）。
//!
//! 调用约定：批量接口（一次调用喂多文本，输出顺序与输入一一对应）；调用方
//! 在专用阻塞线程执行（CPU 密集，不占 tokio worker——与 tree-sitter 解析同款
//! 纪律）。治理面：本地模型零外发；模型下载属一次性显式触发
//! 外发（首次启用语义面时明示大小与来源），实现方承载该明示义务。

use std::path::PathBuf;
use std::sync::Mutex;

use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};

use crate::index::store::IndexError;

/// 文本嵌入器（语义面核心抽象；驻留 [`crate::index::IndexService`]，须 Send+Sync）
pub trait TextEmbedder: Send + Sync {
    /// 批量嵌入（输出顺序与 `texts` 一一对应；空输入返回空）
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, IndexError>;

    /// 模型名（status 面展示）
    fn model_name(&self) -> String;
}

/// 语义面缺省模型目录：`<用户主目录>/.evo-agent/models/`
///
/// 与索引库同侧用户目录惯例（[`crate::index::index_dir`] 同款形态）——
/// 模型一次性下载后跨启动复用，运行时零外发。
pub fn default_model_dir() -> Result<PathBuf, IndexError> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map_err(|_| IndexError("user home dir not found (USERPROFILE/HOME)".into()))?;
    Ok(PathBuf::from(home).join(".evo-agent").join("models"))
}

/// fastembed 本地嵌入器（裁决实现：all-MiniLM-L6-v2 q8）
///
/// - 模型加载即 [`TextEmbedding::try_new`]：首次启用触发一次性显式下载
///   （HuggingFace CDN，进度明示；`HF_ENDPOINT` 可指镜像，`HF_HOME` 优先）；
///   缓存落 [`default_model_dir`]，就位后运行时零外发；
/// - 嵌入调用为 CPU 密集，约定在专用阻塞线程执行（编排层纪律，本类型不加线程）；
/// - `Mutex` 串行化（ORT session 非并发调用契约；嵌入批量本就摊薄调用频次）。
pub struct FastembedEmbedder {
    model: Mutex<TextEmbedding>,
    model_name: String,
}

impl FastembedEmbedder {
    /// 构造并加载缺省模型（all-MiniLM-L6-v2 q8，384 维）
    ///
    /// 模型未缓存时触发下载（一次性，显式启用语义面即用户知情触发）。
    pub fn new(cache_dir: &std::path::Path) -> Result<Self, IndexError> {
        let model = TextEmbedding::try_new(
            TextInitOptions::new(EmbeddingModel::AllMiniLML6V2Q)
                .with_show_download_progress(true)
                .with_cache_dir(cache_dir.to_path_buf()),
        )
        .map_err(|e| IndexError(format!("fastembed init: {e}")))?;
        Ok(Self {
            model: Mutex::new(model),
            model_name: "all-MiniLM-L6-v2 (q8, 384d)".into(),
        })
    }
}

impl TextEmbedder for FastembedEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, IndexError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut model = self.model.lock().unwrap_or_else(|p| p.into_inner());
        let embeddings = model
            .embed(texts, None)
            .map_err(|e| IndexError(format!("fastembed embed: {e}")))?;
        Ok(embeddings
            .into_iter()
            .map(|e| e.into_iter().collect())
            .collect())
    }

    fn model_name(&self) -> String {
        self.model_name.clone()
    }
}
