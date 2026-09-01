// build.rs — 交叉编译辅助（占位）
//
// getauxval 桩已预编译为 cross/libgetauxval.a，通过 .cargo/config.toml 的
// rustflags (-L cross -lgetauxval) 链接（见该文件 armv7 目标段）。
// 此处不再动态编译，以避免在 host 构建时误用交叉工具链。
