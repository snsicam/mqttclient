//! BluFi 蓝牙配网模块（详细设计 §4）。
//!
//! 阶段 1/2/3：模块骨架 + BluFi 协议栈 frame/codec + wpa_cli 封装 wifi。
//! 阶段 4：GATT/D-Bus 服务端见 [`gatt`]（基于 `bluer`，`GattBleLink` 实现 [`BleLink`]）；
//! 应用通过 [`BluFiWorker::spawn_with_link`] 注入真实链路。

pub mod frame;
pub mod codec;
pub mod wifi;
pub mod init;
pub mod gatt;

pub use frame::*;
pub use codec::*;
pub use wifi::*;

use serde::{Deserialize, Serialize};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

// =================== 配置 ===================

/// 安全模式（v1 仅 Plain 生效；Crc16/Aes/DhAes 为后续扩展预留）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SecurityMode {
    #[default]
    Plain,
    Crc16,
    Aes,
    DhAes,
}
impl SecurityMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            SecurityMode::Plain => "plain",
            SecurityMode::Crc16 => "crc16",
            SecurityMode::Aes => "aes",
            SecurityMode::DhAes => "dhaes",
        }
    }
    pub fn from_str(s: &str) -> SecurityMode {
        match s.to_ascii_lowercase().as_str() {
            "crc16" => SecurityMode::Crc16,
            "aes" => SecurityMode::Aes,
            "dhaes" => SecurityMode::DhAes,
            _ => SecurityMode::Plain,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BluFiConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// 是否由应用层注册 LE 广播。
    ///
    /// 广播与 GATT 注册是两件事：广播只负责「让手机扫到/能连上」，GATT 应用
    /// 负责「连上后能看到哪些服务与特征」。
    ///
    /// **默认 `false`**：板子系统侧已在广播（手机 APP 能直接发现并连接），
    /// 应用层只注册 GATT 应用即可，避免重复注册广播失败
    /// （BlueZ/控制器对同时广播的实例数有限制）。
    /// 仅当系统侧不广播、需要应用层自己广播时才设为 `true`。
    #[serde(default = "default_advertise")]
    pub advertise: bool,
    #[serde(default = "default_name_prefix")]
    pub name_prefix: String,
    #[serde(default = "default_adapter")]
    pub adapter: String,
    #[serde(default = "default_dbus_to")]
    pub dbus_timeout_ms: u64,
    #[serde(default = "default_scan_to")]
    pub scan_timeout_ms: u64,
    #[serde(default = "default_conn_to")]
    pub connect_timeout_ms: u64,
    #[serde(default = "default_iface")]
    pub wpa_iface: String,
    #[serde(default = "default_wpa_ctrl")]
    pub wpa_ctrl: String,
    /// wpa_supplicant 配置文件路径，配网后将 SSID/密码写入此处（默认 /data/wpa_supplicant.conf）。
    #[serde(default = "default_wpa_conf")]
    pub wpa_conf: String,
    #[serde(default = "default_ack_repeat")]
    pub ack_repeat: u8,
    /// 扫描列表单帧字节上限。`0` = 不限：超过 255 字节时由 `send_data` 自动按 BluFi
    /// 分片发送（对端 BlufiClient 支持重组，不丢热点）。
    /// 仅当对端 APP 收到分片即断连时，才设为 `255` 退化为单帧（丢弃装不下的最弱热点）。
    #[serde(default = "default_scan_single_frame_bytes")]
    pub scan_single_frame_bytes: usize,
    #[serde(default = "default_ack_interval")]
    pub ack_interval_ms: u64,
    /// 安全模式字符串：`plain`/`crc16`/`aes`/`dhaes`（v1 仅 plain 生效）。
    #[serde(default = "default_security")]
    pub security: String,
    #[serde(default = "default_ver_major")]
    pub version_major: u8,
    #[serde(default = "default_ver_minor")]
    pub version_minor: u8,
    /// 底层 HCI 传输串口（RV1106 UART，如 /dev/ttyS1）。
    /// 注意：应用层**不**打开此串口；它由系统侧 `btattach` 绑定为内核 hci0，
    /// 应用通过 BlueZ D-Bus/socket 通信。此字段仅供 `init.rs` 提示 `btattach` 命令。
    #[serde(default = "default_uart")]
    pub uart: String,
    /// HCI UART 波特率（系统侧 `btattach -s` 参数，AIC8800 默认 115200），仅供系统侧参考。
    #[serde(default = "default_baud")]
    pub baud: u32,
}
impl Default for BluFiConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            advertise: default_advertise(),
            name_prefix: default_name_prefix(),
            adapter: default_adapter(),
            dbus_timeout_ms: default_dbus_to(),
            scan_timeout_ms: default_scan_to(),
            connect_timeout_ms: default_conn_to(),
            wpa_iface: default_iface(),
            wpa_ctrl: default_wpa_ctrl(),
            wpa_conf: default_wpa_conf(),
            ack_repeat: default_ack_repeat(),
            scan_single_frame_bytes: default_scan_single_frame_bytes(),
            ack_interval_ms: default_ack_interval(),
            security: default_security(),
            version_major: default_ver_major(),
            version_minor: default_ver_minor(),
            uart: default_uart(),
            baud: default_baud(),
        }
    }
}
impl BluFiConfig {
    pub fn security_mode(&self) -> SecurityMode {
        SecurityMode::from_str(&self.security)
    }
    /// 蓝牙广播名 = `{前缀}-{device_id}`。
    /// `前缀` 取 `name_prefix`（若非空），否则取 `model`（即配置 `[mqtt] model`，随型号变化）。
    /// 即完整广播名 = `{model|name_prefix}-{device.id}`，如 `M1S-Ge33700a6620dfddc`。
    /// 注意：`name_prefix` 为空时**不可**直接用它拼（会得到 `-device_id`），必须回退到 `model`。
    pub fn bluetooth_name(&self, model: &str, device_id: &str) -> String {
        let prefix = if self.name_prefix.is_empty() { model } else { &self.name_prefix };
        format!("{}-{}", prefix, device_id)
    }
}

