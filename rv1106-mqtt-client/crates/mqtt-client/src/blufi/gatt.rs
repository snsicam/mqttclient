//! 阶段 4：基于 `bluer` 的 GATT 服务端（BlueZ D-Bus，回调式编程模型）。
//!
//! 实现参考 `bluer/bluer/examples/gatt_server_cb.rs`：`Application` / `Service` /
//! `Characteristic` 以回调（`Characteristic*Method::Fun`）方式提供读写与通知，
//! 由 `bluer` 负责 D-Bus 对象树（GattApplication1 + ObjectManager）的注册与生命周期。
//!
//! 注册 BluFi GATT 服务（规格见 `docs/蓝牙配网详细设计.md` §4.3/§4.6.8）：
//! - Service `0xFFFF`（`0000ffff-0000-1000-8000-00805f9b34fb`，Primary）
//! - 特征 `0xFF01`（write / write-without-response）：APP → 设备，字节交给 `BleLink::try_recv`
//! - 特征 `0xFF02`（notify）：设备 → APP，经 bluer 的 `CharacteristicNotifier` 发送
//!
//! 广播名使用蓝牙「友好名」`Adapter::alias()`（即 main.conf `Name` 的完整值，例如
//! `M1S-Ge33700a6620dfddc`），与系统脚本 `bt_ble_up.sh` 一致。
//!
//! **不要**读 `Adapter::system_name()`（= BlueZ `Adapter1.Name` = 系统主机名）：本板
//! 主机名被截成 10 字节（`M1S-Ge3370`），正是 app 看到「被截断」的根因；也不要用
//! `name on` 式的内核短名（内核 `HCI_MAX_SHORT_NAME_LENGTH = 10`）。bluer 的
//! `local_name` 把名字交给 BlueZ 写进 scan response（Complete Local Name，31 字节
//! 预算），不受 10 字节内核限制——故只要名字源正确，app 即显示全名。
//!
//! 仅在 adapter 友好名 / main.conf 都缺失时，才回退到配置短名
//! `{name_prefix}-{device_id}`（`name_prefix` 为空时 `device_id`），与 `BluFiConfig::local_name` 一致。
//!
//! 线程模型：`start_gatt` 启动独立 `blufi-gatt` 线程，在其中建 tokio
//! `current_thread` runtime 并 `block_on` 异步 GATT 服务端（bluer 是异步 API，
//! 必须跑在 tokio runtime 里）。返回的 [`GattBleLink`] 在 `blufi` worker 线程调用，
//! 通过两个 channel 与 gatt 线程桥接：
//! - 上行（APP → 设备）：`std::sync::mpsc`，在 write 回调里同步 `send`；
//! - 下行（设备 → APP）：`tokio::sync::broadcast`，`Sender::send` 是同步的，
//!   因此 `BleLink::send`（同步签名）可直接调用；每个 notify 会话各自 `subscribe()`，
//!   支持断线重连后重新订阅。

use std::collections::BTreeSet;
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use bluer::adv::{Advertisement, Type as AdvType};
use bluer::gatt::local::{
    Application, Characteristic, CharacteristicNotify, CharacteristicNotifyMethod, CharacteristicRead,
    CharacteristicWrite, CharacteristicWriteMethod, Service,
};
use bluer::{Adapter, Session, Uuid};
use futures::FutureExt;
use tokio::sync::broadcast;

use crate::blufi::{BleLink, BleLinkError, BluFiConfig};

/// BluFi 服务 UUID `0xFFFF`（128-bit 展开形式）。
const SVC_UUID: Uuid = Uuid::from_u128(0x0000ffff_0000_1000_8000_00805f9b34fb);
/// 特征 `0xFF01`：APP → 设备（write / write-without-response）。
const CHAR1_UUID: Uuid = Uuid::from_u128(0x0000ff01_0000_1000_8000_00805f9b34fb);
/// 特征 `0xFF02`：设备 → APP（notify）。
const CHAR2_UUID: Uuid = Uuid::from_u128(0x0000ff02_0000_1000_8000_00805f9b34fb);

/// 下行（设备 → APP）广播通道容量。APP 未订阅时不积压（send 直接返回无接收者）。
const OUTBOUND_CAP: usize = 64;
/// 注册失败后的重试退避上限。
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// APP 使能 notify 后补发首帧前的延时：等 BlueZ 内部就绪，避免首帧丢失。
const RESEND_DELAY: Duration = Duration::from_millis(100);

/// 广播名硬上限（scan response 31 字节预算，2 字节 AD 头 → 名字 ≤ 29）。
/// 仅作安全兜底；名字过长由 BlueZ 在 scan response 阶段处理（与 `bt_ble_up.sh` 一致）。
const NAME_MAX: usize = 29;

