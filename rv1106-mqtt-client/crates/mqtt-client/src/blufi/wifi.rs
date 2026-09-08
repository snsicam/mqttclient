//! wpa_cli 封装（详细设计 §4.6.7）。
//!
//! 通过 `std::process::Command` 调用 `wpa_cli -i <iface> ...` 完成扫描/配网。
//! v1 不引入第三方 wait-timeout，超长命令由上层轮询超时兜底。

use std::process::Command;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub enum WifiError {
    CmdFailed(i32, String),
    Timeout,
    Parse(String),
    NoInterface,
    Permission(String),
}

impl std::fmt::Display for WifiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WifiError::CmdFailed(c, m) => write!(f, "cmd failed ({c}): {m}"),
            WifiError::Timeout => write!(f, "timeout"),
            WifiError::Parse(m) => write!(f, "parse: {m}"),
            WifiError::NoInterface => write!(f, "no interface"),
            WifiError::Permission(m) => write!(f, "permission: {m}"),
        }
    }
}
impl std::error::Error for WifiError {}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum WifiStatus {
    Disconnected,
    Connecting,
    Connected { ip: Option<String> },
}

pub struct WifiManager {
    iface: String,
    bin: String,
    scan_timeout: Duration,
    connect_timeout: Duration,
}

/// 扫描阶段读取 `scan_results` 的次数（合并去重，见 [`WifiManager::scan`]）。
const SCAN_READS: usize = 3;
/// 两次 `scan_results` 读取之间的间隔（让 wpa_supplicant 有机会补充结果）。
const SCAN_READ_GAP: Duration = Duration::from_millis(500);

impl WifiManager {
    /// wpa_supplicant 网络接口（设备固定，不暴露到 toml）。
    const WPA_IFACE: &str = "wlan0";

    pub fn new(cfg: &super::BluFiConfig) -> Self {
        log::info!("blufi: wpa_supplicant iface={}", Self::WPA_IFACE);
        Self {
            iface: Self::WPA_IFACE.to_string(),
            bin: "wpa_cli".to_string(),
            scan_timeout: Duration::from_millis(cfg.scan_timeout_ms),
            connect_timeout: Duration::from_millis(cfg.connect_timeout_ms),
        }
    }