fn default_enabled() -> bool {
    true
}
/// 默认**广播**：RV1106 buildroot 板子系统侧广播器未必发布 BluFi 的 `0xFFFF` 服务
/// （实测需应用层自己广播，手机才能发现并连接，否则报 discover service failed）。
/// 故默认 `true` 由 mqtt-client 自注册 LE 广播（ServiceUUIDs=[0xFFFF]）。
/// 若系统侧已在广播，本应用的 `RegisterAdvertisement` 会失败——`gatt.rs` 已对此
/// 做了优雅降级（仅告警并继续注册 GATT 应用），不会阻断 BluFi 服务。
fn default_advertise() -> bool {
    true
}
/// 广播名前缀：**留空则自动取 `[mqtt] model`**。
/// 实测（详细设计 §4.3.2）：板子广播名为 `M1S-<deviceId>`，规则是 `{model}-{device.id}`，
/// 随型号自动变化；故默认留空用型号，仅当需要强制指定前缀时才配置该字段。
fn default_name_prefix() -> String {
    String::new()
}
fn default_adapter() -> String {
    "hci0".into()
}
fn default_dbus_to() -> u64 {
    5000
}
fn default_scan_to() -> u64 {
    5000
}
fn default_conn_to() -> u64 {
    30000
}
fn default_iface() -> String {
    "wlan0".into()
}
fn default_wpa_ctrl() -> String {
    "/var/run/wpa_supplicant/wlan0".into()
}
fn default_wpa_conf() -> String {
    // 设备实际使用的配置目录是 /data（/etc 下并非 wpa_supplicant 生效的配置）
    "/data/wpa_supplicant.conf".into()
}
fn default_ack_repeat() -> u8 {
    3
}
fn default_scan_single_frame_bytes() -> usize {
    0 // 0 = 不限，超 255 走分片（不丢热点）
}
fn default_ack_interval() -> u64 {
    500
}
fn default_security() -> String {
    "plain".into()
}
fn default_ver_major() -> u8 {
    1
}
fn default_ver_minor() -> u8 {
    0
}
fn default_uart() -> String {
    "/dev/ttyS1".into()
}
fn default_baud() -> u32 {
    115200
}

