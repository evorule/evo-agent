// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! E-2(design-36 期3):Windows AppContainer 低盒(tier-2)执行面
//!
//! ## 定位
//!
//! tier-1 容器桥(DockerExec)不可用时的 OS 级兜底:纯 Windows 能力,零外部
//! 依赖(无 Docker/无虚拟机),覆盖 CLI/桌面直装用户。**职责分离形态**
//! (design-36 §6.4 裁定):低盒只跑不可信计算型命令(首航清单 = cargo 链三件,
//! spike 实证闭环见 design-36 §6.1/§6.2),git 等工作流编排留宿主面(mingw/
//! MSYS2 运行时与 AppContainer 系统性不兼容,已隔离根因)。
//!
//! ## 全模块 unsafe 例外声明
//!
//! 本模块是**全仓唯一**显式 `allow(unsafe_code)` 收口点:AppContainer 拉起
//! (PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES)与 ACL 授权必须 Win32 FFI,
//! std 无兑现形态。crate 级与 builtin_tools 模块级哨兵已由 forbid 降为 deny
//! (allow 只能覆盖 deny),本模块之外任何新 unsafe 仍默认编译错误。
//! 每个 unsafe 块均携带 SAFETY 注释。
//!
//! ## 边界语义(低盒内进程拿到的)
//!
//! - **网络物理断绝**:SECURITY_CAPABILITIES capabilities=None = 缺省禁网
//!   (loopback 与外网双拒,design-36 §6.1 ③ 实证),比白名单守卫更强;
//! - **文件系统**:仅 workdir(GENERIC_ALL,(OI)(CI) 继承)+ 工具链映像
//!   (VS/SDK/cargo/rustup 根,RX)+ 系统默认可读面;其余全拒;
//! - **环境块白名单**(§6.3):父环境白名单过滤构建,宿主密钥不入块
//!   (LLM API key 等天然隔离)。
//!
//! ## profile 生命周期(per-execution;§6.5/§6.6)
//!
//! 每次执行:Derive 先查(存在即复用)→ 不存在则 Create → 用后即
//! Delete。SID 由 name 哈希派生(同名同 SID),ACL 中的 ACE 不随 profile
//! 删除失效(工具链授权进程级幂等即可,workdir 每次自愈授权)。

#![allow(unsafe_code)]

use std::cmp::Ordering;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, LocalFree, SetHandleInformation, GENERIC_ALL, GENERIC_EXECUTE, GENERIC_READ,
    HANDLE, HANDLE_FLAGS, HANDLE_FLAG_INHERIT, HLOCAL, WAIT_OBJECT_0, WAIT_TIMEOUT, WIN32_ERROR,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW,
    EXPLICIT_ACCESS_W, NO_MULTIPLE_TRUSTEE, SET_ACCESS, SE_FILE_OBJECT, TRUSTEE_IS_SID,
    TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows::Win32::Security::{
    AclSizeInformation, CreateWellKnownSid, EqualSid, GetAce, GetAclInformation,
    WinBuiltinAnyPackageSid, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION,
    CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, OBJECT_INHERIT_ACE, PSECURITY_DESCRIPTOR,
    PSID, SECURITY_CAPABILITIES, SID,
};
use windows::Win32::Storage::FileSystem::{GetShortPathNameW, ReadFile};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, UpdateProcThreadAttribute, WaitForSingleObject,
    CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

/// 低盒 profile 名(固定:SID 由 name 哈希派生,同名同 SID;§6.6)
pub(crate) const LOWBOX_PROFILE_NAME: &str = "evoagent-shellbox";

/// 低盒等待轮询间隔(与 shell_exec 宿主路径同粒度)
const POLL_INTERVAL_MS: u32 = 50;

/// 超时终止后主进程回收宽限(taskkill /T /F 投递后句柄级等待上限)
const TERMINATE_GRACE_MS: u32 = 30_000;

/// 低盒执行规格(构造面:`program` 必须绝对路径——CreateProcessW 的
/// lpApplicationName 不做 PATH 搜索;宿主侧定位见 [`locate_program_in_path`])
pub struct LowboxSpec {
    /// 低盒内执行的程序绝对路径(宿主 PATH 定位;workdir 遮蔽检查由调用方先行)
    pub program: PathBuf,
    /// 程序参数(argv[1..];低盒无 shell 解释器,引号规则见 [`quote_arg`])
    pub args: Vec<String>,
    /// 工作目录(低盒对该目录获 GENERIC_ALL;缺省目录授权面同)
    pub workdir: PathBuf,
    /// 执行超时(有界等待;到期终止低盒进程族)
    pub timeout: Duration,
}

/// 低盒执行结局(形态对齐宿主 [`std::process::Output`],由 shell_exec 层
/// 复用 completed_result/report_terminated 组装结果 JSON)
pub struct LowboxOutcome {
    /// 主进程 pid(轨迹审计留痕)
    pub pid: u32,
    /// 是否超时终止(true = 走 killed 结局;false = 自然退出)
    pub timed_out: bool,
    /// 进程结局+输出(status 以 Windows ExitStatus 裸码构造)
    pub output: std::process::Output,
}

// ─────────────────────────── 基础工具 ───────────────────────────

/// UTF-16 + NUL 结尾(wide 字符串构造)
fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// HANDLE 跨线程转移壳(HANDLE 本体非 Send:裸指针形态)。此处安全依据:
/// 管道读端自 CreateProcessW 后父侧仅转移给 drain 线程独占使用,无跨线程
/// 别名访问(内核对象句柄本身可跨线程使用,Send 缺省是 Rust 侧保守标记)
struct SendHandle(HANDLE);

impl SendHandle {
    fn get(&self) -> HANDLE {
        self.0
    }
}

// SAFETY:见类型注释——所有权独占转移,无别名访问
unsafe impl Send for SendHandle {}

/// HANDLE RAII 壳(Drop = CloseHandle;spawn 路径早退不漏句柄)
struct HandleBox(HANDLE);

