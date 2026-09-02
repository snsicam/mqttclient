//! 8 个业务模块 + AppModule 聚合（LLD-003 §7.3/§7.5）。
//!
//! AppModule 实现 myrtio-mqtt 的 `MqttModule`：
//! - `on_start`：发布 login；
//! - `on_message`：解析下行包 → 业务处理 / 投递 Dispatcher；
//! - `on_tick`：消费事件、心跳/周期状态上报、FIFO 出队发布；
//! - `needs_immediate_publish`：有事件/待发包时立即发布。

use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use embassy_time::Duration;
use myrtio_mqtt::runtime::{MqttModule, Publish, PublishOutbox, TopicCollector};
use myrtio_mqtt::QoS;

use crate::app_state::AppState;
use crate::config::AppConfig;
use crate::downlink::DownlinkCmd;
use crate::protocol::{self, DownlinkMsg, UplinkMsg};
use crate::state::{ConnState, ConnStateMachine, Event, FifoItem, UplinkFifo};

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
    conn: ConnStateMachine,
    fifo: UplinkFifo,
    mods: BizModules,
    force_publish: bool,
}

impl AppModule {
    pub fn new(
        cfg: AppConfig,
        state: Arc<Mutex<AppState>>,
        event_rx: Arc<Mutex<mpsc::Receiver<Event>>>,
        cmd_tx: mpsc::Sender<DownlinkCmd>,
    ) -> Self {
        Self {
            cfg,
            state,
            event_rx,
            cmd_tx,
            conn: ConnStateMachine::default(),
            fifo: UplinkFifo::new(UPLINK_FIFO_CAP),
            mods: BizModules::default(),
            force_publish: false,
        }
    }

    fn enqueue(&mut self, priority: u8, payload: Vec<u8>) {
        self.fifo.push(FifoItem { priority, payload });
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
                }
                Event::MrDisconnected => {
                    self.state.lock().unwrap().moonraker_connected = false;
                }
                Event::GcodeResult { cmd_type, result } => {
                    self.enqueue(0, UplinkMsg::gcode_reply(&self.cfg.device.id, &cmd_type, &result, now));
                }
                Event::DownloadFinished { file_type, file_name, err_code } => {
                    let st = if err_code == 0 { "OK" } else { "ERROR" };
                    self.enqueue(0, UplinkMsg::download_report(&self.cfg.device.id, "download_end", &file_name, file_type, st, err_code, now));
                }
                Event::FileListResult { files } => {
                    let total = files.len();
                    if total == 0 {
                        self.enqueue(0, UplinkMsg::file_list_reply(&self.cfg.device.id, 0, 0, &[], now));
                    } else {
                        for (i, chunk) in files.chunks(10).enumerate() {
                            self.enqueue(0, UplinkMsg::file_list_reply(&self.cfg.device.id, total, i, chunk, now));
                        }
                    }
                }
                Event::Alarm { err_type, err_msg } => {
                    self.enqueue(0, UplinkMsg::alarm(&self.cfg.device.id, err_type, &err_msg, now));
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
                self.mods.download.active = Some((ft, fname.clone()));
                if let Err(e) = self.cmd_tx.send(DownlinkCmd::DownloadBegin {
                    file_type: ft,
                    file_name: fname,
                    url: msg.url.clone(),
                    server_ip: msg.server_ip.clone(),
                }) {
                    log::error!("dispatcher send failed: {e}");
                }
            }
            "download_end" => {
                // 服务器 ack（ackState）：下载流程结束
                self.mods.download.active = None;
            }
            "upgrade_query" => {
                self.mods.upgrade.last_query_ts = now;
                let _ = self.cmd_tx.send(DownlinkCmd::UpgradeQuery);
            }
            "file_list" => {
                let _ = self.cmd_tx.send(DownlinkCmd::ListFiles);
            }
            "server_unbind" => {
                self.mods.unbind.pending = true;
                let _ = self.cmd_tx.send(DownlinkCmd::ServerUnbind);
            }
            "device_unbind" => {
                self.conn.bound = false;
                self.mods.unbind.pending = false;
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

        // 2. login 回复超时：标记错误（由外层 keepalive 断线驱动重连）
        if self.conn.login_reply_timeout(now) {
            log::warn!("login reply timeout");
            self.conn.on_error();
        }

        // 3. 心跳：Ready 且 3min 无下行 → 状态上报
        if self.conn.heartbeat_due(now) {
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
        if self.conn.bound {
            let topic = self.cfg.up_topic();
            while let Some(item) = self.fifo.pop() {
                self.publish_dbg(outbox, &topic, &item.payload);
            }
            self.force_publish = false;
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
        };
        let (event_tx, event_rx) = mpsc::channel::<Event>();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<DownlinkCmd>();
        let state = Arc::new(Mutex::new(AppState::default()));
        let m = AppModule::new(cfg, state, Arc::new(Mutex::new(event_rx)), cmd_tx);
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
        m.enqueue(0, b"{\"type\":\"alarm\"}".to_vec());
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
}
