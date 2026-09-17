//! 8 个业务模块 + AppModule 聚合（LLD-003 §7.3/§7.5）。
//!
//! AppModule 实现 myrtio-mqtt 的 `MqttModule`：
//! - `on_start`：发布 login；
//! - `on_message`：解析下行包 → 业务处理 / 投递 Dispatcher；
//! - `on_tick`：消费事件、心跳/周期状态上报、FIFO 出队发布；
//! - `needs_immediate_publish`：有事件/待发包时立即发布。

use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use serde_json::json;

use embassy_time::Duration;
use myrtio_mqtt::runtime::{MqttModule, Publish, PublishOutbox, TopicCollector};
use myrtio_mqtt::QoS;

use crate::app_state::AppState;
use crate::config::AppConfig;
use crate::downlink::{DownlinkCmd, UiReply};
use crate::protocol::{self, DownlinkMsg, UplinkMsg};
use crate::state::{ConnState, ConnStateMachine, Event, FifoItem, MAX_LOGIN_ATTEMPTS, SharedUiState, UplinkFifo, UNBOUND_RETRY_INTERVAL_SECS};

const UPLINK_FIFO_CAP: usize = 10;
const TICK_INTERVAL: Duration = Duration::from_millis(500);
const STATUS_IDLE_INTERVAL_SECS: u64 = 5 * 60;
const STATUS_BUSY_INTERVAL_SECS: u64 = 10;

// ---------------------------------------------------------------------------
// 8 个业务子模块（各持有自身状态）
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct LoginModule {
    pub reply_received: bool,
    pub bind_state: u8,
    pub account: String,
}

#[derive(Default)]
pub struct StatusModule {
    /// Moonraker 状态更新事件待处理。
    pub dirty: bool,
}

#[derive(Default)]
pub struct GcodeModule {
    /// 待回复的 cmdType。
    pub pending_cmd_type: Option<String>,
}

#[derive(Default)]
pub struct DownloadModule {
    /// 进行中的下载 (fileType, fileName)。
    pub active: Option<(u8, String)>,
}

#[derive(Default)]
pub struct UpgradeModule {
    pub last_query_ts: u64,
    /// UI 经 UDS 发起 upgrade_query 后暂存回复通道，待服务器下行回复时回填（见 `upgrade_query` 下行处理）。
    pub pending_reply: Option<mpsc::Sender<UiReply>>,
    /// 每次 UI 发起 upgrade_query 自增的序号。下行回复到达时把 `result_seq` 置为当前 `query_seq`，
    /// 用以在「下行先于事件被排空」的竞态下仍能闭环（见 `Event::UpgradeQueryRequested` 与 `upgrade_query` 下行处理）。
    pub query_seq: u64,
    /// 下行回复所应答的查询序号；为 0 表示尚无回复。事件侧据此判断是否已有结果可立即回包。
    pub result_seq: u64,
}

#[derive(Default)]
pub struct FileListModule {
    pub total: usize,
}

#[derive(Default)]
pub struct UnbindModule {
    pub pending: bool,
}

#[derive(Default)]
pub struct AlarmModule {
    pub last_err_type: u8,
    pub last_ts: u64,
}

/// 聚合 8 个子模块。
#[derive(Default)]
pub struct BizModules {
    pub login: LoginModule,
    pub status: StatusModule,
    pub gcode: GcodeModule,
    pub download: DownloadModule,
    pub upgrade: UpgradeModule,
    pub filelist: FileListModule,
    pub unbind: UnbindModule,
    pub alarm: AlarmModule,
}

// ---------------------------------------------------------------------------
// AppModule
// ---------------------------------------------------------------------------

pub struct AppModule {
    cfg: AppConfig,
    state: Arc<Mutex<AppState>>,
    event_rx: Arc<Mutex<mpsc::Receiver<Event>>>,
    cmd_tx: mpsc::Sender<DownlinkCmd>,
    ui_state: SharedUiState,
    conn: ConnStateMachine,
    fifo: UplinkFifo,
    mods: BizModules,
    force_publish: bool,
    /// 解绑上行绕过"未绑定不发布"守卫（见 Event::UiUnbind 处理）。
    force_unbind_publish: bool,
}

