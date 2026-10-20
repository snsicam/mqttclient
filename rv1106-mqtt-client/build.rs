// build.rs — 占位说明（workspace 根，虚拟清单不会执行构建脚本，本文件无实际作用）
//
// 交叉目标的 sysroot 与 uclibc 缺失符号桩库（cross/libgetauxval.a、
// cross/libposix_spawn.a）由 crates/mqtt-client/build.rs 的 cross_link_args()
// 生成链接参数：sysroot 跟随 RV1106_TOOLCHAIN 环境变量（缺省
// /opt/toolchain/arm-rockchip830-linux-uclibcgnueabihf），桩库经
// CARGO_MANIFEST_DIR 定位，无机器特定绝对路径。
