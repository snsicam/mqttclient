//! Marlin → Klipper 行为转换表（LLD-003 §6，依 Klipper 源码核验）。
//!
//! 转换结论（详见详细设计说明书 §6）：
//! - Klipper 原生支持：M104/M140/M106/M107/M115/M117/M118、G28、M20~M27（virtual_sdcard）→ 直通；
//! - M28/M29/M30（SD 写/删除）原生报错 → 翻译为 server.files 操作 / delete_file；
//! - M32（选文件打印）→ printer.print.start；
//! - G29N（调平）→ BED_MESH_CALIBRATE；
//! - M24/M25/M26 → 打印恢复/暂停/取消。

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GcodeKind {
    /// 直通 Klipper 原生命令。
    Native(String),
    /// 调平（G29N → BED_MESH_CALIBRATE）。
    Leveling,
    /// 打印开始（M32 file → printer.print.start）。
    PrintStart(String),
    /// 删除文件（M30 file → server.files.delete_file）。
    DeleteFile(String),
    /// 文件列表（M20 → server.files.list）。
    ListFiles,
    /// 恢复打印（M24）。
    Resume,
    /// 暂停打印（M25）。
    Pause,
    /// 取消打印（M26 / M0 特殊）。
    Cancel,
    /// 不支持的 SD 写命令（M28/M29）。
    Unsupported(String),
    /// 空/无法识别。
    Empty,
}

/// 解析 M 码：`"M32 x.gcode"` → `Some((32, "x.gcode"))`。
/// 精确提取数字，避免 `M300`（提示音，直通）被误判为 `M30`（删除文件）。
fn mcode(cmd: &str) -> Option<(u32, &str)> {
    let t = cmd.trim_start();
    if !t.starts_with(['M', 'm']) {
        return None;
    }
    let rest = &t[1..];
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    let code: u32 = rest[..end].parse().ok()?;
    Some((code, rest[end..].trim_start()))
}

/// 取首个参数并清理引号/尖括号（文件路径）。
fn first_token(s: &str) -> String {
    s.split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(['"', '\'', '<', '>'])
        .to_string()
}

/// 转换单条 GCODE。
///
/// 仅处理平台协议命令（文件操作 / 打印控制 / 调平）。Marlin 方言命令
/// （M92/M201/M203/M205/M300/M420/M600 等）由设备端 macros-marlin.cfg 的
/// Klipper 宏统一实现，此处直通不重复翻译。
pub fn translate(cmd: &str) -> GcodeKind {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return GcodeKind::Empty;
    }
    let upper = trimmed.to_ascii_uppercase();

    // G29N → 自动调平（BED_MESH_CALIBRATE）
    if upper.starts_with("G29") {
        return GcodeKind::Leveling;
    }

    match mcode(trimmed) {
        Some((32, rest)) => GcodeKind::PrintStart(first_token(rest)),
        Some((30, rest)) => GcodeKind::DeleteFile(first_token(rest)),
        Some((20, _)) => GcodeKind::ListFiles,
        Some((24, _)) => GcodeKind::Resume,
        Some((25, _)) => GcodeKind::Pause,
        // M26 / M524（Marlin 取消打印，Klipper 无 M524）→ printer.print.cancel
        Some((26, _)) | Some((524, _)) => GcodeKind::Cancel,
        // M28/M29：Klipper 原生报错（"SD write not supported"），显式拒绝
        Some((28, _)) | Some((29, _)) => GcodeKind::Unsupported(trimmed.to_string()),
        // 其余 → 直通（Marlin 方言命令由设备端 macros-marlin.cfg 宏处理）
        _ => GcodeKind::Native(trimmed.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_leveling() {
        assert_eq!(translate("G29N"), GcodeKind::Leveling);
        assert_eq!(translate("G29 N 5"), GcodeKind::Leveling);
    }

    #[test]
    fn translate_sd_ops() {
        assert_eq!(translate("M20"), GcodeKind::ListFiles);
        assert_eq!(translate("M32 x.gcode"), GcodeKind::PrintStart("x.gcode".into()));
        assert_eq!(translate("M30 x.gcode"), GcodeKind::DeleteFile("x.gcode".into()));
        assert!(matches!(translate("M28 x.gcode"), GcodeKind::Unsupported(_)));
        assert!(matches!(translate("M29"), GcodeKind::Unsupported(_)));
    }

    #[test]
    fn translate_print_ctrl() {
        assert_eq!(translate("M24"), GcodeKind::Resume);
        assert_eq!(translate("M25"), GcodeKind::Pause);
        assert_eq!(translate("M26"), GcodeKind::Cancel);
        assert_eq!(translate("M524"), GcodeKind::Cancel, "Marlin 取消打印 M524");
        assert_eq!(translate("M524 C"), GcodeKind::Cancel, "M524 带参数");
    }

    #[test]
    fn translate_native() {
        assert_eq!(translate("G28"), GcodeKind::Native("G28".into()));
        assert_eq!(translate("M104 S200"), GcodeKind::Native("M104 S200".into()));
        assert_eq!(translate("M115"), GcodeKind::Native("M115".into()));
        assert_eq!(translate("M23 x.gcode"), GcodeKind::Native("M23 x.gcode".into()));
    }

    #[test]
    fn translate_m30_vs_m300() {
        // M30 删除文件；M300（提示音）不能被 M30 前缀误匹配
        assert_eq!(translate("M30 a.gcode"), GcodeKind::DeleteFile("a.gcode".into()));
        assert_eq!(
            translate("M300 S1000 P200"),
            GcodeKind::Native("M300 S1000 P200".into()),
            "M300 与 M30 仅前缀之差，必须精确区分"
        );
    }

    #[test]
    fn translate_marlin_dialect_passthrough() {
        // Marlin 方言命令不再翻译：由设备端 macros-marlin.cfg（Klipper 宏）统一处理
        assert_eq!(translate("M201 X1000 Y2000"), GcodeKind::Native("M201 X1000 Y2000".into()));
        assert_eq!(translate("M203 X500 Y300"), GcodeKind::Native("M203 X500 Y300".into()));
        assert_eq!(translate("M205 J0.05"), GcodeKind::Native("M205 J0.05".into()));
        assert_eq!(translate("M92 X80"), GcodeKind::Native("M92 X80".into()));
        assert_eq!(translate("M851 Z-1.5"), GcodeKind::Native("M851 Z-1.5".into()));
        assert_eq!(translate("M401"), GcodeKind::Native("M401".into()));
        assert_eq!(translate("M402"), GcodeKind::Native("M402".into()));
        assert_eq!(translate("M206 Z0.2"), GcodeKind::Native("M206 Z0.2".into()));
        assert_eq!(translate("M420 S1"), GcodeKind::Native("M420 S1".into()));
        assert_eq!(translate("M600"), GcodeKind::Native("M600".into()), "换丝由 macros.cfg 交互实现");
    }

    #[test]
    fn translate_empty() {
        assert_eq!(translate("  "), GcodeKind::Empty);
    }
}
