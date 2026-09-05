//! 阶段 4：基于 BlueZ D-Bus 的 GATT 服务端（纯 Rust，使用 `zbus`，不依赖 libdbus）。
//!
//! 注册 BluFi GATT 服务（规格见 `docs/蓝牙配网详细设计.md` §4.3/§4.6.8）：
//! - Service `0xFFFF`（`0000ffff-0000-1000-8000-00805f9b34fb`，Primary）
//! - 特征 `0xFF01`（write / write-without-response）：APP → 设备，字节交给 `BleLink::try_recv`
//! - 特征 `0xFF02`（notify）：设备 → APP，经 `PropertiesChanged(Value)` 发送
//!
//! 应用层注册 `LEAdvertisement1`：广播包(adv_data) 仅含 `ServiceUUIDs=[0xFFFF]`
//!（16-bit，供 APP 扫描阶段识别 BluFi 设备），**设备名只放进扫描响应包(scan_rsp_data)
//! 的 Complete Local Name（AD type 0x09）**。两个包各自 31 字节上限，名字（≤29 字节）
//! 完整出现在 scan_rsp，既不会被挤进广播包而截断，也不会同时出现在两个包里。
//! 设备名**取自 BlueZ adapter 的蓝牙名**（等价于系统脚本 `bt_ble_up.sh` 里的
//! `hciconfig hci0 name`，经 `name $NAME` 写入 scan_rsp），而不是应用层自拼
//! `{model}-{device_id}`（过长会截断）。名字来源顺序参考系统脚本：adapter Name →
//! `/etc/bluetooth/main.conf` Name → 回退到配置拼名，最终限制 29 字节（scan_rsp 预算
//! 31 − 2 AD 头），绝不用 `name on`（kernel 会把长名截断到 10 字节）。
//!
//! 线程模型：`start_gatt` 启动独立 `blufi-gatt` 线程跑 D-Bus 事件循环与 notify 派发；
//! 返回的 [`GattBleLink`] 在 `blufi` worker 线程调用，通过两个 mpsc channel 与 gatt 线程桥接。
//!
//! 命名注意（踩过的坑）：`#[zbus::interface]` 会把**方法名**转成 PascalCase，但**属性名
//! 保持原样**。而 BlueZ 的 GATT 接口属性是全大写（`UUID` / `Primary` /
//! `Service` / `Flags` / `Value` / `Notifying` / `Type` …），因此每个属性都
//! 必须显式 `#[zbus(property, name = "UUID")]`，否则 BlueZ 读不到属性，注册时会报
//! `org.bluez.Error.Failed: No object received`。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use zbus::blocking::Proxy;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{blocking::Connection, interface};

use crate::blufi::{BleLink, BleLinkError, BluFiConfig};

const APP_PATH: &str = "/org/geeetech/blufi";
const SVC_PATH: &str = "/org/geeetech/blufi/service";
const CHAR1_PATH: &str = "/org/geeetech/blufi/service/char1";
const CHAR2_PATH: &str = "/org/geeetech/blufi/service/char2";
const ADV_PATH: &str = "/org/geeetech/blufi/advertisement";
const SVC_UUID: &str = "0000ffff-0000-1000-8000-00805f9b34fb";
const CHAR1_UUID: &str = "0000ff01-0000-1000-8000-00805f9b34fb";
const CHAR2_UUID: &str = "0000ff02-0000-1000-8000-00805f9b34fb";

fn opath(s: &str) -> ObjectPath<'static> {
    ObjectPath::try_from(s.to_string()).unwrap()
}
fn opath_owned(s: &str) -> OwnedObjectPath {
    OwnedObjectPath::try_from(s.to_string()).unwrap()
}

/// 将任意可转为 D-Bus `Value` 的 Rust 值包装为 `OwnedValue`（用于 ObjectManager 静态树）。
fn ov<'a, V: Into<Value<'a>>>(v: V) -> OwnedValue {
    // 除 Fd 外不会失败（此处不传文件描述符）
    v.into().try_to_owned().expect("value to owned")
}

// ---------- GattApplication（标记接口） ----------
struct GattApplication;
#[interface(name = "org.bluez.GattApplication1")]
impl GattApplication {}

// ---------- ObjectManager（返回静态对象树，供 BlueZ 枚举服务/特征） ----------
struct AppObjectManager;
#[interface(name = "org.freedesktop.DBus.ObjectManager")]
impl AppObjectManager {
    #[zbus(name = "GetManagedObjects")]
    fn get_managed_objects(
        &self,
    ) -> HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>> {
        managed_objects()
    }
}

