/*
 * 最小 getauxval 桩，用于老版本 uclibc (1.0.31) 工具链。
 *
 * 背景：Rust std (arm-linux) 与 ring 0.17 在运行时调用 getauxval() 读取
 * AT_HWCAP / AT_PAGESZ 等。Luckfox RV1106 的 uclibc 1.0.31 不导出该符号，
 * 导致交叉链接失败。此处提供弱符号桩：
 *   - AT_PAGESZ(6) -> 4096（页大小，Rust stack_overflow 需要）
 *   - 其余 -> 0（保守：不声明硬件特性，ring 退化为软件实现）
 */
unsigned long getauxval(unsigned long type) {
    if (type == 6 /* AT_PAGESZ */) {
        return 4096UL;
    }
    return 0UL;
}
