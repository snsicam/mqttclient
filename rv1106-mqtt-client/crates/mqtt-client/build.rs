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

    cross_link_args();
}

/// 交叉目标（armv7 + uclibc）的链接参数：
/// - sysroot：跟随 `RV1106_TOOLCHAIN` 环境变量（与 scripts/build.sh 同一变量），
///   缺省为 `/opt/toolchain/arm-rockchip830-linux-uclibcgnueabihf`（CI/容器标准布局）；
/// - uclibc 缺失符号的桩库：`getauxval` / `posix_spawn*`（`<workspace>/cross/` 下预编译），
///   经 `CARGO_MANIFEST_DIR` 定位，不再依赖机器特定绝对路径。
/// 仅在交叉目标时生效，host 构建完全不受影响。
fn cross_link_args() {
    const CROSS_TARGET: &str = "armv7-unknown-linux-gnueabihf";
    const CROSS_TRIPLE: &str = "arm-rockchip830-linux-uclibcgnueabihf";

    let Ok(target) = std::env::var("TARGET") else { return };
    if target != CROSS_TARGET {
        return;
    }

    println!("cargo:rerun-if-env-changed=RV1106_TOOLCHAIN");
    let toolchain_root = std::env::var("RV1106_TOOLCHAIN")
        .unwrap_or_else(|_| format!("/opt/toolchain/{CROSS_TRIPLE}"));
    let sysroot = std::path::Path::new(&toolchain_root)
        .join(CROSS_TRIPLE)
        .join("sysroot");
    println!("cargo:rustc-link-arg=--sysroot={}", sysroot.display());
    println!("cargo:rustc-link-arg=-L{}/usr/lib", sysroot.display());

    // 桩库目录：workspace 根下的 cross/（crates/mqtt-client/../../cross）。
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let cross_dir = std::path::Path::new(&manifest_dir).join("../../cross");
    let cross_dir = cross_dir
        .canonicalize()
        .expect("cross/ stub dir must exist at workspace root");
    println!("cargo:rustc-link-search=native={}", cross_dir.display());
    // 必须用 link-arg 而非 link-lib：link-lib 会被 rustc 放在 rlib 之前，
    // 静态库按命令行顺序解析，早于引用它的 libstd 出现则不会拉入成员，
    // posix_spawn*/getauxval 仍为 undefined。link-arg 追加在命令行末尾
    // （libstd 与 -lc 之后），可正确解析这些桩符号。
    println!("cargo:rustc-link-arg=-lgetauxval");
    println!("cargo:rustc-link-arg=-lposix_spawn");
}