fn managed_objects() -> HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>> {
    let mut root = HashMap::new();

    let mut app = HashMap::new();
    app.insert("org.bluez.GattApplication1".to_string(), HashMap::new());
    app.insert("org.freedesktop.DBus.ObjectManager".to_string(), HashMap::new());
    root.insert(opath_owned(APP_PATH), app);

    let mut svc = HashMap::new();
    let mut sp = HashMap::new();
    sp.insert("UUID".to_string(), ov(SVC_UUID));
    sp.insert("Primary".to_string(), ov(true));
    svc.insert("org.bluez.GattService1".to_string(), sp);
    root.insert(opath_owned(SVC_PATH), svc);

    let mut c1 = HashMap::new();
    let mut p1 = HashMap::new();
    p1.insert("UUID".to_string(), ov(CHAR1_UUID));
    p1.insert("Service".to_string(), ov(opath_owned(SVC_PATH)));
    p1.insert(
        "Flags".to_string(),
        ov(vec!["write".to_string(), "write-without-response".to_string()]),
    );
    p1.insert("Value".to_string(), ov(Vec::<u8>::new()));
    c1.insert("org.bluez.GattCharacteristic1".to_string(), p1);
    root.insert(opath_owned(CHAR1_PATH), c1);

    let mut c2 = HashMap::new();
    let mut p2 = HashMap::new();
    p2.insert("UUID".to_string(), ov(CHAR2_UUID));
    p2.insert("Service".to_string(), ov(opath_owned(SVC_PATH)));
    p2.insert("Flags".to_string(), ov(vec!["notify".to_string()]));
    p2.insert("Value".to_string(), ov(Vec::<u8>::new()));
    c2.insert("org.bluez.GattCharacteristic1".to_string(), p2);
    root.insert(opath_owned(CHAR2_PATH), c2);

    root
}

// ---------- GattService ----------
struct GattService;
#[interface(name = "org.bluez.GattService1")]
impl GattService {
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> String {
        SVC_UUID.into()
    }
    #[zbus(property, name = "Primary")]
    fn primary(&self) -> bool {
        true
    }
}

// ---------- Characteristic 0xFF01（write，APP → 设备） ----------
struct Char1 {
    inbound_tx: mpsc::Sender<Vec<u8>>,
}
#[interface(name = "org.bluez.GattCharacteristic1")]
impl Char1 {
    #[zbus(name = "WriteValue")]
    async fn write_value(&self, value: Vec<u8>, _opts: HashMap<String, Value<'_>>) {
        let _ = self.inbound_tx.send(value);
    }
    #[zbus(name = "ReadValue")]
    async fn read_value(&self, _opts: HashMap<String, Value<'_>>) -> Vec<u8> {
        Vec::new()
    }
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> String {
        CHAR1_UUID.into()
    }
    #[zbus(property, name = "Service")]
    fn service(&self) -> OwnedObjectPath {
        opath_owned(SVC_PATH)
    }
    #[zbus(property, name = "Flags")]
    fn flags(&self) -> Vec<String> {
        vec!["write".into(), "write-without-response".into()]
    }
    #[zbus(property, name = "Value")]
    fn value(&self) -> Vec<u8> {
        Vec::new()
    }
}

