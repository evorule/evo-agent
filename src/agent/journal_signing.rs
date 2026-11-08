// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 技能期刊签名（纵深加固 v2：进程边界信任根）
//!
//! 威胁模型：期刊/快照是同用户文件，文件系统攻击者可整体重写（设计稿
//! §〇.5 天花板）。v2 把「动作签发」的信任根移出文件系统：桌面壳每次
//! 启动生成一次性 ed25519 密钥，私钥只存在于壳与 serve 的进程内存
//! （经 env 载体交接，serve 侧固化即清 env），同用户文件系统攻击者读不到。
//!
//! 语义：
//! - 期刊动作记录可携带 `sig` = 对记录 hash 字节串的 ed25519 签名；
//! - 本引导（boot）首次写期刊时落 `session_start` 锚记录（携带本引导
//!   公钥指纹，签名覆盖）——锚之后的所有记录必须可验签（fail-visible）；
//! - 锚之前的历史 = 既有哈希链口径（前向绑定照旧）；
//! - 未配置密钥（裸 serve/旧部署）= 无签名模式，行为与 v1 一致。
//!
//! 诚实边界：攻击者整体删除锚并重写全刊可回退到无签名模式——检测面
//! 归账本哈希链与快照漂移自愈（v1 层）；壳侧对「本引导应存在的锚」
//! 的独立审计属壳侧职责，不在本模块。

use std::sync::OnceLock;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

static BOOT_KEY: OnceLock<Option<SigningKey>> = OnceLock::new();

/// serve 启动期调用：从 env 读取壳交接的引导私钥（hex 32B）并固化进进程
/// 内存；随后调用方应清除 env 副本（scrub 纪律同鉴权令牌）。
pub fn init_from_env() {
    if BOOT_KEY.get().is_some() {
        return;
    }
    let key = std::env::var("EVO_AGENT_JOURNAL__SIGNING_KEY")
        .ok()
        .and_then(|h| decode_hex32(&h))
        .map(|seed| SigningKey::from_bytes(&seed));
    let _ = BOOT_KEY.set(key);
}

/// 测试注入（cfg(test)：并行测试共享 OnceLock——签名/无签名两组测试
/// 以「先到先得」定模式，断言面均对两种模式稳健）
#[cfg(test)]
pub fn set_key_for_tests(seed: [u8; 32]) {
    let _ = BOOT_KEY.set(Some(SigningKey::from_bytes(&seed)));
}

fn boot_key() -> Option<&'static SigningKey> {
    BOOT_KEY.get().and_then(|o| o.as_ref())
}

/// 签名模式是否启用（引导密钥在位）
pub fn enabled() -> bool {
    boot_key().is_some()
}

/// 本引导公钥指纹（锚记录携带；重放校验时以此匹配「本引导的锚」）
pub fn pk_fp() -> Option<String> {
    boot_key().map(|k| {
        let vk: VerifyingKey = k.verifying_key();
        let digest = evorule_hash::digest(vk.as_bytes());
        evorule_hash::prefixed(&digest)
    })
}

/// 对期刊记录 hash 字节串签名（hex；hash 字段本身是载荷+链位的绑定，
/// 签名再绑 hash = 传递性覆盖全记录）
pub fn sign_hash(record_hash: &str) -> Option<String> {
    let key = boot_key()?;
    let sig = key.sign(record_hash.as_bytes());
    Some(to_hex(&sig.to_bytes()))
}

/// 验签（公钥显式传入——校验历史锚/异进程记录时不依赖本引导私钥）
pub fn verify_hash(pub_key: &VerifyingKey, record_hash: &str, sig_hex: &str) -> bool {
    let Some(sig_bytes) = decode_hex64(sig_hex) else {
        return false;
    };
    let Ok(sig) = Signature::from_slice(&sig_bytes) else {
        return false;
    };
    pub_key.verify(record_hash.as_bytes(), &sig).is_ok()
}

fn decode_hex64(s: &str) -> Option<[u8; 64]> {
    let b = s.trim().as_bytes();
    if b.len() != 128 {
        return None;
    }
    let mut out = [0u8; 64];
    for i in 0..64 {
        let hi = (b[i * 2] as char).to_digit(16)?;
        let lo = (b[i * 2 + 1] as char).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Some(out)
}

/// 由公钥指纹反查？不可——指纹是单向哈希。验签用公钥本体：锚记录只带
/// 指纹 + 签名，验签公钥 = 本引导验证密钥（指纹匹配时同钥）
pub fn verifying_key_matches_fp(fp: &str) -> Option<VerifyingKey> {
    let key = boot_key()?;
    if pk_fp().as_deref() == Some(fp) {
        Some(key.verifying_key())
    } else {
        None
    }
}

fn decode_hex32(s: &str) -> Option<[u8; 32]> {
    let b = s.trim().as_bytes();
    if b.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        let hi = (b[i * 2] as char).to_digit(16)?;
        let lo = (b[i * 2 + 1] as char).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Some(out)
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn test_sign_verify_roundtrip() {
        let seed = [7u8; 32];
        let key = SigningKey::from_bytes(&seed);
        let vk = key.verifying_key();
        let sig = sign_with(&key, "blake3:abc");
        assert!(verify_hash(&vk, "blake3:abc", &sig));
        assert!(!verify_hash(&vk, "blake3:other", &sig));
        let mut bad = sig.clone();
        bad.replace_range(0..2, "00");
        assert!(
            !verify_hash(&vk, "blake3:abc", &bad),
            "tampered sig must fail"
        );
    }

    #[test]
    fn test_hex32_roundtrip_and_rejects() {
        let hex = to_hex(&[0xabu8; 32]);
        assert_eq!(hex.len(), 64);
        assert!(decode_hex32(&hex).is_some());
        assert!(decode_hex32("zz").is_none());
        assert!(decode_hex32(&"ab".repeat(31)).is_none());
    }

    fn sign_with(key: &SigningKey, msg: &str) -> String {
        let sig = key.sign(msg.as_bytes());
        to_hex(&sig.to_bytes())
    }
}