/// 按 UTF-8 字符边界截断（避免切到多字节字符中间），长度按字节计。
fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// 从 `/etc/bluetooth/main.conf` 读取 `[General] Name = ...`（蓝牙友好名的真实来源）。
/// 失败 / 不存在 / 为空时返回 `None`。
fn read_main_conf_name() -> Option<String> {
    let content = std::fs::read_to_string("/etc/bluetooth/main.conf").ok()?;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // 形如 `Name = M1S-Ge33700a6620dfddc`（等号前允许空白，值可带引号）
        if let Some(rest) = line.strip_prefix("Name").map(|s| s.trim_start()) {
            if let Some(stripped) = rest.strip_prefix('=') {
                let name = stripped.trim().trim_matches('"').to_string();
                if !name.is_empty() {
                    return Some(name);
                }
            }
        }
    }
    None
}

/// 解析广播用的完整蓝牙名：
/// 1) 优先 adapter 友好名 `Alias`（= main.conf `Name` 的完整值，等效系统脚本的
///    `hciconfig hci0 name`）；
/// 2) 其次直接解析 main.conf `Name`；
/// 3) 最后用配置兜底短名（`{name_prefix}-{device_id}` / `device_id`）。
///
/// 关键：**不能**用 `Adapter::system_name()`（= BlueZ `Adapter1.Name` = 系统主机名），
/// 本板主机名被截成 10 字节（`M1S-Ge3370`），正是 app「被截断」的元凶。仅当 `Alias`
/// 与 `system_name()` 不同（即确为显式友好名）时才采用，避免回退到被截主机名。
async fn resolve_local_name(adapter: &Adapter, cfg_fallback: &str) -> String {
    if let Ok(alias) = adapter.alias().await {
        if !alias.is_empty() {
            match adapter.system_name().await {
                // Alias 与 system_name 不同 → 是显式友好名（完整），采用
                Ok(sys) if alias != sys => return truncate_bytes(&alias, NAME_MAX),
                // 取不到 system_name 无法判断 → 直接信任 alias
                Err(_) => return truncate_bytes(&alias, NAME_MAX),
                // 二者相同 → Alias 回退到了被截主机名，改用 main.conf / 配置
                Ok(_) => {}
            }
        }
    }
    if let Some(name) = read_main_conf_name() {
        if !name.is_empty() {
            return truncate_bytes(&name, NAME_MAX);
        }
    }
    truncate_bytes(cfg_fallback, NAME_MAX)
}

/// 启动 GATT 服务端并返回桥接 [`BleLink`]。
///
/// 返回的 link 交给 `BluFiWorker::spawn_with_link`；本函数另起 `blufi-gatt` 线程
/// 运行 tokio runtime + bluer 服务端，注册失败会自动退避重试。
pub fn start_gatt(cfg: &BluFiConfig, device_id: &str) -> Box<dyn BleLink> {
    // 广播名兜底：仅在 adapter 友好名 / main.conf 都缺失时使用。
    // 正常情况广播名来自 `Adapter::alias()`（= main.conf `Name`，完整 `M1S-Ge33700a6620dfddc`）。
    // 配置短名 `{name_prefix}-{device_id}`（`name_prefix` 为空时为 `device_id`）。
    let fallback = if cfg.name_prefix.is_empty() {
        device_id.to_string()
    } else {
        cfg.local_name(device_id)
    };

    // 上行：write 回调 → worker
    let (inbound_tx, inbound_rx) = mpsc::channel::<Vec<u8>>();
    // 下行：worker → 各 notify 会话（此处丢弃初始 receiver，会话开始时再 subscribe）
    let (outbound_tx, _outbound_rx) = broadcast::channel::<Vec<u8>>(OUTBOUND_CAP);
    // 最近一帧：APP 订阅 notify 时补发（VERSION 帧常早于订阅发出）
    let last = Arc::new(Mutex::new(None::<Vec<u8>>));

    let inbound_tx = Arc::new(Mutex::new(inbound_tx));
    let outbound_tx = Arc::new(Mutex::new(outbound_tx));

    let adapter = cfg.adapter.clone();
    let advertise = cfg.advertise;
    let g_inbound = inbound_tx.clone();
    let g_outbound = outbound_tx.clone();
    let g_last = last.clone();

    thread::Builder::new()
        .name("blufi-gatt".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    log::error!("bluetooth: cannot create tokio runtime: {e}");
                    return;
                }
            };
            rt.block_on(serve_loop(
                &adapter, advertise, &fallback, g_inbound, g_outbound, g_last,
            ));
        })
        .expect("spawn blufi-gatt");

    Box::new(GattBleLink {
        inbound_rx,
        outbound_tx,
        last,
    })
}

