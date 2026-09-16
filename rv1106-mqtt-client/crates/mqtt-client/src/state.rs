//! 连接状态机 / 10 槽上行 FIFO / 内部事件（LLD-003 §7.1/§7.5/§5）。

use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use crate::downlink::UiReply;

// ---------------------------------------------------------------------------
// 内部事件：Moonraker / dispatcher → AppModule
// ---------------------------------------------------------------------------

/// 上行触发事件（各生产者经 mpsc 通道投递，AppModule 在 on_tick 中消费）。
#[derive(Debug, Clone)]
pub enum Event {
    /// Moonraker 状态已更新（AppState 已被写入），应尽快发布状态包。
    StatusUpdated,
    /// Moonraker 已连接（Klippy 就绪）。
    MrConnected,
    /// Moonraker 断线。
    MrDisconnected,
    /// gcode 执行结果（cmdType 原样回传）。
    GcodeResult { cmd_type: String, result: String },
    /// 文件下载开始（协议 §5.5：设备→服务器 `download_begin` 回复/上报）。
    /// MQTT 路径由模块直接回包；UDS（UI 发起）路径经此事件触发 `download_begin` 上行。
    DownloadStarted { file_type: u8, file_name: String },
    /// 文件下载完成（err_code: 0 成功/1 忙/2 无目录或写失败/3 传输超限/4 不支持 file_type）。
    /// `from_ui`：true = UDS（UI）触发，其忙标志由 UDS 线程自行清位，AppModule 不再清。
    DownloadFinished { file_type: u8, file_name: String, err_code: u8, from_ui: bool },
    /// 文件列表查询结果。
    FileListResult { files: Vec<(String, u64)> },
    /// 硬件告警（errType 1~20）。
    Alarm { err_type: u8, err_msg: String },
    /// UI 经 UDS 发起的设备解绑请求（触发 device_unbind 上行）。
    UiUnbind,
    /// UI 经 UDS 发起的固件升级查询：AppModule 据此发布 `upgrade_query` 上行，
    /// 并暂存回复通道（reply_tx），待服务器下行回复后回填 UDS 响应。
    UpgradeQueryRequested { reply_tx: Option<mpsc::Sender<UiReply>> },
}

// ---------------------------------------------------------------------------
// UI 可见云端状态（klipper_screen 经 UDS bind_status 读取，AppModule 写入）
// ---------------------------------------------------------------------------

/// UI 关心的云连接/绑定状态镜像，由 AppModule 在连接/登录变化时更新。
#[derive(Debug, Clone)]
pub struct UiState {
    /// 已连上 MQTT Broker。
    pub cloud_connected: bool,
    /// Moonraker（Klippy）已连接。
    pub moonraker_connected: bool,
    /// 最近一次 login 回复的 bindState（0=已绑定/1=未绑定/2=未录入）。
    /// 绑定态只由此 3 值决定，`bound` 是其冗余派生量，不再单独存储/下发。
    pub bind_state: u8,
    /// 绑定的云端账号。
    pub account: String,
    /// 固件升级查询结果（服务器 `upgrade_query` 下行回复填充，供 UDS 返回 UI 显示固件文件名）。
    pub firmware_server_ip: String,
    pub firmware_mcu_file: String,
    pub firmware_esp_file: String,
    /// 下载忙闲标志（AV-1）：任一入口（MQTT `download_begin` / UDS `download`）置位，
    /// 完成后清位；用于拒绝并发/叠加下载（协议忙语义，err_code=1）。
    pub downloading: bool,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            cloud_connected: false,
            moonraker_connected: false,
            // 默认 2=未录入：尚未收到 login 回复时状态未知，UI 必须显示「未录入」而非误判「已绑定」。
            bind_state: 2,
            account: String::new(),
            firmware_server_ip: String::new(),
            firmware_mcu_file: String::new(),
            firmware_esp_file: String::new(),
            downloading: false,
        }
    }
}

/// 跨线程共享的 UI 状态（AppModule 写、UDS 服务读）。
pub type SharedUiState = Arc<Mutex<UiState>>;

// ---------------------------------------------------------------------------
// 连接状态机
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Disconnected,
    Connecting,
    AwaitLoginReply,
    Ready,
    Reconnecting,
}

pub const LOGIN_REPLY_TIMEOUT_SECS: u64 = 15;
pub const HEARTBEAT_IDLE_SECS: u64 = 3 * 60; // 3min 无下行 → 状态上报
pub const MAX_LOGIN_ATTEMPTS: u8 = 3;

/// MQTT 连接状态机（LLD-003 §7.2）。
#[derive(Debug, Clone)]
pub struct ConnStateMachine {
    pub state: ConnState,
    pub login_attempts: u8,
    pub login_sent_at_ts: Option<u64>,
    pub last_downlink_ts: u64,
    pub last_status_publish_ts: u64,
    pub bound: bool,
    /// 最近一次 login 回复的 bindState（0=已绑定 / 1=未绑定 / 2=未录入）。
    pub bind_state: u8,
}

impl Default for ConnStateMachine {
    fn default() -> Self {
        Self {
            state: ConnState::Disconnected,
            login_attempts: 0,
            login_sent_at_ts: None,
            last_downlink_ts: 0,
            last_status_publish_ts: 0,
            bound: false,
            bind_state: 0,
        }
    }
}

impl ConnStateMachine {
    /// TCP 连接建立（MQTT CONNECT 前）。
    pub fn on_connect(&mut self, now: u64) {
        self.state = ConnState::Connecting;
        self.login_attempts = 0;
        self.last_downlink_ts = now;
        self.bound = false;
    }