impl Drop for HandleBox {
    fn drop(&mut self) {
        // SAFETY:句柄有效性由持有期不变式保证(仅收 CreatePipe/CreateProcessW 产物)
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// LocalFree RAII 壳(收 GetNamedSecurityInfoW 的 SECURITY_DESCRIPTOR 等
/// LocalAlloc 产物;早退路径不漏)
struct LocalFreeGuard(HLOCAL);

impl Drop for LocalFreeGuard {
    fn drop(&mut self) {
        // SAFETY:仅收 Win32 分配器(LocalAlloc 家族)产物
        unsafe {
            LocalFree(Some(self.0));
        }
    }
}

// ─────────────────── 环境块白名单(design-36 §6.3) ───────────────────

/// 环境变量白名单(大小写不敏感匹配;§6.3 坑1:真实环境名 Path/windir/
/// ComSpec 混合大小写,精确匹配会把 PATH 滤掉)。密钥面(LLM API key 等)
/// 不在表内天然不入块;`=C:` 驱动器映像项与 `__COMPAT_LAYER` 同理(§6.3
/// 矩阵实测均非必要)。
const ENV_WHITELIST: &[&str] = &[
    "APPDATA",
    "COMMONPROGRAMFILES",
    "COMMONPROGRAMFILES(X86)",
    "COMPUTERNAME",
    "COMSPEC",
    "HOMEDRIVE",
    "HOMEPATH",
    "LOCALAPPDATA",
    "NUMBER_OF_PROCESSORS",
    "PATH",
    "PATHEXT",
    "PROCESSOR_ARCHITECTURE",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "SYSTEMDRIVE",
    "SYSTEMROOT",
    "TEMP",
    "TMP",
    "USERNAME",
    "USERPROFILE",
    "WINDIR",
];

/// 构建低盒环境块(纯函数;父环境白名单过滤 + cargo 助推项并入)
///
/// 规则(design-36 §6.3):
/// - 父环境按白名单过滤(变量名大小写不敏感);`extra` 为宿主探测构造面
///   (MSVC 助推,非用户输入),后写覆盖同名项;
/// - **必含 LOCALAPPDATA**(§6.3 坑2:低盒初始化定位
///   `%LOCALAPPDATA%\Packages\<SID>` 依赖它,缺失则 CreateProcessW 报
///   0x800700CB)——parent 与 extra 均无即 Err(fail-visible);
/// - 输出按变量名(大小写不敏感)排序(确定性形态,便于测试与审计);
/// - 形态 = UTF-16,每项 `NAME=value\0`,块尾额外双 `\0`。
pub(crate) fn build_environment_block(
    parent: &[(String, String)],
    extra: &[(String, String)],
) -> Result<Vec<u16>, String> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (k, v) in parent {
        let ku = k.to_uppercase();
        // 白名单过滤:密钥/`=` 开头特殊项/__COMPAT_LAYER 均不在表内,天然滤除
        if !ENV_WHITELIST.contains(&ku.as_str()) {
            continue;
        }
        out.push((k.clone(), v.clone()));
    }
    for (k, v) in extra {
        // cargo 助推项覆盖父环境同名命中(RUSTFLAGS 的 linker 注入不可被
        // 父环境既有 RUSTFLAGS 抢占)
        if let Some(slot) = out.iter_mut().find(|(pk, _)| pk.eq_ignore_ascii_case(k)) {
            slot.1 = v.clone();
        } else {
            out.push((k.clone(), v.clone()));
        }
    }
    if !out
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("LOCALAPPDATA"))
    {
        return Err(
            "LOCALAPPDATA missing from environment block (AppContainer init requires it; \
             CreateProcessW would fail with 0x800700CB)"
                .to_string(),
        );
    }
    out.sort_by_key(|a| a.0.to_uppercase());
    let mut block: Vec<u16> = Vec::new();
    for (k, v) in &out {
        block.extend(format!("{k}={v}").encode_utf16());
        block.push(0);
    }
    block.push(0); // 双 NUL 块尾
    Ok(block)
}

/// 父环境采集(std 面;非 UTF-8 变量以 lossy 形态入过滤面——白名单外项
/// 直接滤除,密钥等敏感项均为常规 UTF-8 形态,过滤面不受影响)
fn parent_env() -> Vec<(String, String)> {
    std::env::vars_os()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect()
}

// ─────────────────── profile 生命周期(§6.5/§6.6) ───────────────────

/// SID 缓冲(原始字节;AppContainer SID 由 name 哈希派生)
#[derive(Clone)]
struct SidBuf {
    bytes: Vec<u8>,
}

impl SidBuf {
    fn as_psid(&self) -> PSID {
        PSID(self.bytes.as_ptr() as *mut c_void)
    }
}

/// 从派生 API 返回的 PSID 拷出 SID 原始字节并释放源(LocalAlloc 产物)
///
/// SAFETY:`sid` 必须是 Win32 API 返回的有效 SID 指针(LocalAlloc 语义)。
unsafe fn take_ownership_of_sid(sid: PSID) -> Result<SidBuf, String> {
    if sid.0.is_null() {
        return Err("null PSID from AppContainer SID API".to_string());
    }
    // SID 布局:Revision(1) + SubAuthorityCount(1) + IdentifierAuthority(6)
    // + SubAuthority[count]*4 → 长度 = 8 + 4*count
    let count = (*(sid.0 as *const SID)).SubAuthorityCount as usize;
    let len = 8 + 4 * count;
    let bytes = std::slice::from_raw_parts(sid.0 as *const u8, len).to_vec();
    LocalFree(Some(HLOCAL(sid.0)));
    Ok(SidBuf { bytes })
}

/// 派生 AppContainer SID(profile 已存在路径)
fn derive_sid(name_wide: &[u16]) -> Result<SidBuf, String> {
    // SAFETY:name_wide 以 NUL 结尾;返回的 PSID 为 LocalAlloc 语义,即刻收编
    let sid = unsafe {
        DeriveAppContainerSidFromAppContainerName(PCWSTR::from_raw(name_wide.as_ptr()))
            .map_err(|e| format!("DeriveAppContainerSidFromAppContainerName: {e}"))?
    };
    // SAFETY:上一行成功即返回 LocalAlloc 语义的合法 SID
    unsafe { take_ownership_of_sid(sid) }
}

/// SID 的字符串形态(S-1-15-2-…;icacls 运维提示与日志用)
fn sid_string(sid: &SidBuf) -> String {
    let mut pw = PWSTR::null();
    // SAFETY:sid 内部缓冲即合法 SID;出参指针有效
    unsafe {
        if ConvertSidToStringSidW(sid.as_psid(), &mut pw).is_ok() && !pw.as_ptr().is_null() {
            let s = pw.to_string().unwrap_or_default();
            LocalFree(Some(HLOCAL(pw.as_ptr() as *mut c_void)));
            s
        } else {
            "<sid-string unavailable>".to_string()
        }
    }
}

/// 低盒 profile(per-execution 生命周期:ensure 创建/复用,Drop 即删;§6.5
/// 「用后必须 Delete」)。SID 同名同派生,ACE 不随删除失效(§6.6)。
struct LowboxProfile {
    name_wide: Vec<u16>,
    name: String,
    sid: SidBuf,
}

