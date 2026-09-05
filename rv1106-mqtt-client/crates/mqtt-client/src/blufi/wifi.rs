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
    conf: String,
    scan_timeout: Duration,
    connect_timeout: Duration,
}

impl WifiManager {
    pub fn new(cfg: &super::BluFiConfig) -> Self {
        Self {
            iface: cfg.wpa_iface.clone(),
            bin: "wpa_cli".to_string(),
            conf: cfg.wpa_conf.clone(),
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

    /// 探活：wpa_cli -i <iface> status 含 `wpa_state` 即接口可用。
    pub fn probe(&self) -> Result<(), WifiError> {
        let out = self.run(&["status"])?;
        if out.contains("wpa_state") {
            Ok(())
        } else {
            Err(WifiError::NoInterface)
        }
    }

    /// 触发扫描并轮询 `scan_results`，直到产出结果或超时（返回可能为空列表）。
    pub fn scan(&self) -> Result<Vec<super::ScanItem>, WifiError> {
        let _ = self.run(&["scan"]);
        let deadline = Instant::now() + self.scan_timeout;
        loop {
            let out = self.run(&["scan_results"]).unwrap_or_default();
            let lines: Vec<&str> = out.lines().collect();
            // 首行为表头（bssid / frequency / signal level / flags / ssid）
            let data = if lines.len() > 1 { &lines[1..] } else { &[][..] };
            if !data.is_empty() {
                return Ok(parse_scan_results(data));
            }
            if Instant::now() >= deadline {
                // 超时仍返回空列表（由上层按空列表回送 APP）
                return Ok(Vec::new());
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    /// 配网：将 ssid/pwd 持久化到 `wpa_supplicant.conf`；若新增或修改则重载
    /// wpa_supplicant 使其生效；随后轮询 `status` 等待拿到 IP。
    pub fn connect(&self, ssid: &str, pwd: Option<&str>) -> Result<(), WifiError> {
        let changed = self.update_conf(ssid, pwd)?;
        if changed {
            log::info!("blufi: wpa_supplicant.conf changed for ssid={ssid}, reloading wpa_supplicant");
            self.reload_wpa_supplicant()?;
        }
        let deadline = Instant::now() + self.connect_timeout;
        loop {
            let out = self.run(&["status"]).unwrap_or_default();
            if extract_ip(&out).is_some() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(WifiError::Timeout);
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    /// 将 ssid/pwd 写入 wpa_supplicant.conf。返回是否发生了变化（新增网络或修改了密码）。
    fn update_conf(&self, ssid: &str, pwd: Option<&str>) -> Result<bool, WifiError> {
        let path = &self.conf;
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if config_has_network(&text, ssid, pwd) {
            return Ok(false);
        }
        // 重建：保留全局配置行（跳过 network 块），写入单一目标 network 块
        let mut out = String::new();
        for line in text.lines() {
            let t = line.trim_start();
            if t.starts_with("network=") || t.starts_with('}') {
                continue;
            }
            out.push_str(line);
            out.push('\n');
        }
        if !out.contains("update_config") {
            out.push_str("ctrl_interface=DIR=/var/run/wpa_supplicant GROUP=netdev\n");
            out.push_str("update_config=1\n");
        }
        out.push('\n');
        out.push_str("network={\n");
        out.push_str(&format!("    ssid=\"{ssid}\"\n"));
        match pwd {
            Some(p) if !p.is_empty() => out.push_str(&format!("    psk=\"{p}\"\n")),
            _ => out.push_str("    key_mgmt=NONE\n"),
        }
        out.push_str("}\n");
        std::fs::write(path, out)
            .map_err(|e| WifiError::Permission(format!("write {path}: {e}")))?;
        Ok(true)
    }

    /// 让 wpa_supplicant 重新加载配置并连接（新增/修改网络后调用）。
    /// 优先 `wpa_cli reconfigure`（轻量重载，等同让新配置生效）；失败回退 `systemctl restart`。
    fn reload_wpa_supplicant(&self) -> Result<(), WifiError> {
        match self.run(&["reconfigure"]) {
            Ok(_) => {
                std::thread::sleep(Duration::from_millis(800));
                Ok(())
            }
            Err(_) => {
                let r = Command::new("systemctl")
                    .args(["restart", "wpa_supplicant"])
                    .output();
                match r {
                    Ok(o) if o.status.success() => {
                        std::thread::sleep(Duration::from_millis(1500));
                        Ok(())
                    }
                    _ => Err(WifiError::CmdFailed(-1, "reload wpa_supplicant failed".into())),
                }
            }
        }
    }

    pub fn remove_network(&self, id: u32) {
        let _ = self.run(&["remove_network", &id.to_string()]);
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

/// 判断 conf 中是否已存在与 (ssid, pwd) 完全一致的 network 块。
fn config_has_network(text: &str, ssid: &str, pwd: Option<&str>) -> bool {
    let needle = format!("ssid=\"{ssid}\"");
    let Some(pos) = text.find(&needle) else {
        return false;
    };
    let block_start = text[..pos].rfind("network={").unwrap_or(0);
    let block_end = text[pos..].find('}').map(|e| pos + e + 1).unwrap_or(text.len());
    let block = &text[block_start..block_end];
    match pwd {
        Some(p) if !p.is_empty() => block.contains(&format!("psk=\"{p}\"")),
        _ => block.contains("key_mgmt=NONE") && !block.contains("psk="),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_has_network_matches_psk() {
        let conf = "network={\n    ssid=\"home\"\n    psk=\"12345678\"\n}";
        assert!(config_has_network(conf, "home", Some("12345678")));
        assert!(!config_has_network(conf, "home", Some("wrong")));
        assert!(!config_has_network(conf, "other", Some("12345678")));
    }

    #[test]
    fn config_has_network_matches_open() {
        let conf = "network={\n    ssid=\"open\"\n    key_mgmt=NONE\n}";
        assert!(config_has_network(conf, "open", None));
        assert!(!config_has_network(conf, "open", Some("x")));
    }
}
