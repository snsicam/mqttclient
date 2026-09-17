//! RV1106 MQTT 客户端入口（SPC 阶段 4：编码实现）。
//!
//! 线程模型（LLD-003 §3.2）：
//! - 主线程：`block_on(mqtt_main)` 驱动 MQTT 会话（重连循环）；
//! - moonraker 线程：WS 客户端 + JSON-RPC + 状态桥；
//! - dispatcher 线程：下行命令执行（Moonraker 调用 / HTTP 下载）。

use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::Duration;
use mqtt_client::downlink::{Dispatcher, DownlinkCmd, UiCmd};
use mqtt_client::modules::AppModule;
use mqtt_client::moonraker::MoonrakerWorker;
use mqtt_client::state::{Event, SharedUiState};
use mqtt_client::transport::StdTcpTransport;
use mqtt_client::config::{device_id_from_serial, read_board_serial, PLACEHOLDER_DEVICE_ID};
use mqtt_client::{AppConfig, AppState};
use myrtio_mqtt::runtime::{MqttRuntime, PublishRequest};
use myrtio_mqtt::{LastWill, MqttClient, MqttOptions, QoS};

/// 把蓝牙广播名写入 `/etc/bluetooth/main.conf` 的 `Name` 字段（BlueZ 读取该值作为适配器别名），
/// 使系统侧 bluetoothd 的别名与本应用广播名保持一致。**仅写不读**——本应用广播仍由
/// `gatt::start_gatt` 的 `local_name` 决定，此文件同步只为与系统侧一致。
///
/// 写入策略：已存在未注释的 `Name =` 行则替换其值为最新；否则插入到 `[General]` 段首行之后；
/// 连 `[General]` 段都没有则补一个。文件不存在/不可读/不可写时仅告警，不阻断启动。
fn sync_bluetooth_main_conf(name: &str) {
    const PATH: &str = "/etc/bluetooth/main.conf";
    let content = match std::fs::read_to_string(PATH) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("bluetooth: read {PATH} failed: {e} — skip name sync");
            return;
        }
    };
    let new_line = format!("Name = {name}");

    // 已存在未注释的 Name = 行 → 替换第一处
    if content.lines().any(is_name_assignment) {
        let mut out = String::new();
        let mut replaced = false;
        for line in content.lines() {
            if !replaced && is_name_assignment(line) {
                out.push_str(&new_line);
                out.push('\n');
                replaced = true;
            } else {
                out.push_str(line);
                out.push('\n');
            }
        }
        write_main_conf(PATH, &out, name);
        return;
    }

    // 无 Name = 行：插入到 [General] 段首行之后（bluetoothd 按段解析，必须落在 [General] 内）
    if let Some(pos) = content.lines().position(|l| l.trim() == "[General]") {
        let mut out = String::new();
        for (i, line) in content.lines().enumerate() {
            out.push_str(line);
            out.push('\n');
            if i == pos {
                out.push_str(&new_line);
                out.push('\n');
            }
        }
        write_main_conf(PATH, &out, name);
        return;
    }

    // 连 [General] 段都没有：整文件后补段
    let mut out = String::from(&content);
    if !content.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("[General]\n");
    out.push_str(&new_line);
    out.push('\n');
    write_main_conf(PATH, &out, name);
}

/// 判断一行是否为未注释的 `Name =` 赋值（忽略大小写与空白）。
fn is_name_assignment(line: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with('#') {
        return false;
    }
    match t.strip_prefix("Name") {
        Some(rest) => rest.starts_with('=') || rest.trim_start().starts_with('='),
        None => false,
    }
}

