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
use mqtt_client::downlink::{Dispatcher, DownlinkCmd};
use mqtt_client::modules::AppModule;
use mqtt_client::moonraker::MoonrakerWorker;
use mqtt_client::state::Event;
use mqtt_client::transport::StdTcpTransport;
use mqtt_client::config::{device_id_from_serial, read_board_serial, PLACEHOLDER_DEVICE_ID};
use mqtt_client::{AppConfig, AppState};
use myrtio_mqtt::runtime::{MqttRuntime, PublishRequest};
use myrtio_mqtt::{LastWill, MqttClient, MqttOptions, QoS};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
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

    // 共享状态与通道
    let app_state = Arc::new(Mutex::new(AppState::default()));
    let (event_tx, event_rx) = mpsc::channel::<Event>();
    let (cmd_tx, cmd_rx) = mpsc::channel::<DownlinkCmd>();

    // 独立线程：moonraker 状态服务 / 下行分发
    let mr = MoonrakerWorker::spawn(cfg.moonraker.clone(), app_state.clone(), event_tx.clone());
    Dispatcher::spawn(cfg.clone(), cmd_rx, mr, event_tx);

    let event_rx = Arc::new(Mutex::new(event_rx));

    // 进程级 'static 资源：client_id 与 publisher 通道（每次重连复用，避免泄漏）
    // 'a 需为 'static：MqttRuntime 的 client_id 借用与 publisher channel 生命周期必须一致
    let client_id: &'static str = Box::leak(cfg.device.id.clone().into_boxed_str());
    let ch: &'static Channel<CriticalSectionRawMutex, PublishRequest<'static>, 8> = Box::leak(Box::new(Channel::new()));

    futures::executor::block_on(mqtt_main(cfg, app_state, event_rx, cmd_tx, client_id, ch));
}

type PublisherChannel = Channel<CriticalSectionRawMutex, PublishRequest<'static>, 8>;

async fn mqtt_main(
    cfg: AppConfig,
    app_state: Arc<Mutex<AppState>>,
    event_rx: Arc<Mutex<mpsc::Receiver<Event>>>,
    cmd_tx: mpsc::Sender<DownlinkCmd>,
    client_id: &'static str,
    ch: &'static PublisherChannel,
) {
    loop {
        let res = run_session(&cfg, &app_state, &event_rx, &cmd_tx, client_id, ch).await;
        log::warn!("mqtt session ended: {res:?}; reconnect in 5s");
        // 重连等待：block_on 单线程模型下，直接同步 sleep 即可（等价于异步定时器延时）。
        // 注：embassy-time 的 Timer 需要 embassy executor 提供定时器队列驱动，
        // 本项目用 futures::block_on 而非 embassy executor，故此处用 std 同步 sleep。
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
}

async fn run_session(
    cfg: &AppConfig,
    app_state: &Arc<Mutex<AppState>>,
    event_rx: &Arc<Mutex<mpsc::Receiver<Event>>>,
    cmd_tx: &mpsc::Sender<DownlinkCmd>,
    client_id: &'static str,
    ch: &'static PublisherChannel,
) -> Result<(), String> {
    let addr = cfg.broker_addr().map_err(|e| e.to_string())?;

    let mut transport = StdTcpTransport::new(addr);
    transport.connect().await.map_err(|e| format!("tcp connect: {e}"))?;
    log::info!("broker connected: {addr}");

    let mut options = MqttOptions::new(client_id);
    options = options
        .with_keep_alive(Duration::from_secs(u64::from(cfg.mqtt.keepalive_secs)))
        .with_clean_session(cfg.mqtt.clean_session);
    if let Some(u) = &cfg.mqtt.username {
        options = options.with_credentials(u, cfg.mqtt.password.as_deref().unwrap_or(""));
    }
    // LWT：断线遗嘱（MXS 协议 topic）
    let lwt_topic: &'static str = Box::leak(cfg.lwt_topic().into_boxed_str());
    static LWT_PAYLOAD: &[u8] = b"";
    options = options.with_last_will(LastWill { topic: lwt_topic, payload: LWT_PAYLOAD, qos: QoS::AtLeastOnce, retain: false });

    let client = MqttClient::<_, 4, 2048>::new(transport, options);

    // 发布通道（仅作 runtime 的 publisher 输入；业务发布走模块 outbox）
    let rx = ch.receiver();

    let module = AppModule::new(cfg.clone(), app_state.clone(), event_rx.clone(), cmd_tx.clone());
    let mut runtime = MqttRuntime::new(client, module, rx);
    runtime.run().await.map_err(|e| format!("runtime: {e:?}"))
}