// =================== 状态机 ===================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum BluFiState {
    Init,
    RegisterGatt,
    WaitApp,
    Scanning,
    Acking,
    Connecting,
    Finish,
    Failed,
}

#[derive(Debug, Clone)]
pub enum BlufiCmd {
    StartConfig,
    StopConfig,
}

#[derive(Debug, Clone)]
pub enum BlufiEvent {
    EnterConfigMode,
    AppConnected,
    WifiConnected { ssid: String },
    WifiFailed { reason: String },
    ExitConfigMode,
}

// =================== BLE 链路抽象 ===================

#[derive(Debug)]
#[allow(dead_code)]
pub enum BleLinkError {
    Send(String),
    Recv(String),
    Closed,
}
impl std::fmt::Display for BleLinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BleLinkError::Send(m) => write!(f, "send: {m}"),
            BleLinkError::Recv(m) => write!(f, "recv: {m}"),
            BleLinkError::Closed => write!(f, "closed"),
        }
    }
}
impl std::error::Error for BleLinkError {}

/// 设备↔APP 的字节链路抽象。阶段 4 由 `gatt::GattBleLink`（BlueZ D-Bus）实现；单测用 [`StubBleLink`]。
pub trait BleLink: Send {
    /// 发送一帧（已是完整 BluFi 字节流）给 APP。
    fn send(&self, bytes: &[u8]) -> Result<(), BleLinkError>;
    /// 非阻塞取一条 APP 写入的字节流；无数据返回 Ok(None)。
    fn try_recv(&self) -> Result<Option<Vec<u8>>, BleLinkError>;
    /// 对端是否开始了一次新的 notify 会话（APP 重连）。是则设备侧应按 §2
    ///「重连清零」重置发送 sequence，避免序号延续上一次会话的累计值。
    /// 取走即清除（一次性语义）。默认 false：无重连语义的链路（单测/桩）。
    fn take_new_session(&self) -> bool {
        false
    }
}

/// 无真实 BLE 时的占位链接（仅供单测 / 阶段 4 前的逻辑验证）。
pub struct StubBleLink {
    pub incoming: std::sync::Mutex<Vec<Vec<u8>>>,
}
impl StubBleLink {
    pub fn new() -> Self {
        Self {
            incoming: std::sync::Mutex::new(Vec::new()),
        }
    }
    /// 向队列压入一条模拟 APP 写入的帧字节流。
    pub fn push_incoming(&self, bytes: Vec<u8>) {
        self.incoming.lock().unwrap().push(bytes);
    }
}
impl BleLink for StubBleLink {
    fn send(&self, _bytes: &[u8]) -> Result<(), BleLinkError> {
        Ok(())
    }
    fn try_recv(&self) -> Result<Option<Vec<u8>>, BleLinkError> {
        Ok(self.incoming.lock().unwrap().pop())
    }
}
impl Default for StubBleLink {
    fn default() -> Self {
        Self::new()
    }
}

// =================== Worker ===================

pub struct BluFiWorker;
impl BluFiWorker {
    /// 启动配网 worker（设备侧）。
    ///
    /// `link` 为 BLE 链路实现：阶段 4 传 `gatt::start_gatt` 返回的 `GattBleLink`，单测传 [`StubBleLink`]。
    /// 返回线程句柄；[`BlufiCmd::StopConfig`] 或命令通道断开时退出。
    pub fn spawn_with_link(
        cfg: BluFiConfig,
        device_id: String,
        cmd_rx: mpsc::Receiver<BlufiCmd>,
        event_tx: mpsc::Sender<BlufiEvent>,
        link: Box<dyn BleLink>,
    ) -> thread::JoinHandle<()> {
        thread::Builder::new()
            .name("blufi".into())
            .spawn(move || {
                let wifi = WifiManager::new(&cfg);
                let mut ctx = WorkerCtx {
                    cfg,
                    device_id,
                    cmd_rx,
                    event_tx,
                    link,
                    asm: FragmentAssembler::new(),
                    seq_out: 0,
                    state: BluFiState::WaitApp,
                    wifi,
                };
                ctx.run();
            })
            .expect("spawn blufi worker")
    }
}

