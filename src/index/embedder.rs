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
//! 纪律）。治理面（15 号 §四）：本地模型零外发；模型下载属一次性显式触发
//! 外发（首次启用语义面时明示大小与来源），实现方承载该明示义务。

use crate::index::store::IndexError;

/// 文本嵌入器（语义面核心抽象；驻留 [`crate::index::IndexService`]，须 Send+Sync）
pub trait TextEmbedder: Send + Sync {
    /// 批量嵌入（输出顺序与 `texts` 一一对应；空输入返回空）
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, IndexError>;

    /// 模型名（status 面展示）
    fn model_name(&self) -> String;
}
