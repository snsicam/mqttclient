//! 蓝牙初始化信息（AIC8800 接入 RV1106，由 BlueZ 暴露为 hci0）。
//!
//! 架构说明：AIC8800 与 RV1106 之间的物理链路（本板为 UART）由系统侧 `btattach`/`hciattach`
//! 绑定为内核 HCI 设备（hci0），再由 BlueZ（bluetoothd）通过 D-Bus 向上提供标准 BLE/GATT 接口。
//!
//! **应用层绝不直接操作底层串口**，它只依赖两层：
//!   1. `/sys/class/bluetooth/hci*` —— 内核 HCI 设备存在且已初始化；
//!   2. system bus 上的 `org.bluez` —— bluetoothd 已就绪（阶段 4 的 `gatt.rs` 依赖它注册 GATT）。
//!
//! 本模块启动时只打印这两层的就绪状态，供现场确认蓝牙是否可用；不探测、不打开任何串口。
//! `uart`/`baud` 配置仅在缺少 hci 设备时用于提示系统侧 `btattach` 命令，不做任何访问。

use std::fs;
use std::path::Path;

use crate::blufi::BluFiConfig;

/// 初始化蓝牙并打印状态信息（仅日志 + 就绪性探测，不做 HCI 协商）。
pub fn init_bluetooth(cfg: &BluFiConfig) {
    log::info!("bluetooth: init start — chip=AIC8800 → BlueZ hci0, app uses D-Bus/GATT only");

    // 1. 内核 HCI 设备（hci*）—— 应用依赖的第一层
    match list_bluez_adapters() {
        Ok(ads) if !ads.is_empty() => {
            for a in &ads {
                log::info!(
                    "bluetooth: BlueZ adapter {} present{}",
                    a,
                    if adapter_is_up(a) { " (up)" } else { " (down)" }
                );
            }
        }
        Ok(_) => log::warn!(
            "bluetooth: no BlueZ hci adapter found — bind AIC8800 first, e.g. `btattach -B {} -p h4 -s {}`",
            cfg.uart,
            cfg.baud
        ),
        Err(e) => log::warn!("bluetooth: cannot probe BlueZ adapters: {e}"),
    }

    // 2. system bus 上的 org.bluez —— 应用依赖的第二层（gatt.rs 注册 GATT 需要它）
    match bluez_on_dbus() {
        Ok(true) => log::info!("bluetooth: org.bluez available on system bus"),
        Ok(false) => log::warn!(
            "bluetooth: org.bluez NOT on system bus — is bluetoothd running? (GATT server will fail to register)"
        ),
        Err(e) => log::warn!("bluetooth: cannot check org.bluez on system bus: {e}"),
    }

    log::info!("bluetooth: init completed");
}

/// 列出 `/sys/class/bluetooth` 下的 hci* 适配器名。
fn list_bluez_adapters() -> std::io::Result<Vec<String>> {
    let base = "/sys/class/bluetooth";
    let mut out = Vec::new();
    if !Path::new(base).exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(base)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("hci") {
            out.push(name);
        }
    }
    Ok(out)
}

/// 判断 hci 适配器是否已初始化：存在 `hciX/address` 或 `hciX/name` 即视为已 up。
fn adapter_is_up(name: &str) -> bool {
    let base = format!("/sys/class/bluetooth/{name}");
    Path::new(&format!("{base}/address")).exists() || Path::new(&format!("{base}/name")).exists()
}

/// 检查 system bus 上 `org.bluez` 是否已被 bluetoothd 注册（GATT 服务端依赖它）。
fn bluez_on_dbus() -> Result<bool, String> {
    let conn = zbus::blocking::Connection::system().map_err(|e| e.to_string())?;
    let proxy = zbus::blocking::fdo::DBusProxy::new(&conn).map_err(|e| e.to_string())?;
    let name = zbus::names::BusName::try_from("org.bluez").map_err(|e| e.to_string())?;
    proxy.name_has_owner(name).map_err(|e| e.to_string())
}