impl LowboxProfile {
    fn ensure(name: &str) -> Result<Self, String> {
        let name_wide = to_wide_null(name);
        // 注意:DeriveAppContainerSidFromAppContainerName 是**纯哈希派生**——
        // profile 不存在也返回 Ok(探针实证),不能当存在性判据。存在性以
        // Create 的 already-exists(0x800700B7)为准:create-first,已存在
        // 即复用,其余 Err 传播(fail-visible)。
        let display = to_wide_null("evo-agent shell sandbox");
        let desc = to_wide_null("evo-agent tier-2 lowbox execution profile (per-execution)");
        let create_result: Result<(), windows::core::Error> = {
            // SAFETY:全部 wide 串 NUL 结尾
            unsafe {
                CreateAppContainerProfile(
                    PCWSTR::from_raw(name_wide.as_ptr()),
                    PCWSTR::from_raw(display.as_ptr()),
                    PCWSTR::from_raw(desc.as_ptr()),
                    None,
                )
            }
            .map(|psid| {
                // SAFETY:LocalAlloc 语义 SID,即刻拷贝释放
                unsafe {
                    let _ = take_ownership_of_sid(psid);
                }
            })
        };
        if let Err(e) = &create_result {
            // 0x800700B7 = ERROR_ALREADY_EXISTS:前次执行残留/并发持有,复用;
            // 其余 Err 传播(fail-visible)
            if e.code() != windows::core::HRESULT(0x8007_00B7u32 as i32) {
                return Err(format!("CreateAppContainerProfile: {e}"));
            }
        }
        // create(新建或已存在复用)后 derive 取权威 SID(单一代码路径)
        match derive_sid(&name_wide) {
            Ok(sid) => Ok(Self {
                name_wide,
                name: name.to_string(),
                sid,
            }),
            Err(derive_err) => Err(format!(
                "CreateAppContainerProfile({name}) failed: {create_result:?}; derive fallback: {derive_err}"
            )),
        }
    }
}

impl Drop for LowboxProfile {
    fn drop(&mut self) {
        // SAFETY:name_wide 以 NUL 结尾;删除失败仅留痕(profile 残留无安全
        // 影响——SID 同派生,下次执行 derive 即复用;不 panic 防止吞掉主流程错误)
        unsafe {
            if let Err(e) = DeleteAppContainerProfile(PCWSTR::from_raw(self.name_wide.as_ptr())) {
                tracing::warn!(
                    "DeleteAppContainerProfile failed: {e} (profile '{}')",
                    self.name
                );
            }
        }
    }
}

// ─────────────────── ACL 授权(design-36 §6.1 ②) ───────────────────

/// SID 相等(两个 SID 都是 Win32 API 产物或其字节拷贝,布局有效)
fn sid_eq(a: PSID, b: &SidBuf) -> bool {
    // SAFETY:两侧均为有效 SID 指针
    unsafe { EqualSid(a, b.as_psid()).is_ok() }
}

/// 遍历 DACL 查匹配的 ALLOWED ACE(mask 覆盖 rights 且 SID 谓词命中)
///
/// SAFETY:`dacl` 必须来自成功的 GetNamedSecurityInfoW 且仍有效。
/// GetAclInformation 失败按 0 条 ACE 处理(保守:调用方落到写 DACL 路径)。
unsafe fn dacl_has_allow(dacl: *const ACL, rights: u32, is_match: &dyn Fn(PSID) -> bool) -> bool {
    let mut info = ACL_SIZE_INFORMATION::default();
    let _ = GetAclInformation(
        dacl,
        &mut info as *mut _ as *mut c_void,
        std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
        AclSizeInformation,
    );
    for i in 0..info.AceCount {
        let mut ace: *mut c_void = std::ptr::null_mut();
        if GetAce(dacl, i, &mut ace).is_err() || ace.is_null() {
            continue;
        }
        let header = &*(ace as *const ACE_HEADER);
        if header.AceType != 0 {
            // ACCESS_ALLOWED_ACE_TYPE == 0(windows crate 未出此常量)
            continue;
        }
        let allowed = &*(ace as *const ACCESS_ALLOWED_ACE);
        if (allowed.Mask & rights) != rights {
            continue;
        }
        // SidStart 是内联 SID 首 DWORD,取其地址即 PSID
        let ace_sid = PSID(&allowed.SidStart as *const u32 as *mut c_void);
        if is_match(ace_sid) {
            return true;
        }
    }
    false
}

/// ALL APPLICATION PACKAGES(S-1-15-2-1)SID(Win32 已知 SID 产物)
fn all_app_packages_sid() -> Result<SidBuf, String> {
    let mut buf = vec![0u8; 68]; // SID 最大长度 68
    let mut len = buf.len() as u32;
    // SAFETY:缓冲与长度指针有效;返回 SidBuf 承接所有权
    unsafe {
        CreateWellKnownSid(
            WinBuiltinAnyPackageSid,
            None,
            Some(PSID(buf.as_mut_ptr() as *mut c_void)),
            &mut len,
        )
        .map_err(|e| format!("CreateWellKnownSid(ALL APPLICATION PACKAGES): {e}"))?;
    }
    buf.truncate(len as usize);
    Ok(SidBuf { bytes: buf })
}

