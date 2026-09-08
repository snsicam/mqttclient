//! build.rs — 注入编译期元数据，供启动日志打印版本与编译时间。
//!
//! 输出的 `BUILD_TIME` / `GIT_HASH` 在 `main.rs` 通过 `env!` 读取。
//! 通过监控 `src`/`Cargo.toml` 变化，确保每次改代码后编译时间刷新。

use std::process::Command;

fn main() {
    // 编译时间（本地时区，含时区偏移）。
    let build_time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %z").to_string();
    println!("cargo:rustc-env=BUILD_TIME={build_time}");

    // git 短 hash；仓库不可用时回退为 unknown。
    let git_hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=GIT_HASH={git_hash}");

    // 源码或清单变化时重新生成（让编译时间随每次修复刷新）。
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=build.rs");
}