impl AppModule {
    pub fn new(
        cfg: AppConfig,
        state: Arc<Mutex<AppState>>,
        event_rx: Arc<Mutex<mpsc::Receiver<Event>>>,
        cmd_tx: mpsc::Sender<DownlinkCmd>,
        ui_state: SharedUiState,
    ) -> Self {
        Self {
            cfg,
            state,
            event_rx,
            cmd_tx,
            ui_state,
            conn: ConnStateMachine::default(),
            fifo: UplinkFifo::new(UPLINK_FIFO_CAP),
            mods: BizModules::default(),
            force_publish: false,
            force_unbind_publish: false,
        }
    }

    fn enqueue(&mut self, payload: Vec<u8>) {
        self.fifo.push(FifoItem { payload });
        self.force_publish = true;
    }

    /// 发布并记录调试日志（topic + payload 文本）。
    fn publish_dbg(&self, outbox: &mut dyn PublishOutbox, topic: &str, payload: &[u8]) {
        log::info!("UP   topic={topic} qos=AtLeastOnce payload={}",
            String::from_utf8_lossy(payload));
        outbox.publish(topic, payload, QoS::AtLeastOnce);
    }

    fn publish_status_packs(&mut self, outbox: &mut dyn PublishOutbox) {
        let topic = self.cfg.up_topic();
        let s = self.state.lock().unwrap().clone();
        let now = protocol::now_ts();
        let id = self.cfg.device.id.as_str();
        let packs = [
            UplinkMsg::status_hardware(id, &s, now),
            UplinkMsg::status_temp_fan(id, &s, now),
            UplinkMsg::status_level(id, &s, now),
            UplinkMsg::status_print(id, &s, now),
        ];
        for p in packs {
            self.publish_dbg(outbox, &topic, &p);
        }
    }

