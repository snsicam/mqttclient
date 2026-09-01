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
/// 精确提取数字，避免 `M300`（提示音）被误判为 `M30`（删除文件）。
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
        Some((26, _)) => GcodeKind::Cancel,
        // M28/M29：Klipper 原生报错（"SD write not supported"）
        Some((28, _)) | Some((29, _)) => GcodeKind::Unsupported(trimmed.to_string()),
        // 其余（G 码、M104/M140/M106/M107/M115/M117/M118/M23/M300…）Klipper 原生支持 → 直通
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
        // M30 删除文件；M300（提示音）必须直通，不能被 M30 前缀误匹配
        assert_eq!(translate("M30 a.gcode"), GcodeKind::DeleteFile("a.gcode".into()));
        assert_eq!(translate("M300 S1000 P200"), GcodeKind::Native("M300 S1000 P200".into()));
    }

    #[test]
    fn translate_empty() {
        assert_eq!(translate("  "), GcodeKind::Empty);
    }
}