/// 重连（新 notify 会话）后发送 sequence 的起始值。
/// 会话首帧是版本帧（seq=0，由 gatt 在 APP 订阅时补发），故清零后从 1 继续，
/// 使重连后第一个响应帧为 seq=1，与 APP 期望一致（§2「重连清零」）。
const SEQ_AFTER_VERSION: u8 = 1;

struct WorkerCtx {
    cfg: BluFiConfig,
    #[allow(dead_code)]
    device_id: String,
    cmd_rx: mpsc::Receiver<BlufiCmd>,
    event_tx: mpsc::Sender<BlufiEvent>,
    link: Box<dyn BleLink>,
    asm: FragmentAssembler,
    seq_out: u8,
    state: BluFiState,
    wifi: WifiManager,
}

/// 调试：打印即将发送的一帧（字段摘要 + 完整字节流）。
/// 仅在 `RUST_LOG` 含 debug（如 `mqtt_client::blufi=debug`）时输出。
fn debug_tx(f: &BluFiFrame) {
    let bytes = f.encode();
    log::debug!(
        "[blufi] >> tx type={:#04x} ctrl={:#04x} seq={} len={} data={:02x?}",
        f.type_byte(),
        f.frame_ctrl,
        f.sequence,
        f.data.len(),
        f.data
    );
    log::debug!("[blufi] >> tx bytes: {:02x?}", bytes);
}

/// 调试：打印收到的一帧（字段摘要 + 原始字节流）。
fn debug_rx(bytes: &[u8], f: &BluFiFrame) {
    log::debug!(
        "[blufi] << rx type={:#04x} ctrl={:#04x} seq={} len={} data={:02x?}",
        f.type_byte(),
        f.frame_ctrl,
        f.sequence,
        f.data.len(),
        f.data
    );
    log::debug!("[blufi] << rx bytes: {:02x?}", bytes);
}