    fn drain_events(&mut self) {
        let now = protocol::now_ts();
        // 先批量取出事件（释放锁），再处理，避免与 &mut self 冲突
        let mut events: Vec<Event> = Vec::new();
        {
            let rx = self.event_rx.lock().unwrap();
            while let Ok(ev) = rx.try_recv() {
                events.push(ev);
            }
        }
        for ev in events {
            match ev {
                Event::StatusUpdated => {
                    self.mods.status.dirty = true;
                    self.force_publish = true;
                }
                Event::MrConnected => {
                    self.state.lock().unwrap().moonraker_connected = true;
                    self.ui_state.lock().unwrap().moonraker_connected = true;
                }
                Event::MrDisconnected => {
                    self.state.lock().unwrap().moonraker_connected = false;
                    self.ui_state.lock().unwrap().moonraker_connected = false;
                }
                Event::GcodeResult { cmd_type, result } => {
                    self.enqueue( UplinkMsg::gcode_reply(&self.cfg.device.id, &cmd_type, &result, now));
                }
                Event::UpgradeQueryRequested { reply_tx } => {
                    // UI 经 UDS 发起固件查询：发布 upgrade_query 上行，并暂存回复通道待服务器回包。
                    self.mods.upgrade.query_seq += 1;
                    let seq = self.mods.upgrade.query_seq;
                    self.mods.upgrade.pending_reply = reply_tx;
                    self.enqueue(UplinkMsg::upgrade_query(&self.cfg.device.id, now));
                    // 竞态补偿：云端可能在 `drain_events` 把 pending_reply 设好之前就回了下行
                    // （本地/测试 broker 常同毫秒应答）。若下行已先到（result_seq 命中本序号），
                    // 直接拿 UiState 里的固件信息回包，避免一次性通道被丢弃导致 UI 永久超时。
                    if self.mods.upgrade.result_seq == seq {
                        if let Some(tx) = self.mods.upgrade.pending_reply.take() {
                            let ui = self.ui_state.lock().unwrap();
                            let mcu = if ui.firmware_mcu_file == "NA" { "" } else { ui.firmware_mcu_file.as_str() };
                            let esp = if ui.firmware_esp_file == "NA" { "" } else { ui.firmware_esp_file.as_str() };
                            let _ = tx.send(UiReply::Ok(json!({
                                "server_ip": ui.firmware_server_ip,
                                "mcu_file": mcu,
                                "esp_file": esp,
                            })));
                        }
                    }
                }
                Event::DownloadStarted { file_type, file_name } => {
                    // 协议 §5.5：设备→服务器 `download_begin`（受理 transState=OK / errCode=0）。
                    self.enqueue( UplinkMsg::download_report(&self.cfg.device.id, "download_begin", &file_name, file_type, "OK", 0, now));
                }
                Event::DownloadFinished { file_type, file_name, err_code, from_ui } => {
                    // 忙闲标志归属（AV-1）：MQTT 触发（from_ui=false）在此清位；UDS 触发
                    // （from_ui=true）由 UDS 线程在回包/超时后清位，避免双重清位竞争。
                    if !from_ui {
                        self.ui_state.lock().unwrap().downloading = false;
                        self.mods.download.active = None;
                    }
                    // 无论来源均上报 download_end（两条路径 begin→end 配对）。
                    // 协议 errCode 仅定义 0/1/2；内部扩展值（3=传输超限、4=不支持 file_type）
                    // 统一映射为 1（协议内"错误"），严格符合字段范围（MXS V2.1.2 §5.6）。
                    let err_code = match err_code { 0 => 0, 1 => 1, 2 => 2, _ => 1 };
                    let st = if err_code == 0 { "OK" } else { "ERROR" };
                    self.enqueue( UplinkMsg::download_report(&self.cfg.device.id, "download_end", &file_name, file_type, st, err_code, now));
                }
                Event::UiUnbind => {
                    // UI 经 UDS 发起的解绑：组 device_unbind 上行并强制发布（绕过"未绑定不发布"守卫）。
                    self.enqueue(UplinkMsg::device_unbind(&self.cfg.device.id, now));
                    self.force_unbind_publish = true;
                    // 本地即时把 UI 可见状态置为「未绑定」：不等待云端 device_unbind 回显
                    // （未绑定/未录入设备点击解绑后云端不会回显，否则 bind_status 仍返回旧态）。
                    {
                        let mut ui = self.ui_state.lock().unwrap();
                        ui.bind_state = 1; // 1=未绑定
                        ui.account.clear();
                    }
                }
                Event::FileListResult { files } => {
                    let total = files.len();
                    if total == 0 {
                        self.enqueue( UplinkMsg::file_list_reply(&self.cfg.device.id, 0, 0, &[], now));
                    } else {
                        for (i, chunk) in files.chunks(10).enumerate() {
                            self.enqueue( UplinkMsg::file_list_reply(&self.cfg.device.id, total, i, chunk, now));
                        }
                    }
                }
                Event::Alarm { err_type, err_msg } => {
                    self.enqueue( UplinkMsg::alarm(&self.cfg.device.id, err_type, &err_msg, now));
                }
            }
        }
    }