/// 回写 main.conf 并打印结果日志。
fn write_main_conf(path: &str, content: &str, name: &str) {
    if let Err(e) = std::fs::write(path, content) {
        log::warn!("bluetooth: write {path} failed: {e} — skip name sync");
    } else {
        log::info!("bluetooth: synced name {name:?} -> {path}");
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    // 启动即打印版本与编译时间，便于部署后从日志确认烧录的是哪个构建。
    log::info!(
        "mqtt-client v{} (git {}) built at {}",
        env!("CARGO_PKG_VERSION"),
        env!("GIT_HASH"),
        env!("BUILD_TIME")
    );
    // 配置路径解析优先级：命令行参数 > MQTT_CLIENT_CONFIG 环境变量 > 默认相对路径
    let path = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("MQTT_CLIENT_CONFIG").ok())
        .unwrap_or_else(|| "config/mqtt-client.toml".into());
    let path = Path::new(&path);

    // 配置文件不存在时，生成默认配置文件后再加载（便于首次部署开箱即用）
    let mut cfg = if !path.exists() {
        log::warn!("config not found at {path:?}, generating default config");
        let default = AppConfig::default();
        default.save(path).unwrap_or_else(|e| {
            eprintln!("failed to write default config to {path:?}: {e}");
            std::process::exit(1);
        });
        log::info!("default config written to {path:?}, please edit it and restart, or it will be used as-is");
        // 重新读取刚写入的默认文件（确保与磁盘一致，并走标准校验路径）
        AppConfig::load(path).unwrap_or_else(|e| {
            eprintln!("config error: {e}");
            std::process::exit(1);
        })
    } else {
        AppConfig::load(path).unwrap_or_else(|e| {
            eprintln!("config error: {e}");
            eprintln!("  config path tried: {path:?}");
            eprintln!("  usage: mqtt-client [config-path]  (or set MQTT_CLIENT_CONFIG)");
            std::process::exit(1);
        })
    };
    // device.id 自动填充：配置为占位值时，从板载序列号读取（`G` + `/proc/cpuinfo Serial`），
    // 并回写配置文件，便于后续启动直接使用真实 id。
    if cfg.device.id.is_empty() || cfg.device.id == PLACEHOLDER_DEVICE_ID {
        if let Some(serial) = read_board_serial() {
            let new_id = device_id_from_serial(&serial);
            log::warn!(
                "device.id is placeholder {:?}, overriding from board serial: {new_id}",
                cfg.device.id
            );
            cfg.device.id = new_id;
            if let Err(e) = cfg.save(path) {
                log::warn!("failed to persist device.id to {path:?}: {e}");
            }
        } else {
            log::warn!("device.id is placeholder {:?}, but no board serial found", cfg.device.id);
        }
    }
    log::info!("RV1106 MQTT client start: device={} broker={}:{}", cfg.device.id, cfg.mqtt.broker, cfg.mqtt.port);

    // 蓝牙初始化信息（AIC8800 → BlueZ hci0，应用走 D-Bus/GATT）
    mqtt_client::blufi::init::init_bluetooth(&cfg.blufi);

    // 蓝牙配网 worker（阶段 4：真实 GATT 链路）
    // 注意：`blufi_cmd_tx` 必须存活到进程结束——worker 的命令通道靠它保持连接，
    // 一旦发送端被 drop，`cmd_rx.recv_timeout` 会立即返回 Disconnected，worker 在
    // 处理任何 APP 帧之前就退出，导致「手机写了但设备收不到」。故放在 if 块外持有。
    use mqtt_client::blufi::{BluFiWorker, BlufiCmd, BlufiEvent};
    let (blufi_cmd_tx, blufi_cmd_rx) = mpsc::channel::<BlufiCmd>();
    let (blufi_ev_tx, blufi_ev_rx) = mpsc::channel::<BlufiEvent>();
    // 配网成功后触发 MQTT 重连/重登录：blufi 事件线程在 `WifiConnected` 时 `notify_one()`，
    // mqtt 运行循环（run_session）据此丢弃当前会话、用新网络重连并 login（设计见详细设计 §5.5）。
    // 用 `tokio::sync::Notify`（workspace 已启用 sync feature）：无需 sender 保活，blufi 未启用时
    // 无人 notify，`notified()` 永远 pending，不会误触发重连。
    let reconnect_notify = std::sync::Arc::new(tokio::sync::Notify::new());
    let reconnect_notify_clone = reconnect_notify.clone();
    if cfg.blufi.enabled {
        let bt_name = cfg.blufi.bluetooth_name(&cfg.mqtt.model, &cfg.device.id);
        // 把蓝牙广播名同步写入 /etc/bluetooth/main.conf 的 Name 字段，
        // 使系统侧 bluetoothd 的适配器别名与本应用广播名一致（仅写不读）。
        sync_bluetooth_main_conf(&bt_name);
        let link = mqtt_client::blufi::gatt::start_gatt(&cfg.blufi, &bt_name);
        let _blufi = BluFiWorker::spawn_with_link(
            cfg.blufi.clone(),
            cfg.device.id.clone(),
            blufi_cmd_rx,
            blufi_ev_tx,
            link,
        );
        std::thread::spawn(move || {
            for ev in blufi_ev_rx {
                match ev {
                    BlufiEvent::EnterConfigMode => log::info!("blufi: enter config mode"),
                    BlufiEvent::AppConnected => log::info!("blufi: app connected"),
                    BlufiEvent::WifiConnected { ssid } => {
                        log::info!("blufi: wifi connected {ssid} — trigger mqtt reconnect/login");
                        // 仅成功时触发；notify_one 非阻塞、跨线程唤醒 mqtt 运行循环的 select。
                        reconnect_notify_clone.notify_one();
                    }
                    BlufiEvent::WifiFailed { reason } => log::warn!("blufi: wifi failed: {reason}"),
                    BlufiEvent::ExitConfigMode => log::info!("blufi: exit config mode"),
                }
            }
        });
    }
    // 保留 cmd 发送端（后续可下发 StopConfig / 进入配网指令）；不 drop 以免 worker 退出
    let _blufi_cmd = blufi_cmd_tx;

    // 共享状态与通道
    let app_state = Arc::new(Mutex::new(AppState::default()));
    let (event_tx, event_rx) = mpsc::channel::<Event>();
    let (cmd_tx, cmd_rx) = mpsc::channel::<DownlinkCmd>();
    let (ui_cmd_tx, ui_cmd_rx) = mpsc::channel::<UiCmd>();
    let ui_state: SharedUiState = Arc::new(Mutex::new(Default::default()));

    // 独立线程：moonraker 状态服务 / 下行分发
    let mr = MoonrakerWorker::spawn(cfg.moonraker.clone(), app_state.clone(), event_tx.clone());
    Dispatcher::spawn(cfg.clone(), cmd_rx, ui_cmd_rx, mr, event_tx);

    // UI 控制通道（UDS）：klipper_screen 经此下发云侧操作（下载 / 解绑 / 文件列表 / gcode）。
    mqtt_client::uds::spawn(cfg.uds.clone(), ui_cmd_tx, ui_state.clone());

    let event_rx = Arc::new(Mutex::new(event_rx));

    // 进程级 'static 资源：client_id 与 publisher 通道（每次重连复用，避免泄漏）
    // 'a 需为 'static：MqttRuntime 的 client_id 借用与 publisher channel 生命周期必须一致
    let client_id: &'static str = Box::leak(cfg.device.id.clone().into_boxed_str());
    let ch: &'static Channel<CriticalSectionRawMutex, PublishRequest<'static>, 8> = Box::leak(Box::new(Channel::new()));

    futures::executor::block_on(mqtt_main(cfg, app_state, event_rx, cmd_tx, ui_state, client_id, ch, &*reconnect_notify));
}

