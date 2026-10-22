fn main() {
    // capabilities 目录变更(新增/修改 capability 文件)必须重跑本脚本重生成 ACL:
    // tauri_build 内部 rerun 规则未覆盖"新增文件"场景,曾致新 capability 未进
    // 二进制、remote 窗口控制静默失效(2026-10-22 三键失联破案)
    println!("cargo:rerun-if-changed=capabilities");
    tauri_build::build()
}
