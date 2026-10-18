// Windows Job Object 封装：整进程树回收的硬保证。
//
// 语义：壳创建 job 后优先自收编（self-assign），此后 spawn 的子进程自动
// 隶属该 job；壳进程无论正常退出、崩溃还是被任务管理器强杀，job 句柄
// 随进程关闭即触发 KILL_ON_JOB_CLOSE，serve/engine 整树必回收，不产生
// 孤儿进程。自收编失败（如宿主已处于不容嵌套的 job 中）时退化为逐个
// 收编子进程，收编失败的子进程由退出序列显式 terminate 兜底。

#![cfg(windows)]

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcess};

const PROCESS_ALL_ACCESS: u32 = 0x001F_FFFF;

pub struct JobHandle {
    handle: HANDLE,
}

/// Windows 句柄为进程级整数资源，Job 系 API 均线程安全，跨线程持有无碍。
unsafe impl Send for JobHandle {}

impl JobHandle {
    /// 创建 job 并设置 KILL_ON_JOB_CLOSE（句柄关闭即整树回收）。
    pub fn create() -> Result<Self, String> {
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err("CreateJobObjectW failed".into());
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                CloseHandle(handle);
                return Err("SetInformationJobObject failed".into());
            }
            Ok(Self { handle })
        }
    }

    /// 自收编：壳进程自身加入 job，此后 spawn 的子进程自动隶属。
    pub fn assign_self(&self) -> bool {
        unsafe { AssignProcessToJobObject(self.handle, GetCurrentProcess()) != 0 }
    }

    /// 逐个收编既有进程（自收编失败时的退化路径）。
    pub fn assign_pid(&self, pid: u32) -> bool {
        unsafe {
            let h = OpenProcess(PROCESS_ALL_ACCESS, 0, pid);
            if h.is_null() {
                return false;
            }
            let ok = AssignProcessToJobObject(self.handle, h) != 0;
            CloseHandle(h);
            ok
        }
    }

    /// 立即终止 job 内全部进程（退出序列主路径）。
    pub fn terminate(&self) {
        unsafe {
            TerminateJobObject(self.handle, 0);
        }
    }
}

impl Drop for JobHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}
