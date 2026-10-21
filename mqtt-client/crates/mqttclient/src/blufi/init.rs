//! 蓝牙初始化信息（AIC8800 接入 RV1106，由 BlueZ 暴露为 hci0）。
//!
//! 架构说明：AIC8800 与 RV1106 之间的物理链路（本板为 UART）由系统侧 `btattach`/`hciattach`
//! 绑定为内核 HCI 设备（hci0），再由 BlueZ（bluetoothd）通过 D-Bus 向上提供标准 BLE/GATT 接口。
//!
//! **应用层绝不直接操作底层串口**，它只依赖两层：
//!   1. `/sys/class/bluetooth/hci*` —— 内核 HCI 设备存在且已初始化；
//!   2. system bus 上的 `org.bluez` —— bluetoothd 已就绪（阶段 4 的 `gatt.rs` 依赖它注册 GATT）。
//!
//! 第 2 层通过 `bluer::Session` 探测（bluer 是异步 API，这里临时起一个
//! current_thread runtime 跑一次会话；真正的长驻会话由 `gatt::start_gatt` 建立）。
//!
//! 本模块启动时只打印这两层的就绪状态，供现场确认蓝牙是否可用；不探测、不打开任何串口。
//! `uart`/`baud` 配置仅在缺少 hci 设备时用于提示系统侧 `btattach` 命令，不做任何访问。

use std::fs;
use std::path::Path;

use crate::blufi::BluFiConfig;

/// 初始化蓝牙并打印状态信息（仅日志 + 就绪性探测，不做 HCI 协商）。
pub fn init_bluetooth(cfg: &BluFiConfig) {
    log::info!("bluetooth: init start — chip=AIC8800 → BlueZ hci0, app uses bluer/BlueZ only");

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
    match probe_bluez(&cfg.adapter) {
        Ok(states) if !states.is_empty() => {
            for (name, powered) in &states {
                log::info!("bluetooth: bluer sees adapter {name} (powered={powered})");
            }
        }
        Ok(_) => log::warn!(
            "bluetooth: bluetoothd reachable but no adapter — is hci0 bound?"
        ),
        Err(e) => log::warn!(
            "bluetooth: org.bluez NOT reachable via bluer: {e} — is bluetoothd running? (GATT server will retry)"
        ),
    }

    // 3. 本地 GATT server 对外发布 BluFi 0xFFFF 需 bluetoothd 开启 experimental；
    //    buildroot 侧已在 /etc/bluetooth/main.conf 设 Experimental=true，此处不再重复检查。

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

/// 通过 bluer 探测 system bus 上的 `org.bluez`：返回 `[(adapter 名, 是否上电)]`。
///
/// 失败（如 bluetoothd 未运行 / 无 system bus）返回错误描述。
fn probe_bluez(want: &str) -> Result<Vec<(String, bool)>, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;

    rt.block_on(async {
        let session = bluer::Session::new().await.map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for name in session.adapter_names().await.map_err(|e| e.to_string())? {
            let powered = match session.adapter(&name) {
                Ok(a) => a.is_powered().await.unwrap_or(false),
                Err(_) => false,
            };
            out.push((name, powered));
        }
        if !out.iter().any(|(n, _)| n == want) {
            log::warn!("bluetooth: configured adapter {want} not in bluer adapter list {out:?}");
        }
        Ok(out)
    })
}