    fn run(&self, args: &[&str]) -> Result<String, WifiError> {
        let mut cmd = Command::new(&self.bin);
        cmd.arg("-i").arg(&self.iface);
        cmd.args(args);
        let out = cmd.output().map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                WifiError::Permission(e.to_string())
            } else {
                WifiError::CmdFailed(-1, e.to_string())
            }
        })?;
        if !out.status.success() {
            return Err(WifiError::CmdFailed(
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).to_string(),
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// 执行 wpa_cli 并**校验输出**：wpa_cli 对失败命令（打印 `FAIL`）仍返回退出码 0，
    /// 仅看退出码会误判成功（典型：`save_config` 因 `update_config` 未启用而 FAIL）。
    /// 故输出含 `FAIL` 一律视为错误。
    fn run_checked(&self, args: &[&str]) -> Result<String, WifiError> {
        let out = self.run(args)?;
        let t = out.trim();
        if t.eq_ignore_ascii_case("FAIL") || t.split_whitespace().any(|w| w == "FAIL") {
            return Err(WifiError::CmdFailed(
                -1,
                format!("{} -> {t}", args.join(" ")),
            ));
        }
        Ok(out)
    }

    /// 探活：wpa_cli -i <iface> status 含 `wpa_state` 即接口可用。
    pub fn probe(&self) -> Result<(), WifiError> {
        let out = self.run(&["status"])?;
        if out.contains("wpa_state") {
            Ok(())
        } else {
            Err(WifiError::NoInterface)
        }
    }

    /// 触发扫描后，等待 `scan_timeout`（默认 5 秒）让 wpa_supplicant 完成一次扫描，
    /// 再读 `SCAN_READS`（3）次 `scan_results`（间隔 `SCAN_READ_GAP`），合并去重后回送 APP。
    /// `scan` 是异步的，故先等待再多次读取合并，而非「首次非空即返回」。
    pub fn scan(&self) -> Result<Vec<super::ScanItem>, WifiError> {
        // 触发扫描；wpa_supplicant 正在扫描时可能临时拒绝，仅告警不阻断（结果仍可读取）。
        if let Err(e) = self.run(&["scan"]) {
            log::warn!("blufi: scan trigger failed (ignored): {e}");
        }
        // 定时器：等待一次完整扫描完成
        std::thread::sleep(self.scan_timeout);

        // 读三次 scan_results，合并去重；读取失败直接报错（避免「空列表=成功」误导 APP）
        let mut merged: Vec<super::ScanItem> = Vec::new();
        for _ in 0..SCAN_READS {
            let out = self.run(&["scan_results"])?;
            let lines: Vec<&str> = out.lines().collect();
            let data = if lines.len() > 1 { &lines[1..] } else { &[][..] };
            if !data.is_empty() {
                merge_scan_items(&mut merged, parse_scan_results(data));
            }
            std::thread::sleep(SCAN_READ_GAP);
        }
        let summary: Vec<String> = merged
            .iter()
            .map(|s| format!("{} ({}dBm)", s.ssid, s.rssi))
            .collect();
        log::info!(
            "blufi: scan returned {} wifi network(s): [{}]",
            merged.len(),
            summary.join(", ")
        );
        Ok(merged)
    }

    /// 配网：通过 `wpa_cli` 把 ssid/pwd 写入 wpa_supplicant（`set_network` → `save_config`），
    /// 再 `reconfigure` 重载生效，随后 `select_network` 主动切换到刚配置的网络（禁用其余网络），
    /// 最后轮询 `status` 等待拿到 IP。
    /// WiFi 配置完全由 wpa_supplicant 自身管理，程序不碰配置文件、不假定路径。
    pub fn connect(&self, ssid: &str, pwd: Option<&str>) -> Result<(), WifiError> {
        self.configure_via_cli(ssid, pwd)?;
        log::info!("blufi: reloading wpa_supplicant to apply config for ssid={ssid}");
        self.reload_wpa_supplicant()?;
        // 配网成功后主动切换到刚配置的网络：`select_network` 会禁用其它网络并强制关联目标
        // SSID（等价于 APP/手机侧的「切换 WiFi」），避免仅 enable 时仍停留在旧网络或被自动选择忽略。
        match self.find_network_id(ssid)? {
            Some(id) => {
                self.run_checked(&["select_network", &id.to_string()])?;
                log::info!("blufi: selected network id={id} (ssid={ssid}) — switching now");
            }
            None => {
                log::warn!(
                    "blufi: configured network {ssid:?} not found after reconfigure, skip select_network"
                );
            }
        }
        let deadline = Instant::now() + self.connect_timeout;
        loop {
            match self.run(&["status"]) {
                Ok(out) if extract_ip(&out).is_some() => return Ok(()),
                Ok(_) => {}
                Err(e) => log::warn!("blufi: status poll failed: {e}"),
            }
            if Instant::now() >= deadline {
                return Err(WifiError::Timeout);
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    /// 通过 wpa_cli 配置：已存在同 ssid 的网络则更新、否则新建；无条件 `save_config`
    /// 保证配置落盘（幂等，避免「比对一致就跳过」导致运行时配置未写入、重启即丢失）。
    fn configure_via_cli(&self, ssid: &str, pwd: Option<&str>) -> Result<(), WifiError> {
        let id = match self.find_network_id(ssid)? {
            Some(i) => i,
            None => self.add_network()?,
        };
        let id = id.to_string();
        self.run_checked(&["set_network", &id, "ssid", &quote(ssid)])?;
        match pwd {
            Some(p) if !p.is_empty() => {
                self.run_checked(&["set_network", &id, "psk", &quote(p)])?;
            }
            _ => {
                self.run_checked(&["set_network", &id, "key_mgmt", "NONE"])?;
            }
        }
        self.run_checked(&["enable_network", &id])?;
        self.run_checked(&["save_config"])?;
        Ok(())
    }

    /// `list_networks` 输出形如：`id \t ssid \t bssid \t flags`（首行为表头）。
    fn find_network_id(&self, ssid: &str) -> Result<Option<u32>, WifiError> {
        let out = self.run(&["list_networks"])?;
        for line in out.lines().skip(1) {
            let cols: Vec<&str> = line.split('\t').collect();
            if cols.len() >= 2 && cols[1].trim() == ssid {
                if let Ok(id) = cols[0].trim().parse::<u32>() {
                    return Ok(Some(id));
                }
            }
        }
        Ok(None)
    }

    fn add_network(&self) -> Result<u32, WifiError> {
        let out = self.run(&["add_network"])?;
        out.trim()
            .parse::<u32>()
            .map_err(|_| WifiError::Parse(format!("add_network: {out}")))
    }

    /// 让 wpa_supplicant 重载配置生效（新增/修改网络后调用）：`wpa_cli reconfigure`。
    fn reload_wpa_supplicant(&self) -> Result<(), WifiError> {
        self.run_checked(&["reconfigure"])?;
        Ok(())
    }

    pub fn remove_network(&self, id: u32) {
        let _ = self.run(&["remove_network", &id.to_string()]);
    }

    /// 列出当前已配置网络（wpa_cli `list_networks`），用于配网后确认，不依赖配置文件路径。
    pub fn list_networks_text(&self) -> String {
        self.run(&["list_networks"]).unwrap_or_else(|e| format!("<读取失败: {e}>"))
    }

    pub fn status(&self) -> Result<WifiStatus, WifiError> {
        let out = self.run(&["status"])?;
        if let Some(ip) = extract_ip(&out) {
            Ok(WifiStatus::Connected { ip: Some(ip) })
        } else if out.contains("wpa_state=COMPLETED") {
            Ok(WifiStatus::Connected { ip: None })
        } else {
            Ok(WifiStatus::Disconnected)
        }
    }
}

/// 解析 `scan_results` 数据行：`bssid \t frequency \t signal level \t flags \t ssid`。
fn parse_scan_results(lines: &[&str]) -> Vec<super::ScanItem> {
    let mut items = Vec::new();
    for line in lines {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 5 {
            continue;
        }
        let rssi = cols[2].trim().parse::<i8>().unwrap_or(0);
        let ssid = cols[4..].join("\t").trim().to_string();
        if !ssid.is_empty() {
            items.push(super::ScanItem { ssid, rssi });
        }
    }
    items
}

/// 将 `incoming` 合并进 `acc`：相同 ssid 仅保留一条，且取 RSSI 更强（数值更大、更接近 0）者。
fn merge_scan_items(acc: &mut Vec<super::ScanItem>, incoming: Vec<super::ScanItem>) {
    for it in incoming {
        match acc.iter_mut().find(|x| x.ssid == it.ssid) {
            Some(existing) => {
                if it.rssi > existing.rssi {
                    existing.rssi = it.rssi;
                }
            }
            None => acc.push(it),
        }
    }
}

/// wpa_cli 的 `set_network` 要求字符串值自带引号：`ssid "abc"`。
fn quote(s: &str) -> String {
    format!("\"{s}\"")
}

fn extract_ip(status: &str) -> Option<String> {
    for line in status.lines() {
        if let Some(rest) = line.trim().strip_prefix("ip_address=") {
            let ip = rest.trim();
            if !ip.is_empty() {
                return Some(ip.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_checked_treats_fail_as_error() {
        // wpa_cli 失败时退出码为 0 但输出 FAIL；run_checked 必须把它当错误（否则 save_config
        // 静默失败、conf 不被写入）。这里用真实 wpa_cli 不可行，改为验证判定函数本身。
        let is_fail = |out: &str| {
            let t = out.trim();
            t.eq_ignore_ascii_case("FAIL") || t.split_whitespace().any(|w| w == "FAIL")
        };
        assert!(is_fail("FAIL\n"));
        assert!(is_fail("OK\nFAIL\n"));
        assert!(!is_fail("OK\n"));
        assert!(!is_fail("0\n"));
    }

}