/// 设备↔APP 的真实字节链路：通过 channel 与 `blufi-gatt` 线程桥接。
pub struct GattBleLink {
    inbound_rx: mpsc::Receiver<Vec<u8>>,
    outbound_tx: Arc<Mutex<broadcast::Sender<Vec<u8>>>>,
    last: Arc<Mutex<Option<Vec<u8>>>>,
}

impl BleLink for GattBleLink {
    fn send(&self, bytes: &[u8]) -> Result<(), BleLinkError> {
        // 缓存最后一帧，APP 后续订阅 notify 时可补发
        if let Ok(mut g) = self.last.lock() {
            *g = Some(bytes.to_vec());
        }
        // info 级：发出去的每一帧都可见（排查手机收不到/收错时看这里）
        log::info!(
            "bluetooth: [tx->app] send {} bytes: {:02x?}",
            bytes.len(),
            bytes
        );
        let tx = self
            .outbound_tx
            .lock()
            .map_err(|e| BleLinkError::Send(e.to_string()))?;
        // 无订阅者（APP 未连接 / 未使能 notify）是正常的，不算链路错误
        if let Err(e) = tx.send(bytes.to_vec()) {
            log::debug!("bluetooth: notify not delivered ({e}) — app not subscribed yet");
        }
        Ok(())
    }

