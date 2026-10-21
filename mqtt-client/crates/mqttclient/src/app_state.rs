//! 共享状态 AppState（LLD-003 §5.5）。
//!
//! 由 Moonraker `notify_status_update` 通知经 state_bridge 写入；
//! 各业务模块读取并组织上行状态包。以 `Arc<Mutex<AppState>>` 共享。

/// MXS `printState`（R1 §5.3 定义 0-7）。
///
/// Klipper 的 `print_stats.state` 仅有 6 态（standby/printing/paused/complete/cancelled/error），
/// 只能覆盖 0-5；6（断电续打）/7（换料）Klipper 无原生对应，暂不使用
/// （如需支持，须由平台侧经 `gcode_macro` 配合上报，见 OQ-10）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrintState {
    /// 0 空闲
    Idle = 0,
    /// 1 打印
    Printing = 1,
    /// 2 完成
    Complete = 2,
    /// 3 暂停
    Paused = 3,
    /// 4 失败
    Error = 4,
    /// 5 取消
    Cancelled = 5,
}

impl PrintState {
    /// 从 Klipper `print_stats.state` 映射到 R1 printState。
    /// 未知状态保守映射为 [`Self::Idle`]（避免误报告警）。
    pub fn from_klipper(s: &str) -> Self {
        match s {
            "printing" => Self::Printing,
            "paused" => Self::Paused,
            "complete" => Self::Complete,
            "cancelled" => Self::Cancelled,
            "error" => Self::Error,
            _ => Self::Idle,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AppState {
    // 连接
    pub moonraker_connected: bool,
    // 温度（°C，f64 便于累加）
    pub nozzle_actual: f64,
    pub nozzle_target: f64,
    pub bed_actual: f64,
    pub bed_target: f64,
    // 风扇（0-100）
    pub main_fan_pct: u8,
    pub board_fan_pct: u8,
    pub aux_fan_pct: u8,
    // 预热
    pub preheat_type: u8,
    pub preheat_state: u8,
    pub heat_state: u8,
    // 打印
    pub print_state: u8,
    pub print_progress: u8,
    pub current_file: String,
    pub print_elapsed_secs: u64,
    pub print_remain_secs: u64,
    pub zoffset_x100: i32,
    // 调平
    pub leveling_status: u8,
    pub leveling_point: u8,
    pub leveling_total: u8,
    // 硬件
    pub runout: bool,
    pub beep: bool,
    pub light: bool,
    pub breathing: bool,
    pub sd_ok: bool,
    pub level_ok: bool,
    pub door_open: bool,
}

impl AppState {
    /// 温度/风扇/打印状态是否"忙"（用于状态上报周期切换：忙 10s / 闲 5min，R1 §5.3）。
    pub fn is_busy(&self) -> bool {
        self.print_state != PrintState::Idle as u8
            || self.nozzle_actual > 40.0
            || self.nozzle_target > 1.0
            || self.bed_actual > 40.0
            || self.bed_target > 1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_state_mapping() {
        // R1 §5.3：0空闲/1打印/2完成/3暂停/4失败/5取消
        assert_eq!(PrintState::from_klipper("standby") as u8, 0);
        assert_eq!(PrintState::from_klipper("printing") as u8, 1);
        assert_eq!(PrintState::from_klipper("complete") as u8, 2);
        assert_eq!(PrintState::from_klipper("paused") as u8, 3);
        assert_eq!(PrintState::from_klipper("error") as u8, 4);
        assert_eq!(PrintState::from_klipper("cancelled") as u8, 5);
        // 未知状态保守映射为 Idle（不误报告警）
        assert_eq!(PrintState::from_klipper("??") as u8, 0);
    }

    #[test]
    fn busy_detection() {
        let s = AppState::default();
        assert!(!s.is_busy());
        let s2 = AppState { nozzle_target: 200.0, ..Default::default() };
        assert!(s2.is_busy());
        let s3 = AppState { print_state: 1, ..Default::default() };
        assert!(s3.is_busy());
    }
}