impl WorkerCtx {
    fn run(&mut self) {
        let _ = self.event_tx.send(BlufiEvent::EnterConfigMode);
        // 进入配网后主动发一次 B4 版本帧（APP 仅记录，不回复）
        let ver = encode_version(self.cfg.version_major, self.cfg.version_minor);
        self.send_frame(PKG_DATA, ftype::VERSION, fc::DIRECTION, &ver);

        loop {
            // APP 重连（新 notify 会话）：按 §2 重置发送 sequence（见 handle_new_session）
            self.handle_new_session();
            // 命令通道（20ms 轮询，兼顾 APP 写入的及时性）
            match self.cmd_rx.recv_timeout(Duration::from_millis(20)) {
                Ok(BlufiCmd::StopConfig) => {
                    let _ = self.event_tx.send(BlufiEvent::ExitConfigMode);
                    break;
                }
                Ok(BlufiCmd::StartConfig) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            // APP 写入通道
            match self.link.try_recv() {
                Ok(Some(bytes)) => self.on_app_frame(&bytes),
                Ok(None) => {}
                Err(_) => break,
            }
        }
    }

    /// APP 重连（新 notify 会话）：按 §2「重连清零」重置发送 sequence 与接收重组状态。
    /// 否则 seq 会延续上次会话的累计值（如 5），而 APP 在版本帧(0) 之后期望下一个是 1，
    /// 跳跃的序号会被判定为丢帧/重放而报错。
    fn handle_new_session(&mut self) {
        if self.link.take_new_session() {
            self.seq_out = SEQ_AFTER_VERSION;
            self.asm = FragmentAssembler::new();
            log::info!(
                "blufi: app reconnected — reset tx sequence to {} (§2 重连清零)",
                SEQ_AFTER_VERSION
            );
        }
    }

    fn next_seq(&mut self) -> u8 {
        let s = self.seq_out;
        self.seq_out = self.seq_out.wrapping_add(1);
        s
    }

    /// 发送单帧（不分片），sequence 自增。
    fn send_frame(&mut self, pkg_type: u8, subtype: u8, frame_ctrl: u8, data: &[u8]) {
        let seq = self.next_seq();
        self.send_frame_seq(pkg_type, subtype, frame_ctrl, data, seq);
    }

    /// 发送单帧（不分片），使用指定 sequence（不递增）。
    /// 重传同一条消息时必须复用同一 seq，否则对端会把重复帧当成 N 条不同的新消息。
    fn send_frame_seq(
        &mut self,
        pkg_type: u8,
        subtype: u8,
        frame_ctrl: u8,
        data: &[u8],
        seq: u8,
    ) {
        let f = BluFiFrame::new(pkg_type, subtype, frame_ctrl, seq, data.to_vec());
        debug_tx(&f);
        if let Err(e) = self.link.send(&f.encode()) {
            log::warn!("blufi send failed: {e}");
        }
    }

    /// 连发 N 帧同一内容（配网回执 / 失败文本）。
    ///
    /// **每帧 sequence 必须递增，不能复用**：`BlufiClientImpl.parseNotification()` 严格校验
    /// `sequence == mReadSequence.incrementAndGet() & 0xff`，重复 seq 会打印
    /// "read sequence wrong" 并丢弃该帧；且计数器已自增，会让**后续所有帧（含 0xF 状态报告）
    /// 全部错位被丢弃**，APP 因此判定配网失败。故这里逐帧 `send_frame`（seq 自增）。
    ///
    /// 间隔 `ack_interval_ms`；**最后一帧之后不再等待**，避免白白拖慢后续配网流程。
    fn send_repeat(&mut self, subtype: u8, data: &[u8]) {
        let repeat = self.cfg.ack_repeat.max(1);
        let interval = Duration::from_millis(self.cfg.ack_interval_ms);
        for i in 0..repeat {
            self.send_frame(PKG_DATA, subtype, fc::DIRECTION, data);
            if i + 1 < repeat {
                thread::sleep(interval);
            }
        }
    }

    /// 发送一段逻辑 data（自动按 255 字节分片；首片带 FRAGMENTED）。
    /// 255 = BluFi 帧 data_len(u8)上限，超过须分片，否则长度字段截断、APP 解码错位。
    fn send_data(&mut self, pkg_type: u8, subtype: u8, data: &[u8]) {
        let base = self.seq_out;
        let frames = split_for_tx(pkg_type, subtype, fc::DIRECTION, base, data, 255);
        for f in &frames {
            debug_tx(f);
            if let Err(e) = self.link.send(&f.encode()) {
                log::warn!("blufi send failed: {e}");
            }
        }
        if let Some(last) = frames.last() {
            self.seq_out = last.sequence.wrapping_add(1);
        }
    }

    fn on_app_frame(&mut self, bytes: &[u8]) {
        log::info!("[blufi] << rx raw {} bytes: {:02x?}", bytes.len(), bytes);
        let f = match BluFiFrame::decode(bytes) {
            Ok(f) => f,
            Err(e) => {
                log::warn!("blufi decode error: {e:?}");
                log::debug!("[blufi] << rx raw (decode failed): {:02x?}", bytes);
                return;
            }
        };
        debug_rx(bytes, &f);
        // 链路层 ACK（业务层显式处理）
        if f.needs_ack() {
            let ack = make_ack(f.sequence);
            if let Err(e) = self.link.send(&ack.encode()) {
                log::warn!("blufi ack send failed: {e}");
            }
        }
        // 分片重组
        let payload = match self.asm.feed(&f) {
            Ok(Some(p)) => p,
            Ok(None) => return, // 等待更多分片
            Err(e) => {
                log::warn!("blufi fragment error: {e:?}");
                return;
            }
        };
        if f.pkg_type == PKG_CTRL {
            self.handle_ctrl(f.subtype, &payload);
        } else {
            self.handle_data(f.subtype, &payload);
        }
    }

    fn handle_ctrl(&mut self, subtype: u8, payload: &[u8]) {
        match subtype {
            ftype::NEGOTIATE => {
                let mode = payload.first().copied().unwrap_or(0);
                if mode != 0 {
                    log::warn!(
                        "blufi security mode {mode} != 0 (L0); 解密未实现，按明文处理"
                    );
                }
            }
            ftype::GET_WIFI_LIST => {
                self.state = BluFiState::Scanning;
                match self.wifi.scan() {
                    Ok(mut items) => {
                        // 按 RSSI 降序（i8 数值越大=信号越强），使单帧截断时保留最强热点。
                        // 当前对端 APP 不支持 BluFi 分片（收到 FRAGMENTED 帧会 StopNotify 断开），
                        // 而 BluFi 帧 data_len 为 u8（≤255）无法单帧承载大列表，故把列表压进单帧：
                        // 编码上限 255，超出则丢弃最弱热点（已排序，前缀即最强）。
                        // APP 支持分片后，改回 encode_scan_list(&items) 全量即可，send_data
                        // 会自动按 255 切片（每片 data ≤ 255，data_len 字段不截断）。
                        items.sort_by(|a, b| b.rssi.cmp(&a.rssi));
                        // 列表超 255 字节时由 send_data 自动按 BluFi 分片（每片 ≤255、
                        // data_len 不截断），对端 BlufiClient 支持分片重组 → **不丢热点**。
                        // 仅当对端异常（收到分片即断连）时，才把 `scan_single_frame_bytes`
                        // 设为 255 退化为单帧丢弃最弱热点。
                        let data = match self.cfg.scan_single_frame_bytes {
                            0 => encode_scan_list(&items),
                            max_bytes => {
                                let full_len = encode_scan_list(&items).len();
                                let d = encode_scan_list_fitting(&items, max_bytes);
                                if d.len() < full_len {
                                    log::warn!(
                                        "blufi: scan list {} items ({}B) exceeds single-frame limit {}B; \
                                         truncated to {}B (dropped weakest)",
                                        items.len(), full_len, max_bytes, d.len()
                                    );
                                }
                                d
                            }
                        };
                        self.send_data(PKG_DATA, ftype::WIFI_LIST, &data);
                    }
                    Err(e) => {
                        log::warn!("blufi wifi scan failed: {e}");
                        // 0x12 REPORT_ERROR, data=[0x0b 扫描失败]
                        self.send_frame(PKG_DATA, ftype::REPORT_ERROR, fc::DIRECTION, &[0x0b]);
                    }
                }
                self.state = BluFiState::WaitApp;
            }
            _ => {}
        }
    }

    fn handle_data(&mut self, subtype: u8, payload: &[u8]) {
        match subtype {
            ftype::NEG_DATA => {
                // v1 L0 忽略协商数据
            }
            ftype::CUSTOM_DATA => {
                match parse_provisioning(payload) {
                    Ok(prov) => {
                        // 按需求打印明文密码（含敏感信息，日志外发前请脱敏）
                        log::info!(
                            "blufi: provisioning request ssid={} pwd={} ip={:?} port={:?}",
                            prov.ssid, prov.pwd, prov.ip, prov.port
                        );
                        // B2：回执 ×N（同一 sequence，间隔 ack_interval_ms）
                        log::info!(
                            "blufi: → app 回执 ×{}: \"{}\"",
                            self.cfg.ack_repeat.max(1),
                            String::from_utf8_lossy(RECEIVED_MSG)
                        );
                        self.send_repeat(ftype::CUSTOM_DATA, RECEIVED_MSG);
                        self.state = BluFiState::Connecting;
                        let pwd = if prov.pwd.is_empty() {
                            None
                        } else {
                            Some(prov.pwd.as_str())
                        };
                        let result = self.wifi.connect(&prov.ssid, pwd);
                        // 配网结束：把配置文件（含路径）打出来便于排查；psk 明文已打码
                        log::info!(
                            "blufi: 配置文件 {} 内容:\n{}",
                            self.wifi.conf_path(),
                            self.wifi.dump_conf()
                        );
                        match result {
                            Ok(bssid) => {
                                // 0xF 仅 2 字节 [opmode=STA, state=已连有IP]（对齐 ESP32 参考实现）
                                let st = encode_connect_state(0x00);
                                log::info!(
                                    "blufi: → app 0xF 状态报告 data={:02x?} (opmode=STA, state=已连有IP)",
                                    st
                                );
                                self.send_data(PKG_DATA, ftype::CONNECT_STATE, &st);
                                log::info!(
                                    "blufi: provisioning SUCCESS ssid={} bssid={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} — 已回 0xF(2B, state=已连有IP)",
                                    prov.ssid, bssid[0], bssid[1], bssid[2], bssid[3], bssid[4],
                                    bssid[5]
                                );
                                let _ = self.event_tx.send(BlufiEvent::WifiConnected {
                                    ssid: prov.ssid.clone(),
                                });
                                self.state = BluFiState::Finish;
                            }
                            Err(e) => {
                                log::warn!(
                                    "blufi: provisioning FAILED ssid={} reason={} — 回 \"Wifi connection failed\" ×{}",
                                    prov.ssid, e, self.cfg.ack_repeat.max(1)
                                );
                                log::warn!(
                                    "blufi: → app 失败文本 ×{}: \"{}\"",
                                    self.cfg.ack_repeat.max(1),
                                    String::from_utf8_lossy(FAILED_MSG)
                                );
                                // 失败文本 ×N（同一 sequence）
                                self.send_repeat(ftype::CUSTOM_DATA, FAILED_MSG);
                                let _ = self.event_tx.send(BlufiEvent::WifiFailed {
                                    reason: e.to_string(),
                                });
                                self.state = BluFiState::Failed;
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!("blufi provisioning parse failed: {e}");
                        self.send_frame(PKG_DATA, ftype::CUSTOM_DATA, fc::DIRECTION, FAILED_MSG);
                        self.state = BluFiState::WaitApp;
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};

    /// 用 StubBleLink 模拟 APP 下发一帧配网，验证 worker 走通「回执 → 连接状态」路径。
    #[test]
    fn worker_handles_custom_data() {
        let cfg = BluFiConfig::default();
        let (cmd_tx, cmd_rx) = mpsc::channel::<BlufiCmd>();
        let (ev_tx, ev_rx) = mpsc::channel::<BlufiEvent>();
        let link = Box::new(StubBleLink::new());

        // 构造 APP 下发的 CUSTOM_DATA 帧（SSID/PWD 明文）
        let prov_raw = b"SSID:testnet,PWD:secret123";
        let frame = BluFiFrame::new(PKG_DATA, ftype::CUSTOM_DATA, 0, 1, prov_raw.to_vec());
        link.push_incoming(frame.encode());

        let _h = BluFiWorker::spawn_with_link(cfg, "Gtest".into(), cmd_rx, ev_tx, link);

        // 命令通道关闭即触发 worker 退出；这里直接关闭 cmd 通道
        drop(cmd_tx);

        // 至少应能收到 EnterConfigMode（worker 启动即发）
        let ev = ev_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(ev, BlufiEvent::EnterConfigMode));
        // 注意：因 StubBleLink 不真正执行 wpa_cli（无网络），connect 会失败，
        // 本测试仅验证帧处理不 panic、事件通道可用。
    }

    /// APP 重连后，设备发送 sequence 应按 §2「重连清零」重置：
    /// 版本帧（会话首帧 seq=0）之后，重连后第一个响应帧必须是 seq=1，
    /// 否则 APP 会把它当序号跳跃（丢帧/重放）而报错。
    #[test]
    fn reconnect_resets_tx_sequence() {
        struct ReconnectLink {
            sent: Arc<Mutex<Vec<Vec<u8>>>>,
            new_session: Arc<AtomicBool>,
        }
        impl BleLink for ReconnectLink {
            fn send(&self, bytes: &[u8]) -> Result<(), BleLinkError> {
                self.sent.lock().unwrap().push(bytes.to_vec());
                Ok(())
            }
            fn try_recv(&self) -> Result<Option<Vec<u8>>, BleLinkError> {
                Ok(None)
            }
            fn take_new_session(&self) -> bool {
                self.new_session.swap(false, Ordering::SeqCst)
            }
        }

        let cfg = BluFiConfig::default();
        let wifi = WifiManager::new(&cfg);
        let (_cmd_tx, cmd_rx) = mpsc::channel::<BlufiCmd>();
        let (ev_tx, _ev_rx) = mpsc::channel::<BlufiEvent>();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let flag = Arc::new(AtomicBool::new(false));

        let mut ctx = WorkerCtx {
            cfg,
            device_id: "Gtest".into(),
            cmd_rx,
            event_tx: ev_tx,
            link: Box::new(ReconnectLink {
                sent: sent.clone(),
                new_session: flag.clone(),
            }),
            asm: FragmentAssembler::new(),
            seq_out: 5, // 模拟重连前已累计发出多帧
            state: BluFiState::WaitApp,
            wifi,
        };

        // 未重连：seq 延续 5
        ctx.handle_new_session();
        assert_eq!(ctx.seq_out, 5);

        // 标记新会话（APP 重连）后清零到版本帧之后的 1
        flag.store(true, Ordering::SeqCst);
        ctx.handle_new_session();
        assert_eq!(ctx.seq_out, SEQ_AFTER_VERSION);

        // 实际发出的一帧，帧内 sequence 字段（第 3 字节）应为 1
        ctx.send_frame(PKG_DATA, ftype::CUSTOM_DATA, fc::DIRECTION, b"x");
        let sent = sent.lock().unwrap();
        let frame = sent.last().expect("应发出一帧");
        assert_eq!(frame[2], 1, "重连后第一帧 seq 应为 1，实际 {}", frame[2]);
    }

    /// 连发 N 帧（回执 / 失败文本）**sequence 必须递增**：
    /// 库 parseNotification 严格校验 read sequence，重复 seq 会丢弃该帧并让后续帧错位。
    #[test]
    fn repeat_frames_increment_sequence() {
        struct RecLink {
            sent: Arc<Mutex<Vec<Vec<u8>>>>,
        }
        impl BleLink for RecLink {
            fn send(&self, bytes: &[u8]) -> Result<(), BleLinkError> {
                self.sent.lock().unwrap().push(bytes.to_vec());
                Ok(())
            }
            fn try_recv(&self) -> Result<Option<Vec<u8>>, BleLinkError> {
                Ok(None)
            }
        }

        let mut cfg = BluFiConfig::default();
        cfg.ack_repeat = 3;
        cfg.ack_interval_ms = 0; // 测试不等待
        let wifi = WifiManager::new(&cfg);
        let (_cmd_tx, cmd_rx) = mpsc::channel::<BlufiCmd>();
        let (ev_tx, _ev_rx) = mpsc::channel::<BlufiEvent>();
        let sent = Arc::new(Mutex::new(Vec::new()));

        let mut ctx = WorkerCtx {
            cfg,
            device_id: "Gtest".into(),
            cmd_rx,
            event_tx: ev_tx,
            link: Box::new(RecLink { sent: sent.clone() }),
            asm: FragmentAssembler::new(),
            seq_out: 5,
            state: BluFiState::WaitApp,
            wifi,
        };

        ctx.send_repeat(ftype::CUSTOM_DATA, RECEIVED_MSG);

        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 3, "应连发 3 帧");
        // seq 必须逐帧递增：库按 mReadSequence.incrementAndGet() 严格校验
        assert_eq!(sent[0][2], 5);
        assert_eq!(sent[1][2], 6, "seq 必须递增，重复 seq 会被库丢弃");
        assert_eq!(sent[2][2], 7);
        assert_eq!(ctx.seq_out, 8, "3 帧后 seq 应为 8");
    }

    #[test]
    fn bluetooth_name_format() {
        // name_prefix 留空 → 跟随 model
        let cfg = BluFiConfig::default();
        assert_eq!(cfg.bluetooth_name("M1S", "Ge33700a6620dfddc"), "M1S-Ge33700a6620dfddc");
        // name_prefix 显式指定 → 用 name_prefix（覆盖 model）
        let cfg2 = BluFiConfig {
            name_prefix: "M2".into(),
            ..Default::default()
        };
        assert_eq!(cfg2.bluetooth_name("M1S", "Ge33700a6620dfddc"), "M2-Ge33700a6620dfddc");
    }
}