type PublisherChannel = Channel<CriticalSectionRawMutex, PublishRequest<'static>, 8>;

async fn mqtt_main(
    cfg: AppConfig,
    app_state: Arc<Mutex<AppState>>,
    event_rx: Arc<Mutex<mpsc::Receiver<Event>>>,
    cmd_tx: mpsc::Sender<DownlinkCmd>,
    ui_state: SharedUiState,
    client_id: &'static str,
    ch: &'static PublisherChannel,
    reconnect_notify: &tokio::sync::Notify,
) {
    // LWT topic 在重连循环外计算一次（仅 `Box::leak` 一次）。若放进 run_session，
    // 每次重连（失败 5s 一次）都会泄漏一个 String，长期运行内存只增不减。
    let lwt_topic: &'static str = Box::leak(cfg.lwt_topic().into_boxed_str());
    loop {
        let (immediate, reason) = run_session(&cfg, &app_state, &event_rx, &cmd_tx, &ui_state, client_id, ch, lwt_topic, reconnect_notify).await;
        if immediate {
            // 配网成功触发：已切到新网络，立即重连并重新 login，无需退避。
            log::info!("mqtt session ended: {reason}; re-login immediately");
        } else {
            // 断网/错误：退避 5s 避免频繁重连打爆 broker。
            log::warn!("mqtt session ended: {reason}; reconnect in 5s");
            // 注：block_on 单线程模型下直接同步 sleep 即可（等价于异步定时器延时）；
            // embassy-time 的 Timer 需 embassy executor 驱动，本项目用 futures::block_on，故用 std sleep。
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    }
}

async fn run_session(
    cfg: &AppConfig,
    app_state: &Arc<Mutex<AppState>>,
    event_rx: &Arc<Mutex<mpsc::Receiver<Event>>>,
    cmd_tx: &mpsc::Sender<DownlinkCmd>,
    ui_state: &SharedUiState,
    client_id: &'static str,
    ch: &'static PublisherChannel,
    lwt_topic: &'static str,
    reconnect_notify: &tokio::sync::Notify,
) -> (bool, String) {
    // 返回 (immediate_reconnect, reason)：
    //  - immediate=true  → 配网成功触发，mqtt_main 需「立即」重连/login（不退避）；
    //  - immediate=false → 断网/错误，mqtt_main 退避 5s 后重连。
    let addr = match cfg.broker_addr() {
        Ok(a) => a,
        Err(e) => return (false, e.to_string()),
    };

    let mut transport = StdTcpTransport::new(addr);
    if let Err(e) = transport.connect().await {
        return (false, format!("tcp connect: {e}"));
    }
    log::info!("broker connected: {addr}");

    let mut options = MqttOptions::new(client_id);
    options = options
        .with_keep_alive(Duration::from_secs(u64::from(cfg.mqtt.keepalive_secs)))
        .with_clean_session(cfg.mqtt.clean_session);
    // username 未显式配置时，默认等于设备 id（G+序列号，随板载序列号动态生成，
    // 与 topic 中的设备标识一致）——即「和 id 一样是动态的」，每台设备自动用自身序列号作为 MQTT 用户名。
    let username = cfg.mqtt.username.as_deref().unwrap_or(&cfg.device.id);
    options = options.with_credentials(username, cfg.mqtt.password.as_deref().unwrap_or(""));
    // LWT：断线遗嘱（MXS 协议 topic）。lwt_topic 由调用方在重连循环外泄漏一次，避免每 5s 重复泄漏。
    static LWT_PAYLOAD: &[u8] = b"";
    options = options.with_last_will(LastWill { topic: lwt_topic, payload: LWT_PAYLOAD, qos: QoS::AtLeastOnce, retain: false });

    let client = MqttClient::<_, 4, 2048>::new(transport, options);

    // 发布通道（仅作 runtime 的 publisher 输入；业务发布走模块 outbox）
    let rx = ch.receiver();

    let module = AppModule::new(cfg.clone(), app_state.clone(), event_rx.clone(), cmd_tx.clone(), ui_state.clone());
    let mut runtime = MqttRuntime::new(client, module, rx);
    // 与「配网成功后重连」信号竞速：信号到达即丢弃当前运行 future（底层 StdTcpTransport
    // 的 TcpStream 随之关闭），本次 session 结束，mqtt_main 据 immediate 标志「立即」重连/login；
    // 断网/错误则走 Either::Left，返回 immediate=false，由 mqtt_main 退避 5s。
    let run_fut = std::pin::pin!(runtime.run());
    let reconnect_fut = std::pin::pin!(reconnect_notify.notified());
    match futures::future::select(run_fut, reconnect_fut).await {
        futures::future::Either::Left((r, _)) => (false, format!("runtime: {r:?}")),
        futures::future::Either::Right((_, _)) => {
            log::info!("mqtt: wifi provisioned — re-login immediately (no backoff)");
            (true, "reconnect requested".to_string())
        }
    }
}