    /// login 包已发出。
    pub fn on_login_sent(&mut self, now: u64) {
        self.login_sent_at_ts = Some(now);
        self.login_attempts += 1;
        self.state = ConnState::AwaitLoginReply;
    }

    /// 收到 login 回复。
    ///
    /// R1 §5.1：`bindState` 0=已绑定 / 1=序列号未绑定 / 2=序列号未录入。
    pub fn on_login_reply(&mut self, bind_state: u8, now: u64) {
        self.bind_state = bind_state;
        self.bound = bind_state == 0;
        self.login_sent_at_ts = None;
        self.login_attempts = 0;
        self.state = ConnState::Ready;
        self.last_downlink_ts = now;
    }

    /// 收到任意下行包（刷新"有下行"时间）。
    pub fn on_downlink(&mut self, now: u64) {
        self.last_downlink_ts = now;
    }

    pub fn on_error(&mut self) {
        self.state = ConnState::Reconnecting;
        self.login_sent_at_ts = None;
    }

    /// login 回复超时判定。
    pub fn login_reply_timeout(&self, now: u64) -> bool {
        matches!(self.state, ConnState::AwaitLoginReply)
            && self.login_sent_at_ts.map(|t| now.saturating_sub(t) > LOGIN_REPLY_TIMEOUT_SECS).unwrap_or(false)
    }

    /// 心跳判定：Ready 状态且无下行 ≥3min，且 3min 内未发布过状态包。
    pub fn heartbeat_due(&self, now: u64) -> bool {
        self.state == ConnState::Ready
            && now.saturating_sub(self.last_downlink_ts) >= HEARTBEAT_IDLE_SECS
            && now.saturating_sub(self.last_status_publish_ts) >= HEARTBEAT_IDLE_SECS
    }

    /// 需要重连（login 超时 / 重试超限）。
    pub fn should_reconnect(&self, now: u64) -> bool {
        self.login_attempts >= MAX_LOGIN_ATTEMPTS || self.login_reply_timeout(now)
    }
}

// ---------------------------------------------------------------------------
// 10 槽上行 FIFO
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct FifoItem {
    pub payload: Vec<u8>,
}

/// 上行出队 FIFO（LLD-003 §7.5：容量 10）。
///
/// 早期实现带「优先级淘汰」逻辑，但所有入队调用（`modules.rs` 的 `enqueue`）优先级恒为 0，
/// 该逻辑从未生效，故简化为普通有界 FIFO：未满追加、已满丢弃新项、出队按先入先出。
pub struct UplinkFifo {
    items: std::collections::VecDeque<FifoItem>,
    capacity: usize,
}

impl UplinkFifo {
    pub fn new(capacity: usize) -> Self {
        Self { items: Default::default(), capacity: capacity.max(1) }
    }

    pub fn len(&self) -> usize { self.items.len() }
    pub fn is_empty(&self) -> bool { self.items.is_empty() }

    /// 入队：未满则追加；已满则丢弃新项（保持先入先出语义）。
    pub fn push(&mut self, item: FifoItem) {
        if self.items.len() < self.capacity {
            self.items.push_back(item);
        }
    }

    /// 出队：优先最早入队者（FIFO）。
    pub fn pop(&mut self) -> Option<FifoItem> {
        self.items.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conn_state_login_flow() {
        let mut c = ConnStateMachine::default();
        c.on_connect(100);
        assert_eq!(c.state, ConnState::Connecting);
        c.on_login_sent(100);
        assert_eq!(c.state, ConnState::AwaitLoginReply);
        assert!(!c.login_reply_timeout(110));
        assert!(c.login_reply_timeout(120)); // >15s
        // R1：bindState=0 表示已绑定
        c.on_login_reply(0, 121);
        assert_eq!(c.state, ConnState::Ready);
        assert!(c.bound);
        // bindState=1 未绑定
        c.on_login_reply(1, 122);
        assert!(!c.bound);
    }

    #[test]
    fn heartbeat_due_logic() {
        let mut c = ConnStateMachine::default();
        c.on_connect(0);
        c.on_login_sent(0);
        c.on_login_reply(1, 1);
        c.last_status_publish_ts = 1;
        assert!(!c.heartbeat_due(100)); // 不足 3min
        assert!(c.heartbeat_due(181));
    }

    #[test]
    fn fifo_capacity_and_priority() {
        let mut f = UplinkFifo::new(3);
        f.push(FifoItem { payload: b"low".to_vec() });
        f.push(FifoItem { payload: b"high".to_vec() });
        f.push(FifoItem { payload: b"mid".to_vec() });
        assert_eq!(f.len(), 3);
        // 满时推入新项：丢弃新项（有界 FIFO）
        f.push(FifoItem { payload: b"new".to_vec() });
        assert_eq!(f.len(), 3);
        assert_eq!(f.pop().unwrap().payload, b"low".to_vec());
        assert_eq!(f.pop().unwrap().payload, b"high".to_vec());
        assert_eq!(f.pop().unwrap().payload, b"mid".to_vec());
        assert!(f.pop().is_none());
    }

    #[test]
    fn fifo_drop_lower_priority_new() {
        let mut f = UplinkFifo::new(1);
        f.push(FifoItem { payload: b"a".to_vec() });
        f.push(FifoItem { payload: b"b".to_vec() }); // 满，丢弃新项
        assert_eq!(f.len(), 1);
        assert_eq!(f.pop().unwrap().payload, b"a".to_vec());
    }
}