    fn handle_downlink(&mut self, msg: &DownlinkMsg) {
        let now = protocol::now_ts();
        self.conn.on_downlink(now);
        match msg.kind.as_str() {
            "login" => {
                let bind = msg.bind_state.unwrap_or(0);
                self.conn.on_login_reply(bind, now);
                self.mods.login.reply_received = true;
                self.mods.login.bind_state = bind;
                self.mods.login.account = msg.account.clone().unwrap_or_default();
                // 同步 UI 可见绑定状态
                {
                    let mut ui = self.ui_state.lock().unwrap();
                    ui.bind_state = bind;
                    ui.account = self.mods.login.account.clone();
                }
                // 协议：bindState 0=已绑定 / 1=未绑定 / 2=序列号未录入（与 ConnStateMachine::bound 一致）
                if bind == 0 {
                    log::info!("login ok, bound");
                    self.force_publish = true;
                } else {
                    // 0 以外均视为未完成绑定：不上报状态，等待平台侧录入/绑定
                    log::warn!("login reply: not bound (bindState={bind})");
                }
            }
            "status_query" => {
                self.force_publish = true;
            }
            "gcode" => {
                let cmd_type = msg.gcode_cmd.clone().unwrap_or_default();
                let raw = cmd_type.clone();
                self.mods.gcode.pending_cmd_type = Some(cmd_type.clone());
                if let Err(e) = self.cmd_tx.send(DownlinkCmd::Gcode { cmd_type, raw }) {
                    log::error!("dispatcher send failed: {e}");
                }
            }
            "download_begin" => {
                let ft = msg.file_type.unwrap_or(0);
                let fname = msg.file_name.clone().unwrap_or_default();
                // 协议 §5.5：文件下载由服务器发起，设备校验后**必须回 `download_begin`**
                // （transState/errCode）：忙 → ERROR/1；受理 → OK/0。
                // 忙闲守卫（AV-1）：与 UDS `download` 共用 `UiState.downloading`。
                let busy = {
                    let mut ui = self.ui_state.lock().unwrap();
                    if ui.downloading {
                        true
                    } else {
                        ui.downloading = true;
                        false
                    }
                };
                if busy {
                    log::warn!("download_begin rejected: another download in progress");
                    self.enqueue( UplinkMsg::download_report(&self.cfg.device.id, "download_begin", &fname, ft, "ERROR", 1, now));
                    return;
                }
                self.mods.download.active = Some((ft, fname.clone()));
                // 受理即回 download_begin(transState=OK, errCode=0)（协议 §5.5）
                self.enqueue( UplinkMsg::download_report(&self.cfg.device.id, "download_begin", &fname, ft, "OK", 0, now));
                let fname_for_err = fname.clone();
                if let Err(e) = self.cmd_tx.send(DownlinkCmd::DownloadBegin {
                    file_type: ft,
                    file_name: fname,
                    url: msg.url.clone(),
                    server_ip: msg.server_ip.clone(),
                }) {
                    log::error!("dispatcher send failed: {e}");
                    // 入队失败：清位并回 ERROR，避免卡死后续下载。
                    self.ui_state.lock().unwrap().downloading = false;
                    self.mods.download.active = None;
                    self.enqueue( UplinkMsg::download_report(&self.cfg.device.id, "download_begin", &fname_for_err, ft, "ERROR", 1, now));
                }
            }
            "download_end" => {
                // 服务器 ack（ackState）：下载流程结束
                self.mods.download.active = None;
            }
            "upgrade_query" => {
                // 服务器对升级查询的**回复**（含 serverIp / mcuFile / espFile），非发起查询。
                // 捕获固件信息到 UiState，并回填等待中的 UDS 查询结果。
                let server_ip = msg.server_ip.clone().unwrap_or_default();
                let mcu = msg.mcu_file.clone().unwrap_or_default();
                let esp = msg.esp_file.clone().unwrap_or_default();
                {
                    let mut ui = self.ui_state.lock().unwrap();
                    ui.firmware_server_ip = server_ip.clone();
                    ui.firmware_mcu_file = mcu.clone();
                    ui.firmware_esp_file = esp.clone();
                }
                // 标记本下行回复所应答的查询序号（命中当前在途查询）；若无等待中的 pending_reply，
                // 仅置位 result_seq，由 `Event::UpgradeQueryRequested` 在事件被排空时立即回包（竞态补偿）。
                self.mods.upgrade.result_seq = self.mods.upgrade.query_seq;
                // 若有等待中的 UI 查询请求，回传固件信息（mcuFile/espFile 为 "NA" 表示无对应固件）。
                if let Some(tx) = self.mods.upgrade.pending_reply.take() {
                    let _ = tx.send(UiReply::Ok(json!({
                        "server_ip": server_ip,
                        "mcu_file": if mcu == "NA" { "" } else { mcu.as_str() },
                        "esp_file": if esp == "NA" { "" } else { esp.as_str() },
                    })));
                }
            }
            "file_list" => {
                let _ = self.cmd_tx.send(DownlinkCmd::ListFiles);
            }
            "server_unbind" => {
                // 云端发起解绑：本地绑定态需同步清零（与 device_unbind 一致），
                // 否则 UDS `bind_status` 仍读旧 ui_state，UI 一直显示「已绑定」。
                self.conn.bound = false;
                self.mods.unbind.pending = true;
                {
                    let mut ui = self.ui_state.lock().unwrap();
                    ui.bind_state = 1; // 1=未绑定
                    ui.account.clear();
                }
                let _ = self.cmd_tx.send(DownlinkCmd::ServerUnbind);
            }
            "device_unbind" => {
                self.conn.bound = false;
                self.mods.unbind.pending = false;
                // 同步 UI 可见绑定状态（与 login 回复一致）：解绑后应为「未绑定」，
                // 否则 UDS `bind_status` 仍读旧 ui_state，UI 会一直显示「已绑定」。
                {
                    let mut ui = self.ui_state.lock().unwrap();
                    ui.bind_state = 1; // 1=未绑定
                    ui.account.clear();
                }
            }
            other => {
                log::debug!("unknown downlink kind: {other}");
            }
        }
    }
}

