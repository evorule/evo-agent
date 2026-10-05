// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! journal_gc——journal 体积治理离线命令（规格修正批批次四/K-01）。
//!
//! 用法：
//! ```text
//! journal_gc <journal-dir> [--wire-blob-days 7] [--expire-days 90] [--dry-run]
//! ```
//!
//! 分层保留：骨架事件永久；wire_rendered 全文超窗入旁路归档件，超过期窗
//! 删除（journal 落 WireBlobExpired 标记事件，降级可见）。活跃会话拒绝
//! （fail-visible）；dry-run 只报数不改文件。
use evo_agent::agent::journal::gc_wire_blobs;
use std::path::PathBuf;

struct CliArgs {
    dir: PathBuf,
    wire_blob_days: u64,
    expire_days: u64,
    dry_run: bool,
}

fn parse_args() -> Result<CliArgs, String> {
    let mut args = CliArgs {
        dir: PathBuf::new(),
        wire_blob_days: 7,
        expire_days: 90,
        dry_run: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--wire-blob-days" => {
                args.wire_blob_days = it
                    .next()
                    .ok_or("--wire-blob-days 缺值")?
                    .parse()
                    .map_err(|_| "--wire-blob-days 需正整数")?
            }
            "--expire-days" => {
                args.expire_days = it
                    .next()
                    .ok_or("--expire-days 缺值")?
                    .parse()
                    .map_err(|_| "--expire-days 需正整数")?
            }
            "--dry-run" => args.dry_run = true,
            other if other.starts_with('-') => return Err(format!("未知参数 {other}")),
            other => args.dir = PathBuf::from(other),
        }
    }
    if args.dir.as_os_str().is_empty() {
        return Err("缺 <journal-dir>（journal 目录）".into());
    }
    Ok(args)
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("参数错误: {e}");
            std::process::exit(2);
        }
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    if args.dry_run {
        println!(
            "[dry-run] dir={} wire_blob_days={} expire_days={}(干跑只报参,不改文件;正式跑去掉 --dry-run)",
            args.dir.display(),
            args.wire_blob_days,
            args.expire_days
        );
        return;
    }
    match gc_wire_blobs(&args.dir, now_ms, args.wire_blob_days, args.expire_days) {
        Ok(r) => {
            println!("=== journal GC（wire blob 分层保留治理） ===");
            println!(
                "files={} wire_archived={} wire_expired={} bytes {} → {}",
                r.files_scanned, r.wire_archived, r.wire_expired, r.bytes_before, r.bytes_after
            );
            println!(
                "(注) 骨架事件永久保留;过期 blob 降级为 hash 校验(WireBlobExpired 事件在账);I4 全文级重建保证以保留期窗口为界"
            );
        }
        Err(e) => {
            eprintln!("GC 失败: {e}");
            std::process::exit(1);
        }
    }
}