/// 目录是否已带 ALL APPLICATION PACKAGES 的 ALLOWED ACE(覆盖 rights)
///
/// 系统目录(VS/SDK 安装根)缺省自带该继承 ACE(实测 icacls 形态:
/// `(I)(RX)` + `(I)(OI)(CI)(IO)(GR,GE)`),低盒无需个体授权即可读执行;
/// 用于管理员属主目录授权被拒时的回退判定。
fn system_default_app_rx(dir: &Path, rights: u32) -> bool {
    let dir_wide = to_wide_null(&dir.as_os_str().to_string_lossy());
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
    let got = {
        // SAFETY:dir_wide NUL 结尾;全部出参指针有效
        unsafe {
            GetNamedSecurityInfoW(
                PCWSTR::from_raw(dir_wide.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(&mut dacl),
                None,
                &mut sd,
            )
        }
    };
    if got != WIN32_ERROR(0) {
        return false;
    }
    let _sd_guard = LocalFreeGuard(HLOCAL(sd.0));
    let app_sid = match all_app_packages_sid() {
        Ok(s) => s,
        Err(_) => return false,
    };
    // SAFETY:dacl 来自上方成功的 GetNamedSecurityInfoW,守卫仍存活
    unsafe { dacl_has_allow(dacl, rights, &|p| sid_eq(p, &app_sid)) }
}

/// 目录 DACL 追加 ACE(SID trustee;(OI)(CI) 继承传播)
///
/// 幂等:本 SID 的 ALLOWED ACE 已覆盖 rights 时直接返回(不重写 DACL——
/// 管理员属主目录在一次性提权 icacls 授权后,后续运行不再需要 WRITE_DAC);
/// 缺失才追加。管理员属主目录(未提升进程改 DACL)报 win32=5 → fail-visible
/// 带提权 icacls 提示(§6.1 ② 实证形态);工具链根调用方另有无授权回退。
fn grant_dir_access(dir: &Path, sid: &SidBuf, rights: u32) -> Result<(), String> {
    let dir_wide = to_wide_null(&dir.as_os_str().to_string_lossy());
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
    let got = {
        // SAFETY:dir_wide NUL 结尾;全部出参指针有效
        unsafe {
            GetNamedSecurityInfoW(
                PCWSTR::from_raw(dir_wide.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(&mut dacl),
                None,
                &mut sd,
            )
        }
    };
    if got != WIN32_ERROR(0) {
        return Err(format!("GetNamedSecurityInfoW({}): {got:?}", dir.display()));
    }
    let _sd_guard = LocalFreeGuard(HLOCAL(sd.0));

    // 幂等快路径:ACE 已在(如管理员属主目录一次性提权授权后)不再写
    let sid_for_check = sid.clone();
    // SAFETY:dacl 来自上方成功的 GetNamedSecurityInfoW,守卫仍存活
    let already = unsafe { dacl_has_allow(dacl, rights, &|p| sid_eq(p, &sid_for_check)) };
    if already {
        return Ok(());
    }

    let ea = EXPLICIT_ACCESS_W {
        grfAccessPermissions: rights,
        grfAccessMode: SET_ACCESS,
        grfInheritance: OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            // TRUSTEE_IS_SID 形态下 ptstrName 实际承载 SID 指针
            ptstrName: PWSTR(sid.bytes.as_ptr() as *mut u16),
        },
    };
    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    let set_result = {
        // SAFETY:ea/dacl/new_dacl 指针有效;dacl 来自上一 GetNamedSecurityInfoW
        unsafe { SetEntriesInAclW(Some(&[ea]), Some(dacl), &mut new_dacl) }
    };
    if set_result != WIN32_ERROR(0) {
        return Err(format!(
            "SetEntriesInAclW({}): {set_result:?}",
            dir.display()
        ));
    }

    let applied = {
        // SAFETY:dir_wide/new_dacl 有效
        unsafe {
            SetNamedSecurityInfoW(
                PCWSTR::from_raw(dir_wide.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(new_dacl),
                None,
            )
        }
    };
    // SAFETY:SetEntriesInAclW 产物为 LocalAlloc 语义
    unsafe {
        LocalFree(Some(HLOCAL(new_dacl as *mut c_void)));
    }
    if applied != WIN32_ERROR(0) {
        // 管理员属主目录(未提升进程改 DACL):win32=5 → fail-visible 带提权
        // icacls 一次性授权提示(§6.1 ② 实证形态;全权=F,只读执行=RX)
        if applied == WIN32_ERROR(5) {
            let flag = if rights == GENERIC_ALL.0 {
                "(OI)(CI)F"
            } else {
                "(OI)(CI)RX"
            };
            return Err(format!(
                "lowbox ACL grant failed on '{}' (access denied; likely an \
                 administrator-owned directory). Grant once from an elevated shell: \
                 icacls \"{}\" /grant *{}:{flag}  — see design-36 §6.1",
                dir.display(),
                dir.display(),
                sid_string(sid)
            ));
        }
        return Err(format!(
            "SetNamedSecurityInfoW({}): {applied:?}",
            dir.display()
        ));
    }
    Ok(())
}

/// 工具链根 RX 授权(进程级一次)
///
/// 幂等(ACE 已在则跳过)之外带系统缺省回退:VS/SDK 安装根是管理员属主
/// (未提升进程改 DACL 必败),但系统目录缺省自带 ALL APPLICATION PACKAGES
/// RX 继承 ACE(低盒读执行天然可用),授权被拒时实测确认后 warn 继续仅记
/// 账;无回退面(如异常目录布局)才 fail-visible。失败结果同样缓存(工具链
/// 布局进程内不变,重试同败)。
fn grant_toolchain_rx(sid: &SidBuf, msvc_env: &[(String, String)]) -> Result<(), String> {
    static GRANTED: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    GRANTED
        .get_or_init(|| {
            let rx = GENERIC_READ.0 | GENERIC_EXECUTE.0;
            let env_get = |name: &str| {
                msvc_env
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(name))
                    .map(|(_, v)| PathBuf::from(v))
            };
            let mut roots: Vec<PathBuf> = Vec::new();
            // VS 安装根(VCINSTALLDIR = <install>\VC\ → install)
            if let Some(vc) = env_get("VCINSTALLDIR") {
                if let Some(install) = vc.parent() {
                    roots.push(install.to_path_buf());
                }
            }
            // SDK 根(WindowsSdkDir = <kits>\10\)
            if let Some(sdk) = env_get("WindowsSdkDir") {
                roots.push(sdk);
            }
            // rust 工具链面(shim 与真身两处;存在才授;用户属主,未提权可授)
            if let Some(home) = std::env::var_os("USERPROFILE").map(PathBuf::from) {
                roots.push(home.join(".cargo"));
                roots.push(home.join(".rustup"));
            }
            for root in &roots {
                if !root.is_dir() {
                    continue;
                }
                match grant_dir_access(root, sid, rx) {
                    Ok(()) => {}
                    Err(_) if system_default_app_rx(root, rx) => {
                        tracing::warn!(
                            "lowbox toolchain RX grant denied on '{}' but \
                             ALL APPLICATION PACKAGES RX present (system default) — \
                             proceeding without individual grant",
                            root.display()
                        );
                    }
                    Err(e) => {
                        return Err(format!(
                            "lowbox toolchain RX grant failed on '{}': {e}",
                            root.display()
                        ));
                    }
                }
            }
            Ok(())
        })
        .clone()
}

// ─────────────────── VS/SDK 探测(cargo 助推,design-36 §6.2) ───────────────────

/// 版本号比较(目录名点分段数值序;段缺失按 0)
fn cmp_version(a: &str, b: &str) -> Ordering {
    let seg = |s: &str| -> Vec<u64> {
        s.split('.')
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (av, bv) = (seg(a), seg(b));
    for i in 0..av.len().max(bv.len()) {
        let x = av.get(i).copied().unwrap_or(0);
        let y = bv.get(i).copied().unwrap_or(0);
        match x.cmp(&y) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    Ordering::Equal
}

/// 8.3 短路径(design-36 §6.2 栅栏 2:RUSTFLAGS 带空格路径被 cargo 分词
/// 切断——GetShortPathNameW 消除空格;卷禁用 8.3 生成时结果仍带空格,
/// fail-visible 拒绝而非静默产出必败配置)
fn short_path(p: &Path) -> Result<PathBuf, String> {
    let long_wide = to_wide_null(&p.as_os_str().to_string_lossy());
    // SAFETY:long_wide NUL 结尾;出参切片生命周期覆盖调用
    let len = unsafe { GetShortPathNameW(PCWSTR::from_raw(long_wide.as_ptr()), None) };
    if len == 0 {
        return Err(format!(
            "GetShortPathNameW failed for '{}'(路径不存在或短名解析失败)",
            p.display()
        ));
    }
    let mut buf = vec![0u16; len as usize];
    // SAFETY:buf 容量即上查长度
    let n = unsafe { GetShortPathNameW(PCWSTR::from_raw(long_wide.as_ptr()), Some(&mut buf)) };
    if n == 0 || n as usize > buf.len() {
        return Err(format!("GetShortPathNameW failed for '{}'", p.display()));
    }
    buf.truncate(n as usize);
    let s = String::from_utf16_lossy(&buf);
    if s.contains(' ') {
        return Err(format!(
            "short path for '{}' still contains spaces (volume has 8.3 name \
             generation disabled; RUSTFLAGS linker would be split by cargo)",
            p.display()
        ));
    }
    Ok(PathBuf::from(s))
}

/// MSVC 环境面探测(vsdevcmd 式变量;§6.2 三栅栏)
///
/// - VS:`<vs_root>\<year>\<edition>\VC\Tools\MSVC\<ver>`(取最高版本;
///   校验 bin\Hostx64\x64\link.exe 与 lib\x64 在位);
/// - SDK:`<sdk_root>\10\Lib\<ver>`(取最高;校验 um\x64 与 ucrt\x64);
/// - LIB 三段齐全(§6.2 栅栏 3:MSVC lib\x64 + SDK um\x64 + SDK ucrt\x64,
///   缺则 LNK1181);
/// - RUSTFLAGS 显式 linker(8.3 短路径;§6.2 栅栏 1/2:rustc 低盒内自动
///   探测失败 → 显式绕过)。
///
/// 探测根由调用方注入(测试友好;生产走 [`default_vs_roots`]/[`default_sdk_roots`])。
pub(crate) fn detect_msvc_env(
    vs_roots: &[PathBuf],
    sdk_roots: &[PathBuf],
) -> Result<Vec<(String, String)>, String> {
    // VS 侧:取最高版本 MSVC 工具集
    let mut best_msvc: Option<(
        String,
        PathBuf, /* vs_install */
        PathBuf, /* msvc dir */
    )> = None;
    for root in vs_roots {
        let Ok(years) = std::fs::read_dir(root) else {
            continue;
        };
        for year in years.flatten() {
            let Ok(editions) = std::fs::read_dir(year.path()) else {
                continue;
            };
            for edition in editions.flatten() {
                let vs_install = edition.path();
                let msvc_root = vs_install.join(r"VC\Tools\MSVC");
                let Ok(vers) = std::fs::read_dir(&msvc_root) else {
                    continue;
                };
                for ver in vers.flatten() {
                    let vdir = ver.path();
                    if !vdir.join(r"bin\Hostx64\x64\link.exe").is_file() {
                        continue;
                    }
                    if !vdir.join(r"lib\x64").is_dir() {
                        continue;
                    }
                    let name = ver.file_name().to_string_lossy().to_string();
                    let better = match &best_msvc {
                        None => true,
                        Some((n, _, _)) => cmp_version(&name, n) == Ordering::Greater,
                    };
                    if better {
                        best_msvc = Some((name, vs_install.clone(), vdir));
                    }
                }
            }
        }
    }
    let Some((_msvc_ver, vs_install, msvc_dir)) = best_msvc else {
        return Err(
            "MSVC toolchain not detected (VS BuildTools with C++ workload required \
             for the lowbox cargo chain; see design-36 §6.2)"
                .to_string(),
        );
    };

    // SDK 侧:<sdk_root>\10\Lib\<ver>(取最高;um/ucrt 两段必须齐)
    let mut best_sdk: Option<(String, PathBuf)> = None;
    for root in sdk_roots {
        let sdk10 = root.join("10");
        let Ok(vers) = std::fs::read_dir(sdk10.join("Lib")) else {
            continue;
        };
        for ver in vers.flatten() {
            let vdir = ver.path();
            if !vdir.join(r"um\x64").is_dir() || !vdir.join(r"ucrt\x64").is_dir() {
                continue;
            }
            let name = ver.file_name().to_string_lossy().to_string();
            let better = match &best_sdk {
                None => true,
                Some((n, _)) => cmp_version(&name, n) == Ordering::Greater,
            };
            if better {
                best_sdk = Some((name, sdk10.clone()));
            }
        }
    }
    let Some((sdk_ver, sdk_dir)) = best_sdk else {
        return Err(
            "Windows SDK not detected (Lib\\<ver>\\um\\x64 + ucrt\\x64 required; \
             see design-36 §6.2)"
                .to_string(),
        );
    };

    // LIB 三段(§6.2 栅栏 3)
    let lib = format!(
        "{};{};{}",
        msvc_dir.join(r"lib\x64").display(),
        sdk_dir.join(format!(r"Lib\{sdk_ver}\um\x64")).display(),
        sdk_dir.join(format!(r"Lib\{sdk_ver}\ucrt\x64")).display(),
    );

    // RUSTFLAGS 显式 linker(8.3 短路径)
    let link_exe = msvc_dir.join(r"bin\Hostx64\x64\link.exe");
    let linker = short_path(&link_exe)?;
    let rustflags = format!("-C linker={}", linker.display());

    Ok(vec![
        (
            "VCINSTALLDIR".to_string(),
            format!("{}\\", vs_install.join("VC").display()),
        ),
        (
            "VCToolsInstallDir".to_string(),
            format!("{}\\", msvc_dir.display()),
        ),
        (
            "WindowsSdkDir".to_string(),
            format!("{}\\", sdk_dir.display()),
        ),
        ("WindowsSdkVersion".to_string(), format!("{sdk_ver}\\")),
        ("LIB".to_string(), lib),
        ("RUSTFLAGS".to_string(), rustflags),
    ])
}

/// 宿主 VS 探测根(生产默认)
fn default_vs_roots() -> Vec<PathBuf> {
    [
        r"C:\Program Files (x86)\Microsoft Visual Studio",
        r"C:\Program Files\Microsoft Visual Studio",
    ]
    .iter()
    .map(PathBuf::from)
    .collect()
}

/// 宿主 SDK 探测根(生产默认)
fn default_sdk_roots() -> Vec<PathBuf> {
    [
        r"C:\Program Files (x86)\Windows Kits",
        r"C:\Program Files\Windows Kits",
    ]
    .iter()
    .map(PathBuf::from)
    .collect()
}

/// MSVC 环境面进程级缓存(探测失败同样落账:工具链布局进程内不变)
fn msvc_env_cached() -> Result<Vec<(String, String)>, String> {
    static CACHE: std::sync::OnceLock<Result<Vec<(String, String)>, String>> =
        std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| detect_msvc_env(&default_vs_roots(), &default_sdk_roots()))
        .clone()
}

// ─────────────────── 程序定位 ───────────────────

/// 宿主 PATH 定位程序绝对路径(低盒 CreateProcessW 的 lpApplicationName
/// 不做 PATH 搜索,必须显式绝对路径;只搜 PATH 不含 cwd——比宿主 CreateProcess
/// 搜索序更强,workdir 植文件不入候选;workdir 遮蔽检查仍由调用方先行,
/// 与宿主面同防护语义)
pub(crate) fn locate_program_in_path(program: &str) -> Result<PathBuf, String> {
    let exe = format!("{program}.exe");
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(&exe);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "'{program}' not found in PATH (lowbox launch needs the absolute image path)"
    ))
}