impl MqttModule for AppModule {
    fn register(&self, collector: &mut dyn TopicCollector) {
        collector.add(&self.cfg.down_topic());
    }

    fn on_start(&mut self, outbox: &mut dyn PublishOutbox) {
        let now = protocol::now_ts();
        self.conn.on_connect(now);
        self.ui_state.lock().unwrap().cloud_connected = true;
        let ip = local_ip().unwrap_or_default();
        let payload = UplinkMsg::login(&self.cfg.device, &ip, now);
        let topic = self.cfg.up_topic();
        self.publish_dbg(outbox, &topic, &payload);
        self.conn.on_login_sent(now);
        log::info!("login published (attempt {})", self.conn.login_attempts);
    }

    fn on_message(&mut self, message: &Publish<'_>) {
        log::info!("DOWN topic={} qos={:?} payload={}",
            message.topic,
            message.qos,
            String::from_utf8_lossy(message.payload));
        match DownlinkMsg::parse(message.payload) {
            Ok(msg) => self.handle_downlink(&msg),
            Err(e) => log::warn!("bad downlink payload: {e}"),
        }
    }

    fn on_tick(&mut self, outbox: &mut dyn PublishOutbox) -> Duration {
        let now = protocol::now_ts();

        // 1. 消费事件
        self.drain_events();

        // 2. login 回复超时处理：连接仍存活则主动重发 login（给平台第二次机会，
        //    避免仅靠 keepalive 断线才重登导致期间不登录）；重试超限再标记错误由断线重连。
        if self.conn.login_reply_timeout(now) {
            if self.conn.login_attempts >= MAX_LOGIN_ATTEMPTS {
                log::warn!("login reply timeout after {} attempts, force reconnect", self.conn.login_attempts);
                self.conn.on_error();
            } else {
                log::warn!("login reply timeout, resend login (attempt {})", self.conn.login_attempts + 1);
                let ip = local_ip().unwrap_or_default();
                let payload = UplinkMsg::login(&self.cfg.device, &ip, now);
                let topic = self.cfg.up_topic();
                self.publish_dbg(outbox, &topic, &payload);
                self.conn.on_login_sent(now);
            }
        }

        // 2.5 未绑定轮询：已 Ready 但平台未绑定（bindState≠0），每 5s 重发 login，
        //     直到平台侧完成绑定（下次 login 回复 bindState=0）。
        //     与 login_reply_timeout 区分：此处连接健康、已收到回复，仅因未绑定而轮询；
        //     不计入 login_attempts / should_reconnect，不会触发断线重连。
        if self.conn.state == ConnState::Ready && !self.conn.bound {
            if now.saturating_sub(self.conn.last_unbound_retry_ts) >= UNBOUND_RETRY_INTERVAL_SECS {
                log::info!("unbound: resend login (polling every {}s)", UNBOUND_RETRY_INTERVAL_SECS);
                let ip = local_ip().unwrap_or_default();
                let payload = UplinkMsg::login(&self.cfg.device, &ip, now);
                let topic = self.cfg.up_topic();
                self.publish_dbg(outbox, &topic, &payload);
                self.conn.last_unbound_retry_ts = now;
            }
        }

        // 3. 心跳：已 login（Ready 且 bound）且 3min 无下行 → 状态上报。
        //    未 login（连接已建立但 login 回复未达 / 未绑定）不报状态包，
        //    必须先完成 login 才允许上报（对齐 FIFO/周期上报的 bound 守卫）。
        if self.conn.state == ConnState::Ready && self.conn.bound && self.conn.heartbeat_due(now) {
            log::info!("heartbeat: no downlink 3min, publish status");
            self.publish_status_packs(outbox);
            self.conn.last_status_publish_ts = now;
            return TICK_INTERVAL;
        }

        // 4. 周期状态上报（忙 10s / 闲 5min）
        if self.conn.state == ConnState::Ready && self.conn.bound {
            let busy = self.state.lock().unwrap().is_busy();
            let interval = if busy { STATUS_BUSY_INTERVAL_SECS } else { STATUS_IDLE_INTERVAL_SECS };
            if now.saturating_sub(self.conn.last_status_publish_ts) >= interval {
                self.publish_status_packs(outbox);
                self.conn.last_status_publish_ts = now;
            }
        }

        // 5. FIFO 出队发布
        //
        // 未绑定（bindState 非 0）时不出队：对齐 m2-software 参考实现
        // （`wifi_send_dev_state_cycle()` 首行 `login_success_flag == false` 即 return，
        //  未绑定时状态包只堆积在 FIFO，不对外发送）。
        // `force_unbind_publish` 例外：UI 经 UDS 发起的解绑包需绕过该守卫强制发出。
        if self.conn.bound || self.force_unbind_publish {
            let topic = self.cfg.up_topic();
            while let Some(item) = self.fifo.pop() {
                self.publish_dbg(outbox, &topic, &item.payload);
            }
            self.force_publish = false;
            self.force_unbind_publish = false;
        }

        TICK_INTERVAL
    }