    fn try_recv(&self) -> Result<Option<Vec<u8>>, BleLinkError> {
        match self.inbound_rx.try_recv() {
            Ok(b) => Ok(Some(b)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(BleLinkError::Closed),
        }
    }
}

// ---------- GATT 服务端（tokio + bluer） ----------

/// 注册失败时退避重试，直到成功（bluetoothd / hci0 可能尚未就绪）。
async fn serve_loop(
    adapter: &str,
    advertise: bool,
    fallback: &str,
    inbound_tx: Arc<Mutex<mpsc::Sender<Vec<u8>>>>,
    outbound_tx: Arc<Mutex<broadcast::Sender<Vec<u8>>>>,
    last: Arc<Mutex<Option<Vec<u8>>>>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match serve(adapter, advertise, fallback, &inbound_tx, &outbound_tx, &last).await {
            Ok(()) => return,
            Err(e) => {
                log::warn!(
                    "bluetooth: gatt server error: {e} — retry in {}s",
                    backoff.as_secs()
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}

/// 建立会话、注册广播（可选）与 GATT 应用，然后永久挂起（句柄存活即保持注册）。
///
/// 广播与 GATT 注册是相互独立的两步：广播决定「手机能否扫到/连上」，
/// `RegisterApplication` 决定「连上后能看到哪些服务与特征」。因此广播失败
/// （例如系统侧已经在广播、控制器不支持多个广播实例）不应阻断 GATT 注册，
/// 否则系统已在广播时 BluFi 服务反而注册不上。
async fn serve(
    adapter_name: &str,
    advertise: bool,
    fallback: &str,
    inbound_tx: &Arc<Mutex<mpsc::Sender<Vec<u8>>>>,
    outbound_tx: &Arc<Mutex<broadcast::Sender<Vec<u8>>>>,
    last: &Arc<Mutex<Option<Vec<u8>>>>,
) -> bluer::Result<()> {
    let session = Session::new().await?;
    log::info!("bluetooth: bluer session established (D-Bus org.bluez)");

    let adapter = match session.adapter(adapter_name) {
        Ok(a) => a,
        Err(e) => {
            log::warn!(
                "bluetooth: adapter {adapter_name} unavailable ({e}) — using default adapter"
            );
            session.default_adapter().await?
        }
    };
    // hci0 未上电时注册广播/服务会失败
    adapter.set_powered(true).await?;

    let local_name = resolve_local_name(&adapter, fallback).await;
    log::info!(
        "bluetooth: advertising on adapter {} as {:?} (friendly name / main.conf, not system hostname)",
        adapter.name(),
        local_name
    );

    // 广播包只声明 ServiceUUIDs=[0xFFFF]（供 APP 扫描阶段识别 BluFi），
    // 设备名交给 BlueZ 填进 adv / scan response（过长会自动进 scan response）。
    //
    // `advertise=false` 时不注册广播（由系统侧负责），只注册 GATT 应用；
    // 默认 `advertise=true`（见 `BluFiConfig::default_advertise`），由本应用自广播，
    // 因为 RV1106 系统侧广播器未必发布 0xFFFF。注册失败也不致命——
    // 多数情况是系统侧已占用广播实例，此时 GATT 仍应注册成功。
    let _adv_handle = if advertise {
        let le_advertisement = Advertisement {
            advertisement_type: AdvType::Peripheral,
            service_uuids: BTreeSet::from([SVC_UUID]),
            discoverable: Some(true),
            local_name: Some(local_name),
            ..Default::default()
        };
        match adapter.advertise(le_advertisement).await {
            Ok(handle) => {
                log::info!("bluetooth: LE advertisement registered (ServiceUUIDs=[0xFFFF])");
                Some(handle)
            }
            Err(e) => {
                log::warn!(
                    "bluetooth: RegisterAdvertisement failed: {e} — 系统侧可能已在广播，继续注册 GATT 应用"
                );
                None
            }
        }
    } else {
        log::info!("bluetooth: skip LE advertisement (advertise=false，由系统侧负责广播)");
        None
    };

    // char1（0xFF01）：APP 写入 → 转发给 worker
    let char1_tx = inbound_tx.clone();
    // char2（0xFF02）：worker 下发 → notify 给 APP；同时可读（协议文档：可读 + 可通知）
    let char2_tx = outbound_tx.clone();
    let char2_last = last.clone();
    let char2_read_last = last.clone();

    let app = Application {
        services: vec![Service {
            uuid: SVC_UUID,
            primary: true,
            characteristics: vec![
                Characteristic {
                    uuid: CHAR1_UUID,
                    write: Some(CharacteristicWrite {
                        write: true,
                        write_without_response: true,
                        method: CharacteristicWriteMethod::Fun(Box::new(move |new_value, req| {
                            let tx = char1_tx.clone();
                            async move {
                                log::info!(
                                    "bluetooth: [char1] write {} bytes from {} (mtu {}): {:02x?}",
                                    new_value.len(),
                                    req.device_address,
                                    req.mtu,
                                    new_value
                                );
                                if let Ok(tx) = tx.lock() {
                                    if let Err(e) = tx.send(new_value) {
                                        log::warn!("bluetooth: [char1] write dropped (worker channel closed): {e}");
                                    }
                                }
                                Ok(())
                            }
                            .boxed()
                        })),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                Characteristic {
                    uuid: CHAR2_UUID,
                    read: Some(CharacteristicRead {
                        read: true,
                        fun: Box::new(move |_req| {
                            // 读回最近一次下发的帧（无则空），与 BluFi 协议的
                            // 「0xFF02 可读 + 可通知」一致。
                            let cached = char2_read_last
                                .lock()
                                .ok()
                                .and_then(|g| g.clone())
                                .unwrap_or_default();
                            async move { Ok(cached) }.boxed()
                        }),
                        ..Default::default()
                    }),
                    notify: Some(CharacteristicNotify {
                        notify: true,
                        method: CharacteristicNotifyMethod::Fun(Box::new(move |mut notifier| {
                            let tx = char2_tx.clone();
                            let last_frame = char2_last.clone();
                            async move {
                                // notify 会话是长驻的，放到独立任务里跑，避免阻塞 StartNotify 返回
                                tokio::spawn(async move {
                                    notify_session(&mut notifier, &tx, &last_frame).await;
                                });
                            }
                            .boxed()
                        })),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }],
        ..Default::default()
    };
    let _app_handle = adapter.serve_gatt_application(app).await?;
    log::info!("bluetooth: GATT application registered (0xFFFF)");

    // 句柄 `_adv_handle` / `_app_handle` 必须存活到进程结束：drop 即注销广播与服务。
    // 这里永久挂起，保持它们不被释放（实际不会返回）。
    std::future::pending::<()>().await;
    Ok(())
}

/// 一次 notify 会话：先补发最近一帧（VERSION 帧常早于订阅发出），再转发后续帧。
async fn notify_session(
    notifier: &mut bluer::gatt::local::CharacteristicNotifier,
    tx: &Arc<Mutex<broadcast::Sender<Vec<u8>>>>,
    last: &Arc<Mutex<Option<Vec<u8>>>>,
) {
    log::info!(
        "bluetooth: APP enabled notify (confirming={})",
        notifier.confirming()
    );

    let mut rx = match tx.lock() {
        Ok(tx) => tx.subscribe(),
        Err(e) => {
            log::warn!("bluetooth: outbound channel poisoned: {e}");
            return;
        }
    };

    // BlueZ 在 StartNotify() 返回前可能尚未完成内部就绪，稍延迟再发首帧，避免丢帧。
    tokio::time::sleep(RESEND_DELAY).await;
    if let Some(data) = last.lock().ok().and_then(|g| g.clone()) {
        log::info!(
            "bluetooth: [tx->app] resend last frame {} bytes: {:02x?}",
            data.len(),
            data
        );
        if let Err(e) = notifier.notify(data).await {
            log::warn!("bluetooth: resend failed: {e}");
        }
    }

    loop {
        match rx.recv().await {
            Ok(data) => {
                if let Err(e) = notifier.notify(data).await {
                    log::warn!("bluetooth: notify failed: {e}");
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                log::warn!("bluetooth: notify channel lagged — {n} frame(s) dropped");
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    log::info!("bluetooth: notify session stop");
}