// ─────────────────── 低盒拉起(PROC_THREAD_ATTRIBUTE 面) ───────────────────

/// Windows 命令行参数引号包裹(CRT 分词规则:空格/tab 包引号;内部引号前
/// 补反斜杠;引号前原有反斜杠翻倍;闭合引号前尾部反斜杠翻倍)
fn quote_arg(s: &str) -> String {
    let need_quote = s.is_empty() || s.chars().any(|c| matches!(c, ' ' | '\t' | '"'));
    if !need_quote {
        return s.to_string();
    }
    let mut out = String::from("\"");
    let mut slashes = 0usize;
    for c in s.chars() {
        match c {
            '\\' => {
                slashes += 1;
                out.push('\\');
            }
            '"' => {
                for _ in 0..slashes + 1 {
                    out.push('\\');
                }
                slashes = 0;
                out.push('"');
            }
            _ => {
                slashes = 0;
                out.push(c);
            }
        }
    }
    for _ in 0..slashes {
        out.push('\\');
    }
    out.push('"');
    out
}

/// 命令行 wide 构造(NUL 结尾;argv[0] 与 lpApplicationName 同绝对路径)
fn build_command_line(program: &Path, args: &[String]) -> Vec<u16> {
    let mut line = quote_arg(&program.display().to_string());
    for a in args {
        line.push(' ');
        line.push_str(&quote_arg(a));
    }
    to_wide_null(&line)
}