    fn needs_immediate_publish(&self) -> bool {
        self.force_publish || !self.fifo.is_empty()
    }
}

/// 获取本机局域网 IP（经 UDP 探测技巧，不产生实际流量）。
pub fn local_ip() -> Option<String> {
    use std::net::UdpSocket;
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    s.local_addr().ok().map(|a| a.ip().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 记录所有 publish 调用，供测试断言。
    #[derive(Default)]
    struct Recorder {
        items: Vec<(String, Vec<u8>)>,
    }
    impl PublishOutbox for Recorder {
        fn publish(&mut self, topic: &str, payload: &[u8], _qos: QoS) {
            self.items.push((topic.to_string(), payload.to_vec()));
        }
    }

    fn test_module() -> (AppModule, mpsc::Sender<Event>) {
        let cfg = AppConfig {
            device: crate::config::DeviceConfig { id: "G9".into(), mb: "M".into(), sf1: "1".into(), sf2: "2".into(), wf: "W".into(), lang: 0 },
            mqtt: crate::config::MqttConfig::default(),
            moonraker: crate::config::MoonrakerConfig::default(),
            download: crate::config::DownloadConfig::default(),
            blufi: crate::blufi::BluFiConfig::default(),
            uds: crate::config::UdsConfig::default(),
        };
        let (event_tx, event_rx) = mpsc::channel::<Event>();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<DownlinkCmd>();
        let state = Arc::new(Mutex::new(AppState::default()));
        let ui_state: SharedUiState = Arc::new(Mutex::new(Default::default()));
        let m = AppModule::new(cfg, state, Arc::new(Mutex::new(event_rx)), cmd_tx, ui_state);
        (m, event_tx)
    }

    #[test]
    fn downlink_gcode_forwards() {
        let (mut m, _tx) = test_module();
        let payload = br#"{"type":"gcode","gcodeCmd":"M115"}"#;
        let msg = DownlinkMsg::parse(payload).unwrap();
        m.handle_downlink(&msg);
        assert!(m.mods.gcode.pending_cmd_type.is_some());
        // dispatcher 通道应有待消费命令
        assert!(m.fifo.is_empty());
    }

    #[test]
    fn downlink_login_reply_updates_conn() {
        let (mut m, _tx) = test_module();
        let payload = br#"{"type":"login","bindState":0,"account":"acc"}"#;
        let msg = DownlinkMsg::parse(payload).unwrap();
        m.handle_downlink(&msg);
        assert_eq!(m.conn.state, ConnState::Ready);
        assert!(m.conn.bound);
        assert!(m.force_publish);
    }

    /// 回归：bindState 语义 0=已绑定 / 1=未绑定 / 2=未录入。
    /// 曾误判为 `bind == 1` 才绑定，导致 0 时反而发解绑包。
    #[test]
    fn downlink_login_bindstate_semantics() {
        // bindState=1（未绑定）：状态机进 Ready 但 bound=false，且不触发状态上报
        let (mut m, _tx) = test_module();
        m.handle_downlink(&DownlinkMsg::parse(br#"{"type":"login","bindState":1}"#).unwrap());
        assert_eq!(m.conn.state, ConnState::Ready);
        assert!(!m.conn.bound, "bindState=1 应为未绑定");
        assert!(!m.force_publish, "未绑定时不应触发状态上报");
        assert!(m.fifo.is_empty(), "未绑定时不应发 device_unbind");

        // bindState=2（未录入）：同样未绑定
        let (mut m2, _tx2) = test_module();
        m2.handle_downlink(&DownlinkMsg::parse(br#"{"type":"login","bindState":2}"#).unwrap());
        assert!(!m2.conn.bound, "bindState=2 应为未绑定");
        assert!(m2.fifo.is_empty());
    }

    #[test]
    fn downlink_status_query_forces_publish() {
        let (mut m, _tx) = test_module();
        let msg = DownlinkMsg::parse(br#"{"type":"status_query"}"#).unwrap();
        m.handle_downlink(&msg);
        assert!(m.force_publish);
    }

    #[test]
    fn event_gcode_result_enqueues() {
        let (mut m, tx) = test_module();
        // 通过 Sender 投递事件
        tx.send(Event::GcodeResult { cmd_type: "M115".into(), result: "OK".into() }).unwrap();
        m.drain_events();
        assert!(!m.fifo.is_empty());
        assert!(m.needs_immediate_publish());
    }

    /// 未绑定（bindState 非 0）时不得对外发送状态包与 FIFO 包。
    /// 对齐 m2-software：未绑定时包只留在 FIFO，不 publish。
    #[test]
    fn unbound_device_does_not_publish() {
        let (mut m, _tx) = test_module();
        // 置为未绑定（bindState=1）
        m.handle_downlink(&DownlinkMsg::parse(br#"{"type":"login","bindState":1}"#).unwrap());
        assert!(!m.conn.bound);
        // 入队一个业务包，并触发周期上报时间点
        m.enqueue(b"{\"type\":\"alarm\"}".to_vec());
        m.conn.last_status_publish_ts = 0;

        let mut outbox = Recorder::default();
        m.on_tick(&mut outbox);
        assert!(outbox.items.is_empty(), "未绑定时不应发布任何包");
        assert!(!m.fifo.is_empty(), "未绑定时包应留在 FIFO 中");

        // 绑定后（bindState=0）应恢复发布
        m.handle_downlink(&DownlinkMsg::parse(br#"{"type":"login","bindState":0}"#).unwrap());
        assert!(m.conn.bound);
        let mut outbox2 = Recorder::default();
        m.on_tick(&mut outbox2);
        assert!(!outbox2.items.is_empty(), "绑定后应恢复发布");
    }

    /// 回归：未绑定时必须持续（每 5s）重发 login，直到平台侧绑定完成。
    /// 之前 on_login_reply 一律 state=Ready + 清 login_sent_at_ts，导致 login_reply_timeout
    /// 不再触发，未绑定设备再也不重发 login（丢失「5s 重连/登录」机制）。
    #[test]
    fn unbound_polls_login() {
        let (mut m, _tx) = test_module();
        m.handle_downlink(&DownlinkMsg::parse(br#"{"type":"login","bindState":1}"#).unwrap());
        assert!(!m.conn.bound);
        // 模拟距上次轮询已过去 5s 以上：把基准时间置 0（now 为真实 unix 秒，必然 >=5）
        m.conn.last_unbound_retry_ts = 0;
        let mut outbox = Recorder::default();
        m.on_tick(&mut outbox);
        let has_login = outbox.items.iter().any(|(_t, p)| {
            String::from_utf8_lossy(p).contains("\"type\":\"login\"")
        });
        assert!(has_login, "未绑定时应每 5s 轮询重发 login，实际: {:?}", outbox.items);
    }
}