// ---------- Characteristic 0xFF02（notify，设备 → APP） ----------
/// `resend` 由 `StartNotify` 置位，实际补发交给派发线程延迟执行（帧缓存由派发线程持有）。
///
/// 为什么要延迟：APP 使能 notify 通常晚于设备首发帧（VERSION 0x41），需要补发；
/// 但 BlueZ 在 `StartNotify()` 方法返回前可能尚未完成内部就绪，直接在该调用内发
/// `PropertiesChanged` 会丢帧，故只置位、由派发线程稍后补发。
struct Char2 {
    resend: Arc<AtomicBool>,
}
#[interface(name = "org.bluez.GattCharacteristic1")]
impl Char2 {
    /// APP 使能通知：仅置位，补发由派发循环延迟执行。
    #[zbus(name = "StartNotify")]
    async fn start_notify(&self) {
        self.resend.store(true, Ordering::SeqCst);
        log::info!("bluetooth: APP enabled notify (StartNotify)");
    }
    #[zbus(name = "StopNotify")]
    async fn stop_notify(&self) {
        log::info!("bluetooth: APP disabled notify (StopNotify)");
    }
    #[zbus(name = "WriteValue")]
    async fn write_value(&self, _value: Vec<u8>, _opts: HashMap<String, Value<'_>>) {}
    #[zbus(name = "ReadValue")]
    async fn read_value(&self, _opts: HashMap<String, Value<'_>>) -> Vec<u8> {
        Vec::new()
    }
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> String {
        CHAR2_UUID.into()
    }
    #[zbus(property, name = "Service")]
    fn service(&self) -> OwnedObjectPath {
        opath_owned(SVC_PATH)
    }
    #[zbus(property, name = "Flags")]
    fn flags(&self) -> Vec<String> {
        vec!["notify".into()]
    }
    #[zbus(property, name = "Notifying")]
    fn notifying(&self) -> bool {
        false
    }
    #[zbus(property, name = "Value")]
    fn value(&self) -> Vec<u8> {
        Vec::new()
    }
}

// ---------- LE Advertisement（adv 包只放 ServiceUUIDs，名字放 scan response） ----------
// 广播包(adv_data) 仅含 ServiceUUIDs=[0xFFFF]（16-bit，3 字节），供 APP 扫描阶段识别 BluFi；
// 设备名只放进扫描响应包(scan_rsp_data) 的 Complete Local Name（AD type 0x09）。
// 两个包各自 31 字节上限，分开后名字（≤29 字节）完整出现在 scan_rsp，既不会被挤进
// adv 包而截断，也不会同时出现在两个包里。名字取自 BlueZ adapter 蓝牙名（系统脚本
// `hciconfig hci0 name` 已写好），不再自拼长串。
struct Advertisement {
    adv_name: String,
}
#[interface(name = "org.bluez.LEAdvertisement1")]
impl Advertisement {
    #[zbus(property, name = "Type")]
    fn type_(&self) -> String {
        "peripheral".into()
    }
    #[zbus(property, name = "ServiceUUIDs")]
    fn service_uuids(&self) -> Vec<String> {
        vec!["0xFFFF".to_string()]
    }
    #[zbus(property, name = "Discoverable")]
    fn discoverable(&self) -> bool {
        true
    }
    // 名字只放 scan response（AD type 0x09 = Complete Local Name），不在 adv 包里。
    #[zbus(property, name = "ScanResponseData")]
    fn scan_response_data(&self) -> HashMap<u8, OwnedValue> {
        let mut m: HashMap<u8, OwnedValue> = HashMap::new();
        m.insert(0x09u8, ov(self.adv_name.as_bytes().to_vec()));
        m
    }
}

/// 读取 adapter 蓝牙名（等价于系统脚本 `hciconfig hci0 name`）。
/// 系统脚本用 `name $NAME` 把它写入 scan_rsp（完整名、不截断）；应用层取同一个名字，
/// 避免自拼长串导致截断。来源顺序参考系统脚本：adapter Name → main.conf Name → 回退值。
fn read_adapter_name(conn: &Connection, adapter: &str, fallback: &str) -> String {
    let path = format!("/org/bluez/{adapter}");
    let name = Proxy::new(conn, "org.bluez", path.as_str(), "org.bluez.Adapter1")
        .ok()
        .and_then(|p| p.get_property::<String>("Name").ok())
        .filter(|n| !n.is_empty());
    let name = match name {
        Some(n) => n,
        None => read_main_conf_name().unwrap_or_else(|| fallback.to_string()),
    };
    // 29 字节限制（参考系统脚本 NAME_MAX=29：scan_rsp 31 − 2 AD 头）
    truncate_bytes(&name, 29)
}

/// 从 `/etc/bluetooth/main.conf` 的 `Name=` 读设备名（系统脚本同样的兜底来源）。
fn read_main_conf_name() -> Option<String> {
    std::fs::read_to_string("/etc/bluetooth/main.conf")
        .ok()
        .and_then(|c| {
            c.lines().find_map(|l| {
                let l = l.trim();
                l.strip_prefix("Name").map(|v| {
                    v.trim_matches(|c: char| c == '=' || c == ' ' || c == '\t' || c == '"')
                        .to_string()
                })
            })
        })
}

/// 按 UTF-8 安全截断到 `max` 字节（不切断多字节字符）。
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

// ---------- 运行 GATT 服务端 ----------
fn run_gatt(
    adapter: &str,
    fallback: &str,
    inbound_tx: mpsc::Sender<Vec<u8>>,
    outbound_rx: mpsc::Receiver<Vec<u8>>,
) -> zbus::Result<()> {
    let conn = Connection::system()?;
    log::info!("bluetooth: gatt connected to system bus");

    // 设备名取自 adapter 蓝牙名（系统脚本已写好进 scan_rsp），而非自拼长串，避免截断；
    // 名字只放 scan response（见 Advertisement::scan_response_data），adv 包仅含 ServiceUUIDs
    let local_name = read_adapter_name(&conn, adapter, fallback);
    log::info!("bluetooth: adv name(scan_rsp) = {local_name} (from adapter/main.conf)");

    // 缓存最近一帧，供 APP 订阅 notify 时补发（VERSION 帧常早于订阅发出）
    let last = Arc::new(Mutex::new(None::<Vec<u8>>));
    let resend = Arc::new(AtomicBool::new(false));
    {
        let os = conn.object_server();
        os.at(APP_PATH, GattApplication)?;
        os.at(APP_PATH, AppObjectManager)?;
        os.at(SVC_PATH, GattService)?;
        os.at(CHAR1_PATH, Char1 { inbound_tx })?;
        os.at(CHAR2_PATH, Char2 {
            resend: resend.clone(),
        })?;
        os.at(ADV_PATH, Advertisement {
            adv_name: local_name.to_string(),
        })?;
    }

    let adapter_path = opath(&format!("/org/bluez/{}", adapter));

    // adapter 可能尚未就绪（bluetoothd 刚起 / hci0 未上电），退避重试注册
    let mut backoff_ms = 1_000u64;
    loop {
        ensure_adapter_powered(&conn, adapter);

        match register_application(&conn, &adapter_path) {
            Ok(()) => {
                log::info!("bluetooth: GATT application registered (0xFFFF)");
                break;
            }
            Err(e) => {
                log::warn!("bluetooth: RegisterApplication failed: {e} — retry in {backoff_ms}ms");
                thread::sleep(Duration::from_millis(backoff_ms));
                backoff_ms = (backoff_ms * 2).min(30_000);
            }
        }
    }

    // LE 广播注册：adv 包仅含 ServiceUUIDs=[0xFFFF]；设备名由 Advertisement 的
    // ScanResponseData(AD 0x09) 放进 scan response，不挤占 adv 包 31B 预算
    backoff_ms = 1_000;
    loop {
        match register_advertisement(&conn, &adapter_path) {
            Ok(()) => {
                log::info!("bluetooth: LE advertisement registered (ServiceUUIDs=[0xFFFF])");
                break;
            }
            Err(e) => {
                log::warn!("bluetooth: RegisterAdvertisement failed: {e} — retry in {backoff_ms}ms");
                thread::sleep(Duration::from_millis(backoff_ms));
                backoff_ms = (backoff_ms * 2).min(30_000);
            }
        }
    }

    // notify 派发循环：worker 通过 GattBleLink.send 推入的数据，经 0xFF02 的 PropertiesChanged 发往 APP
    loop {
        // APP 订阅 notify 后补发最近一帧：稍延迟等 BlueZ 就绪，避免首帧（VERSION）丢失
        if resend.swap(false, Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(100));
            let cached = last.lock().ok().and_then(|g| g.clone());
            if let Some(data) = cached {
                log::info!(
                    "bluetooth: APP subscribed — resending last frame ({} bytes)",
                    data.len()
                );
                if let Err(e) = notify_char2(&conn, &data) {
                    log::warn!("bluetooth: resend failed: {e}");
                }
            }
        }

        match outbound_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(data) => {
                // 缓存最后一帧，APP 后续订阅 notify 时可补发
                if let Ok(mut g) = last.lock() {
                    *g = Some(data.clone());
                }
                if let Err(e) = notify_char2(&conn, &data) {
                    log::warn!("bluetooth: notify failed: {e}");
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break, // worker 退出，链路关闭
        }
    }
    Ok(())
}

fn register_application(conn: &Connection, adapter_path: &ObjectPath<'_>) -> zbus::Result<()> {
    let opts: HashMap<String, OwnedValue> = HashMap::new();
    conn.call_method(
        Some("org.bluez"),
        adapter_path,
        Some("org.bluez.GattManager1"),
        "RegisterApplication",
        &(opath(APP_PATH), &opts),
    )?;
    Ok(())
}

fn register_advertisement(conn: &Connection, adapter_path: &ObjectPath<'_>) -> zbus::Result<()> {
    let opts: HashMap<String, OwnedValue> = HashMap::new();
    conn.call_method(
        Some("org.bluez"),
        adapter_path,
        Some("org.bluez.LEAdvertisingManager1"),
        "RegisterAdvertisement",
        &(opath(ADV_PATH), &opts),
    )?;
    Ok(())
}

/// 确保 adapter 已上电：hci0 未 powered 时 RegisterApplication 会失败。
fn ensure_adapter_powered(conn: &Connection, adapter: &str) {
    let adapter_path = format!("/org/bluez/{adapter}");
    match Proxy::new(conn, "org.bluez", adapter_path.as_str(), "org.bluez.Adapter1") {
        Ok(proxy) => match proxy.get_property::<bool>("Powered") {
            Ok(true) => log::debug!("bluetooth: adapter {adapter_path} powered"),
            Ok(false) => {
                log::warn!("bluetooth: adapter {adapter_path} not powered — setting Powered=true");
                if let Err(e) = proxy.set_property("Powered", true) {
                    log::warn!("bluetooth: failed to power on adapter: {e}");
                }
            }
            Err(e) => log::warn!("bluetooth: cannot read adapter Powered: {e}"),
        },
        Err(e) => log::warn!("bluetooth: cannot create adapter proxy: {e}"),
    }
}

fn notify_char2(conn: &Connection, data: &[u8]) -> zbus::Result<()> {
    let changed: HashMap<&str, OwnedValue> = HashMap::from([("Value", ov(data))]);
    let invalidated: Vec<&str> = Vec::new();
    let msg = zbus::Message::signal(
        opath_owned(CHAR2_PATH),
        "org.freedesktop.DBus.Properties",
        "PropertiesChanged",
    )?
    .build(&("org.bluez.GattCharacteristic1", changed, invalidated))?;
    conn.send(&msg)?;
    Ok(())
}

// ---------- 桥接 BleLink ----------
/// 设备↔APP 的真实字节链路：通过 mpsc 与 `blufi-gatt` 线程桥接。
pub struct GattBleLink {
    inbound_rx: mpsc::Receiver<Vec<u8>>,
    outbound_tx: mpsc::Sender<Vec<u8>>,
}

impl BleLink for GattBleLink {
    fn send(&self, bytes: &[u8]) -> Result<(), BleLinkError> {
        self.outbound_tx
            .send(bytes.to_vec())
            .map_err(|e| BleLinkError::Send(e.to_string()))
    }
    fn try_recv(&self) -> Result<Option<Vec<u8>>, BleLinkError> {
        match self.inbound_rx.try_recv() {
            Ok(b) => Ok(Some(b)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err(BleLinkError::Closed),
        }
    }
}

/// 启动 GATT 服务端并返回桥接 [`BleLink`]。
/// 返回的 link 交给 `BluFiWorker::spawn_with_link`；本函数另起 `blufi-gatt` 线程跑 D-Bus 循环。
pub fn start_gatt(cfg: &BluFiConfig, device_id: &str) -> Box<dyn BleLink> {
    // 广播名回退值：配置 name_prefix 时用 `{prefix}-{device_id}`，否则 `device_id` 本身；
    // 实际广播名优先取 adapter 蓝牙名（见 run_gatt::read_adapter_name，参考系统脚本写法），
    // 这里仅作读不到 adapter 名时的兜底，且会被截断到 29 字节。
    let fallback = if cfg.name_prefix.is_empty() {
        device_id.to_string()
    } else {
        cfg.local_name(device_id)
    };
    let fallback = truncate_bytes(&fallback, 29);
    let (inbound_tx, inbound_rx) = mpsc::channel::<Vec<u8>>();
    let (outbound_tx, outbound_rx) = mpsc::channel::<Vec<u8>>();
    let adapter = cfg.adapter.clone();

    thread::Builder::new()
        .name("blufi-gatt".into())
        .spawn(move || {
            if let Err(e) = run_gatt(&adapter, &fallback, inbound_tx, outbound_rx) {
                log::error!("bluetooth: gatt server exited: {e}");
            }
        })
        .expect("spawn blufi-gatt");

    Box::new(GattBleLink {
        inbound_rx,
        outbound_tx,
    })
}