/// 管道创建(inherit 标志由调用方按端处理)
fn make_pipe() -> Result<(HANDLE, HANDLE), String> {
    let mut r = HANDLE::default();
    let mut w = HANDLE::default();
    // SAFETY:出参指针有效;安全属性 None(默认 DACL)
    unsafe {
        CreatePipe(&mut r, &mut w, None, 0).map_err(|e| format!("CreatePipe: {e}"))?;
    }
    Ok((r, w))
}

/// 排空句柄到缓冲(读至 EOF/断管;与宿主路径 drain_pipe 同语义——不排空
/// 会撑满管道死锁)。管道 EOF = 写端全灭 = 低盒进程族全退出的强信号。
///
/// SAFETY:调用方保证 h 为可读句柄且本线程独占读取。
unsafe fn drain_handle(h: HANDLE) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 16384];
    loop {
        let mut n = 0u32;
        let ok = ReadFile(h, Some(&mut buf), Some(&mut n), None).is_ok();
        if !ok || n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
    out
}

/// 低盒执行全流程(profile ensure → ACL 授权 → 环境块 → 拉起 → 有界等待)
///
/// profile 用后即删(Drop;§6.5);执行失败同样走 Drop(残留 profile 下次
/// derive 复用,无安全影响)。
pub fn run_lowbox(spec: &LowboxSpec) -> Result<LowboxOutcome, String> {
    let profile = LowboxProfile::ensure(LOWBOX_PROFILE_NAME)?;
    // MSVC 环境面(cargo 链三栅栏;§6.2;进程级缓存)
    let msvc_env = msvc_env_cached()?;
    // ACL 授权:workdir GENERIC_ALL 每次自愈(不同 workdir 不同 ACE);工具链
    // RX 进程级一次(OnceLock;§6.1 ②)
    grant_dir_access(&spec.workdir, &profile.sid, GENERIC_ALL.0)
        .map_err(|e| format!("lowbox workdir grant: {e}"))?;
    grant_toolchain_rx(&profile.sid, &msvc_env)?;
    let env_block = build_environment_block(&parent_env(), &msvc_env)?;

    // SAFETY:spec/profile 状态已备;句柄与 attribute 生命周期在函数内闭环
    unsafe { spawn_lowbox(spec, &profile.sid, &env_block) }
}

/// attribute list 删除守卫(Drop = DeleteProcThreadAttributeList)
struct AttrListGuard(LPPROC_THREAD_ATTRIBUTE_LIST);

impl Drop for AttrListGuard {
    fn drop(&mut self) {
        // SAFETY:list 由 InitializeProcThreadAttributeList 初始化且未删
        unsafe {
            DeleteProcThreadAttributeList(self.0);
        }
    }
}

