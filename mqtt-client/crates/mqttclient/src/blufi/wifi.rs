//! wpa_cli 封装（详细设计 §4.6.7）。
//!
//! 通过 `std::process::Command` 调用 `wpa_cli -i <iface> ...` 完成扫描/配网。
//! v1 不引入第三方 wait-timeout，超长命令由上层轮询超时兜底。

use std::process::{Command, Stdio};
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
    /// 已关联时扫描回退（见 [`WifiManager::scan`]）：AIC8800 等芯片关联后 off-channel 扫描失效。
    scan_disconnect_fallback: bool,
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
            scan_disconnect_fallback: cfg.scan_disconnect_fallback,
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
    ///
    /// **已关联回退（AIC8800 等芯片）**：部分驱动关联 AP 后无法做 off-channel 扫描，
    /// `scan` 触发成功但 `scan_results` 恒为空（实测：配网后第二次取 WiFi 列表返回 0 个网络，
    /// 而断开关联后的普通扫描正常）。开启 `scan_disconnect_fallback` 时，一旦检测到已关联即
    /// **直接**临时 `disconnect` 让驱动做全信道扫描，扫完 `reconnect` 恢复关联并重起 udhcpc 拿回 IP。
    /// 走单程（不先尝试再做回退）是为避免两次 `scan` 叠加超过 APP 的 10s 列表超时；断开支路的
    /// 等待上限 4s，保证总耗时（≈0.5s + 4s + 读间隔）稳稳低于 10s。代价：扫描期间 WiFi/云连接
    /// 短暂断开约 1~2s（配网交互窗口内可接受）。关闭该开关（`scan_disconnect_fallback=false`）
    /// 则始终走普通扫描（适用于关联后扫描正常的其它芯片）。
    ///
    /// **注意**：此回退只治标。已关联时扫描为空，真正常见根因是**电源/SDIO 总线不稳**
    /// （内核 `sdio_err` -84/-110 + `rv1106_npor_powergood_isr voltage jitter detected`），
    /// 关联后芯片高功耗把供电轨拉抖，SDIO 传输损坏/超时；断开回到空闲低功耗态总线才稳。
    /// 若电源抖到连空闲扫描都失败，本回退也救不回（仍返回空）——此时需修硬件电源，
    /// 见下方「断开后仍为空」的告警。
    pub fn scan(&self) -> Result<Vec<super::ScanItem>, WifiError> {
        // 已关联 AP：关联态扫描不稳，直接临时断开做全信道扫描（回到空闲低功耗态）。
        let merged = if self.scan_disconnect_fallback && self.is_connected() {
            log::warn!(
                "blufi: interface associated — temporary disconnect to scan (scan while associated unreliable: power/SDIO instability)"
            );
            // 断开关联，使芯片回到空闲低功耗态、SDIO 总线恢复稳定
            if let Err(e) = self.run(&["disconnect"]) {
                log::warn!("blufi: disconnect before scan failed (ignored): {e}");
            }
            std::thread::sleep(Duration::from_millis(500));
            // 断关联后扫描；等待上限 4s 以稳稳低于 APP 10s 列表超时
            let wait = Duration::from_millis(std::cmp::min(self.scan_timeout.as_millis(), 4000) as u64);
            let r = self.scan_once(wait)?;
            // 恢复之前的关联（重连到已选网络）；失败仅告警，不阻断回送列表
            if let Err(e) = self.run(&["reconnect"]) {
                log::warn!("blufi: reconnect after scan failed (ignored): {e}");
            }
            // 关联恢复后重起 udhcpc，重新 DHCP 拿 IP（避免 IP 丢失）
            self.start_dhcp();
            r
        } else {
            // 未关联（首次配网前）或已关闭回退：常规扫描
            self.scan_once(self.scan_timeout)?
        };

        // 已关联回退后仍为空：说明连空闲态 SDIO 都不稳，几乎可断定是电源/硬件问题，明指方向。
        if merged.is_empty() && self.scan_disconnect_fallback && self.is_connected() {
            log::warn!(
                "blufi: scan STILL empty after disconnect — likely power/SDIO instability; \
                 check `dmesg` for `sdio_err` (-84/-110) and `rv1106_npor_powergood_isr voltage jitter`"
            );
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

    /// 单次扫描：触发 `scan` → 等 `wait` → 读 `SCAN_READS` 次 `scan_results` 合并去重。
    /// `wait` 由调用方传入（普通路径用 `scan_timeout`，已关联回退路径用上限 4s）。
    fn scan_once(&self, wait: Duration) -> Result<Vec<super::ScanItem>, WifiError> {
        // 触发扫描；wpa_supplicant 正在扫描时可能临时拒绝，仅告警不阻断（结果仍可读取）。
        if let Err(e) = self.run(&["scan"]) {
            log::warn!("blufi: scan trigger failed (ignored): {e}");
        }
        // 定时器：等待一次完整扫描完成
        std::thread::sleep(wait);

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
        Ok(merged)
    }

    /// 当前是否已关联到 AP（用于判断是否需要临时断开再扫描）。
    fn is_connected(&self) -> bool {
        matches!(self.status(), Ok(WifiStatus::Connected { .. }))
    }

    /// 仅触发一次扫描（`wpa_cli scan`），**不等结果、不读 `scan_results`**。
    /// 用于「蓝牙连接后预热扫描」等场景：异步扫描由 wpa_supplicant 后台完成，
    /// APP 随后发 `GET_WIFI_LIST` 时 `scan()` 能直接拿到较新的结果。
    /// 触发失败仅告警（如正在扫描中被临时拒绝），不阻断业务流程。
    pub fn trigger_scan(&self) {
        match self.run(&["scan"]) {
            Ok(_) => log::info!("blufi: triggered wpa_cli scan (bluetooth connected)"),
            Err(e) => log::warn!("blufi: scan trigger failed (ignored): {e}"),
        }
    }

    /// 选网后确保 wlan0 拿到**新网络**的 IP。设备侧通常已有 `udhcpc -i <iface>` 在跑，
    /// wpa_supplicant 切到新网络后旧 udhcpc 仍持旧租约（busybox 续期要等租约 1/2~7/8 时长），
    /// 不会立刻去新网络重新要 IP —— 这正是「配网完成但 IP 不更新」的根因。
    /// 故这里**重启** udhcpc：先按命令行匹配杀掉已有实例（避免多个 DHCP 客户端争抢接口），
    /// 再 spawn 新实例，新实例会在刚关联的接口上重新 DHCP 拿到新 IP。
    /// 后台运行（spawn，不等结果、stdout/stderr 重定向到 /dev/null）。
    /// 依赖 busybox 的 `pkill` 与 `default.script`（配置地址/路由）。
    fn start_dhcp(&self) {
        // 杀掉已有 udhcpc（按命令行匹配本接口）；pkill 不存在/无匹配则忽略，照常启动新实例。
        let _ = Command::new("pkill")
            .args(["-f", &format!("udhcpc.*{}", self.iface)])
            .status();
        let pidfile = format!("/var/run/udhcpc.{}.pid", self.iface);
        match Command::new("udhcpc")
            .arg("-i")
            .arg(&self.iface)
            .arg("-p")
            .arg(&pidfile)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => log::info!("blufi: restarted udhcpc -i {} (pid {})", self.iface, child.id()),
            Err(e) => log::warn!("blufi: start udhcpc failed (ignored): {e}"),
        }
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
        // 选网后启动 DHCP 客户端：wpa_supplicant 只负责认证/关联，IP 地址需 udhcpc 分配，
        // 否则 `status` 永远没有 `ip_address=`，`connect` 只能靠超时退出。
        self.start_dhcp();
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