/// 低盒进程拉起 + 有界等待编排
///
/// SAFETY(整函数 FFI 面):
/// - 全部 wide 串 NUL 结尾且生命周期覆盖 CreateProcessW 调用;
/// - attribute list/sc/sid 活到 CreateProcessW 返回后才释放;
/// - 管道端读写权单线程独占;句柄经 HandleBox RAII 回收。
unsafe fn spawn_lowbox(
    spec: &LowboxSpec,
    sid: &SidBuf,
    env_block: &[u16],
) -> Result<LowboxOutcome, String> {
    // 1. 三根管道:stdout/stderr(子写父读)+ stdin-EOF(父立即关写端,子读即 EOF)
    let (out_r, out_w) = make_pipe()?;
    let (err_r, err_w) = make_pipe()?;
    let (in_r, in_w) = make_pipe()?;
    let out_w = HandleBox(out_w);
    let err_w = HandleBox(err_w);
    let in_r = HandleBox(in_r);
    let in_w = HandleBox(in_w);
    // 继承位:子进程只需要 out_w/err_w/in_r 三个端(dwmask/dwflags 同值 = 置位)
    for h in [out_w.0, err_w.0, in_r.0] {
        // SAFETY:合法句柄
        unsafe {
            SetHandleInformation(h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT)
                .map_err(|e| format!("SetHandleInformation(inherit): {e}"))?;
        }
    }
    // 读端显式不可继承(防子进程族持有读端致父侧 drain 永不 EOF)
    for h in [out_r, err_r] {
        // SAFETY:合法句柄;mask 命中继承位、flags=0 = 清位
        unsafe {
            SetHandleInformation(h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0))
                .map_err(|e| format!("SetHandleInformation(no-inherit): {e}"))?;
        }
    }

    // 2. attribute list(size 查询 → 分配 → 初始化 → 两 attribute;个数必须
    //    = 实际 Update 次数,少一个则第二次 Update 报 0x8007001F)
    let mut size = 0usize;
    let _ = InitializeProcThreadAttributeList(None, 2, Some(0), &mut size); // 期望 Err+size 填充
    if size == 0 {
        return Err("InitializeProcThreadAttributeList size probe failed".to_string());
    }
    let mut attr_buf = vec![0u8; size];
    let list = LPPROC_THREAD_ATTRIBUTE_LIST(attr_buf.as_mut_ptr() as *mut c_void);
    // SAFETY:缓冲按上查 size 分配
    unsafe {
        InitializeProcThreadAttributeList(Some(list), 2, Some(0), &mut size)
            .map_err(|e| format!("InitializeProcThreadAttributeList: {e}"))?;
    }
    let _attr_guard = AttrListGuard(list);
    let mut sc = SECURITY_CAPABILITIES {
        AppContainerSid: sid.as_psid(),
        Capabilities: std::ptr::null_mut(), // capabilities=None = 缺省禁网
        CapabilityCount: 0,
        Reserved: 0,
    };
    // SAFETY:list 已初始化;sc 生命周期覆盖 CreateProcessW
    unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
            Some(&mut sc as *mut SECURITY_CAPABILITIES as *const c_void),
            std::mem::size_of::<SECURITY_CAPABILITIES>(),
            None,
            None,
        )
        .map_err(|e| format!("UpdateProcThreadAttribute(security): {e}"))?;
    }
    // 显式句柄继承清单(低盒最小授权:仅三根管道端可继承)
    let handle_list: [HANDLE; 3] = [out_w.0, err_w.0, in_r.0];
    // SAFETY:同上;handle_list 生命周期覆盖 CreateProcessW
    unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            Some(handle_list.as_ptr() as *const c_void),
            std::mem::size_of::<[HANDLE; 3]>(),
            None,
            None,
        )
        .map_err(|e| format!("UpdateProcThreadAttribute(handle-list): {e}"))?;
    }

    // 3. STARTUPINFOEXW + 进程参数
    let mut si: STARTUPINFOEXW = std::mem::zeroed();
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = in_r.0;
    si.StartupInfo.hStdOutput = out_w.0;
    si.StartupInfo.hStdError = err_w.0;
    si.lpAttributeList = list;

    let appname = to_wide_null(&spec.program.display().to_string());
    let mut cmdline = build_command_line(&spec.program, &spec.args);
    let cwd = to_wide_null(&spec.workdir.display().to_string());
    let mut pi: PROCESS_INFORMATION = std::mem::zeroed();

    // SAFETY:全部参数就绪;si 以 STARTUPINFOEXW 形态传入(cb 已标 EXTENDED)
    unsafe {
        CreateProcessW(
            PCWSTR::from_raw(appname.as_ptr()),
            Some(PWSTR::from_raw(cmdline.as_mut_ptr())),
            None,
            None,
            true,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            Some(env_block.as_ptr() as *const c_void),
            PCWSTR::from_raw(cwd.as_ptr()),
            &si.StartupInfo,
            &mut pi,
        )
        .map_err(|e| {
            format!(
                "CreateProcessW(appcontainer) failed: {e} (note: lowbox image access \
                 failure surfaces as ERROR_FILE_NOT_FOUND, not access denied — \
                 design-36 §6.6)"
            )
        })?;
    }
    let pid = pi.dwProcessId;
    let proc_handle = HandleBox(pi.hProcess);
    // SAFETY:主线程句柄即刻回收
    unsafe {
        let _ = CloseHandle(pi.hThread);
    }

    // 4. 父侧继承端全关(子进程族持唯一写端;此后 drain 才有 EOF 语义)
    drop(out_w);
    drop(err_w);
    drop(in_r);
    drop(in_w);

    // 5. drain 线程 + 有界轮询等待(读端先包 Send 壳再入闭包;方法访问
    // 防止 disjoint capture 把非 Send 的 HANDLE 字段单捕进闭包环境)
    let out_r = SendHandle(out_r);
    let err_r = SendHandle(err_r);
    let out_reader = std::thread::spawn(move || {
        // SAFETY:out_r 独占读取,父侧已无其它读者
        unsafe { drain_handle(out_r.get()) }
    });
    let err_reader = std::thread::spawn(move || {
        // SAFETY:err_r 独占读取
        unsafe { drain_handle(err_r.get()) }
    });

    let deadline = Instant::now() + spec.timeout;
    let mut timed_out = false;
    loop {
        let evt = WaitForSingleObject(proc_handle.0, POLL_INTERVAL_MS);
        if evt == WAIT_OBJECT_0 {
            break;
        }
        if evt == WAIT_TIMEOUT && Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        if evt != WAIT_TIMEOUT {
            return Err(format!("WaitForSingleObject: WAIT_EVENT({:?})", evt.0));
        }
    }

    let mut exit_code: u32 = 0;
    // SAFETY:主进程句柄有效
    unsafe {
        GetExitCodeProcess(proc_handle.0, &mut exit_code)
            .map_err(|e| format!("GetExitCodeProcess: {e}"))?;
    }
    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();

    if timed_out {
        // 低盒进程族终止:taskkill /T /F 树杀(与宿主路径同工具;低盒内孙进程
        // 由树杀+低盒边界覆盖)。两相核验面不在本批——verified 保守入账
        // false(不确定性显式化,不假宣称已净)
        crate::builtin_tools::shell_exec::terminate_family_by_pid(pid);
        let _ = WaitForSingleObject(proc_handle.0, TERMINATE_GRACE_MS);
        // SAFETY:主进程句柄有效;失败则保留击杀前快照码
        unsafe {
            let mut late_code: u32 = 0;
            let _ = GetExitCodeProcess(proc_handle.0, &mut late_code);
            if late_code != 0 {
                exit_code = late_code;
            }
        }
        return Ok(LowboxOutcome {
            pid,
            timed_out: true,
            output: std::process::Output {
                status: std::os::windows::process::ExitStatusExt::from_raw(exit_code),
                stdout,
                stderr,
            },
        });
    }

    Ok(LowboxOutcome {
        pid,
        timed_out: false,
        output: std::process::Output {
            status: std::os::windows::process::ExitStatusExt::from_raw(exit_code),
            stdout,
            stderr,
        },
    })
}

// ─────────────────────────── 测试 ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // === 环境块白名单(design-36 §6.3) ===

    #[test]
    fn env_block_filters_secrets() {
        let parent = vec![
            ("PATH".to_string(), r"C:\bin".to_string()),
            ("LOCALAPPDATA".to_string(), r"C:\L".to_string()),
            (
                "EVO_AGENT_LLM__API_KEY".to_string(),
                "sk-secret".to_string(),
            ),
            ("MINIMAX_API_KEY".to_string(), "sk-2".to_string()),
            ("GITHUB_TOKEN".to_string(), "ghp-x".to_string()),
        ];
        let block = build_environment_block(&parent, &[]).unwrap();
        let text = String::from_utf16_lossy(&block);
        assert!(
            !text.contains("sk-secret"),
            "密钥面绝不入低盒环境块: {text}"
        );
        assert!(!text.contains("sk-2"));
        assert!(!text.contains("ghp-x"));
        assert!(text.contains("PATH=C:\\bin"));
    }

    #[test]
    fn env_block_case_insensitive_whitelist() {
        // §6.3 坑1:真实环境名混合大小写,精确匹配会滤掉 PATH
        let parent = vec![
            ("Path".to_string(), r"C:\bin".to_string()),
            (
                "LocalAppData".to_string(),
                r"C:\Users\u\AppData\Local".to_string(),
            ),
        ];
        let block = build_environment_block(&parent, &[]).unwrap();
        let text = String::from_utf16_lossy(&block);
        assert!(text.contains("Path=C:\\bin"), "大小写不敏感匹配: {text}");
        assert!(text.contains("LocalAppData="));
    }

    #[test]
    fn env_block_requires_localappdata() {
        // §6.3 坑2:LOCALAPPDATA 缺失 → CreateProcessW 0x800700CB
        let parent = vec![("PATH".to_string(), r"C:\bin".to_string())];
        let err = build_environment_block(&parent, &[]).unwrap_err();
        assert!(err.contains("LOCALAPPDATA"), "{err}");
        // extra 提供亦可
        let extra = vec![(
            "LOCALAPPDATA".to_string(),
            r"C:\u\AppData\Local".to_string(),
        )];
        assert!(build_environment_block(&parent, &extra).is_ok());
    }

    #[test]
    fn env_block_excludes_special_items() {
        let parent = vec![
            ("=C:".to_string(), "C:\\".to_string()),
            ("__COMPAT_LAYER".to_string(), "RunAsInvoker".to_string()),
            ("SOME_SECRET".to_string(), "x".to_string()),
            ("LOCALAPPDATA".to_string(), r"C:\L".to_string()),
        ];
        let block = build_environment_block(&parent, &[]).unwrap();
        let text = String::from_utf16_lossy(&block);
        assert!(!text.contains("=C:="));
        assert!(!text.contains("__COMPAT_LAYER"));
        assert!(!text.contains("SOME_SECRET"));
    }

    #[test]
    fn env_block_extra_overrides_parent() {
        let parent = vec![
            ("RUSTFLAGS".to_string(), "-C target-cpu=native".to_string()),
            ("LOCALAPPDATA".to_string(), r"C:\L".to_string()),
        ];
        let extra = vec![("RUSTFLAGS".to_string(), "-C linker=C:/LINK.EXE".to_string())];
        let block = build_environment_block(&parent, &extra).unwrap();
        let text = String::from_utf16_lossy(&block);
        assert!(
            text.contains("RUSTFLAGS=-C linker=C:/LINK.EXE"),
            "extra 覆盖父环境: {text}"
        );
        assert!(!text.contains("target-cpu"));
    }

    #[test]
    fn env_block_double_nul_terminated_utf16() {
        let parent = vec![("LOCALAPPDATA".to_string(), "L".to_string())];
        let block = build_environment_block(&parent, &[]).unwrap();
        assert_eq!(block.last(), Some(&0), "块尾双 NUL");
        assert_eq!(
            &block[block.len() - 2..block.len() - 1],
            &[0],
            "末项自身 NUL 结尾"
        );
        // 单项形态:NAME=value\0\0
        assert!(block.starts_with(&[b'L' as u16, b'O' as u16, b'C' as u16]));
    }

    // === 版本比较 ===

    #[test]
    fn cmp_version_numeric_semantics() {
        assert_eq!(cmp_version("14.36.32532", "14.35.1"), Ordering::Greater);
        assert_eq!(
            cmp_version("10.0.22621.0", "10.0.22000.0"),
            Ordering::Greater
        );
        assert_eq!(cmp_version("14.29", "14.29.0"), Ordering::Equal);
        assert_eq!(cmp_version("9", "10"), Ordering::Less);
        assert_eq!(cmp_version("14.36", "14.36.1"), Ordering::Less);
    }

    // === Windows 命令行引号(CRT 分词规则) ===

    #[test]
    fn quote_arg_basic() {
        assert_eq!(quote_arg("build"), "build");
        assert_eq!(quote_arg(""), "\"\"");
        assert_eq!(quote_arg("a b"), "\"a b\"");
        assert_eq!(quote_arg("say \"hi\""), "\"say \\\"hi\\\"\"");
        // 尾部反斜杠本身不触发引号(与 std 同语义:无空格/引号不包引号,
        // 翻倍规则只在引号体内有意义);含空格的尾部反斜杠才走翻倍路径
        assert_eq!(quote_arg("trail\\"), "trail\\");
        assert_eq!(quote_arg(r"a b\"), "\"a b\\\\\"");
        assert_eq!(quote_arg("mid\\\"x"), "\"mid\\\\\\\"x\"");
    }

    // === MSVC 探测(临时树注入) ===

    #[test]
    fn detect_msvc_env_probes_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let vs_root = tmp.path().join("VS");
        let sdk_root = tmp.path().join("Kits");
        // VS 树:2022/BuildTools/VC/Tools/MSVC/14.36.32532
        let msvc = vs_root.join(r"2022\BuildTools\VC\Tools\MSVC\14.36.32532");
        std::fs::create_dir_all(msvc.join(r"bin\Hostx64\x64")).unwrap();
        std::fs::create_dir_all(msvc.join(r"lib\x64")).unwrap();
        std::fs::write(msvc.join(r"bin\Hostx64\x64\link.exe"), b"").unwrap();
        // 次版本树应被「取最高」压过
        let msvc_old = vs_root.join(r"2022\BuildTools\VC\Tools\MSVC\14.29.30133");
        std::fs::create_dir_all(msvc_old.join(r"bin\Hostx64\x64")).unwrap();
        std::fs::create_dir_all(msvc_old.join(r"lib\x64")).unwrap();
        std::fs::write(msvc_old.join(r"bin\Hostx64\x64\link.exe"), b"").unwrap();
        // SDK 树:10/Lib/10.0.22621.0
        let sdk_lib = sdk_root.join(r"10\Lib\10.0.22621.0");
        std::fs::create_dir_all(sdk_lib.join(r"um\x64")).unwrap();
        std::fs::create_dir_all(sdk_lib.join(r"ucrt\x64")).unwrap();

        let env = detect_msvc_env(&[vs_root], &[sdk_root]).unwrap();
        let get = |k: &str| {
            env.iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert!(get("VCINSTALLDIR").ends_with(r"2022\BuildTools\VC\"));
        assert!(get("VCToolsInstallDir").contains("14.36.32532"));
        assert!(get("WindowsSdkDir").ends_with("10\\"));
        assert_eq!(get("WindowsSdkVersion"), "10.0.22621.0\\");
        let lib = get("LIB");
        assert!(lib.contains("14.36.32532\\lib\\x64"), "{lib}");
        assert!(lib.contains("10.0.22621.0\\um\\x64"), "{lib}");
        assert!(lib.contains("10.0.22621.0\\ucrt\\x64"), "{lib}");
        assert!(get("RUSTFLAGS").starts_with("-C linker="));
    }

    #[test]
    fn detect_msvc_env_missing_toolchain_fails_visible() {
        let tmp = tempfile::tempdir().unwrap();
        let err =
            detect_msvc_env(&[tmp.path().join("nope")], &[tmp.path().join("nope")]).unwrap_err();
        assert!(err.contains("MSVC"), "{err}");
    }

    // === 程序定位 ===

    #[test]
    fn locate_program_uses_path_ex() {
        // System32 必有:cmd.exe(纯定位面,不执行)
        let found = locate_program_in_path("cmd").unwrap();
        assert!(found.is_file());
        assert!(found.extension().is_some_and(|e| e == "exe"));
        let missing = locate_program_in_path("definitely-not-a-real-tool-xyz").unwrap_err();
        assert!(missing.contains("not found in PATH"));
    }

    // === 真低盒集成验证(显式运行) ===

    #[test]
    #[cfg(windows)]
    #[ignore = "真低盒验证:需 Windows + 本机 VS 工具链;cargo test -- --ignored 显式运行(design-36 期3 首航)"]
    fn appcontainer_lowbox_cargo_version() {
        let cargo = locate_program_in_path("cargo").expect("cargo in PATH");
        let workdir = tempfile::tempdir().unwrap();
        let outcome = run_lowbox(&LowboxSpec {
            program: cargo,
            args: vec!["--version".to_string()],
            workdir: workdir.path().to_path_buf(),
            timeout: Duration::from_secs(180),
        })
        .expect("lowbox run");
        assert!(!outcome.timed_out, "cargo --version 不应超时");
        let text = String::from_utf8_lossy(&outcome.output.stdout);
        assert!(
            text.contains("cargo"),
            "低盒内 cargo --version 应有输出版本行: stdout={text} stderr={}",
            String::from_utf8_lossy(&outcome.output.stderr)
        );
    }
}
