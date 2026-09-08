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

/// 扫描阶段读取 `scan_results` 的次数（合并去重，见 [`WifiManager::scan`]）。
const SCAN_READS: usize = 3;
/// 两次 `scan_results` 读取之间的间隔（让 wpa_supplicant 有机会补充结果）。
const SCAN_READ_GAP: Duration = Duration::from_millis(500);

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

    /// 执行 wpa_cli 并**校验输出**：wpa_cli 对失败命令（打印 `FAIL`）仍返回退出码 0，
    /// 仅看退出码会误判成功（典型：`save_config` 因 `update_config` 未启用而 FAIL，
    /// 结果 conf 从未被写入）。故输出含 `FAIL` 一律视为错误，交给上层回退。
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

    /// 确保 conf 含 `update_config=1` 与 `ctrl_interface`，缺失则补写。
    /// 返回是否改动了文件。
    fn ensure_conf_header_file(&self) -> Result<bool, WifiError> {
        let path = &self.conf;
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if text.contains("update_config") {
            return Ok(false);
        }
        let out = ensure_conf_header(&text);
        std::fs::write(path, out)
            .map_err(|e| WifiError::Permission(format!("write {path}: {e}")))?;
        Ok(true)
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

    /// 触发扫描后，用定时器等待 `scan_timeout`（默认 5 秒）让 wpa_supplicant 完成一次
    /// 完整扫描；随后读 `SCAN_READS`（3）次 `scan_results`（每次间隔 `SCAN_READ_GAP`），
    /// 合并（按 ssid 去重、取最强 RSSI）后回送 APP。
    ///
    /// 说明：wpa_supplicant 的 `scan` 是异步的——下发 `scan` 后需等待数秒结果才齐全，
    /// 过早读取会得到空/不完整列表。故先定时器等待、再多次读取合并，而非「首次非空即返回」。
    pub fn scan(&self) -> Result<Vec<super::ScanItem>, WifiError> {
        // 触发扫描（结果稍后通过 scan_results 读取；忽略触发命令的瞬时错误）
        let _ = self.run(&["scan"]);

        // 定时器：等待 scan_timeout，确保一次完整扫描完成
        std::thread::sleep(self.scan_timeout);

        // 读三次 scan_results，合并去重
        let mut merged: Vec<super::ScanItem> = Vec::new();
        for _ in 0..SCAN_READS {
            if let Ok(out) = self.run(&["scan_results"]) {
                let lines: Vec<&str> = out.lines().collect();
                // 首行为表头（bssid / frequency / signal level / flags / ssid）
                let data = if lines.len() > 1 { &lines[1..] } else { &[][..] };
                if !data.is_empty() {
                    merge_scan_items(&mut merged, parse_scan_results(data));
                }
            }
            std::thread::sleep(SCAN_READ_GAP);
        }
        Ok(merged)
    }

    /// 配网：将 ssid/pwd 持久化到 `wpa_supplicant.conf`；若新增或修改则重载
    /// wpa_supplicant 使其生效；随后轮询 `status` 等待拿到 IP。
    ///
    /// 成功时返回已连接 AP 的 BSSID（拿不到则全 0），供连接状态报告 0xF 携带。
    pub fn connect(&self, ssid: &str, pwd: Option<&str>) -> Result<[u8; 6], WifiError> {
        let changed = self.configure_network(ssid, pwd)?;
        if changed {
            log::info!("blufi: wpa_supplicant.conf changed for ssid={ssid}, reloading wpa_supplicant");
            self.reload_wpa_supplicant()?;
        }
        let deadline = Instant::now() + self.connect_timeout;
        loop {
            let out = self.run(&["status"]).unwrap_or_default();
            if extract_ip(&out).is_some() {
                return Ok(extract_bssid(&out).unwrap_or([0u8; 6]));
            }
            if Instant::now() >= deadline {
                return Err(WifiError::Timeout);
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    /// 配置网络：优先用 `wpa_cli`（同 ssid 只更新密码，不新增条目）；
    /// 若 wpa_cli 不可用（如 wpa_supplicant 未运行），回退到直接改写 conf。
    /// 返回是否发生了变化（变化才需要重载/重连）。
    fn configure_network(&self, ssid: &str, pwd: Option<&str>) -> Result<bool, WifiError> {
        // 关键：wpa_cli 的 `save_config` 只有在 conf 含 `update_config=1` 时才会回写文件，
        // 且 wpa_cli 本身依赖 `ctrl_interface`。缺这两行会让 save_config 静默失败
        // （退出码仍为 0），conf 始终不变。故走 wpa_cli 之前先补齐。
        if let Err(e) = self.ensure_conf_header_file() {
            log::warn!("blufi: ensure conf header failed ({e}) — 继续尝试 wpa_cli");
        }
        let changed = match self.configure_via_cli(ssid, pwd) {
            Ok(c) => c,
            Err(e) => {
                log::warn!(
                    "blufi: wpa_cli configure failed ({e}) — fallback to writing {}",
                    self.conf
                );
                self.update_conf(ssid, pwd)?
            }
        };
        // 落盘校验：`save_config` 在部分环境不生效（conf 实际没变、重启即丢配置），
        // 这里确认 conf 真的含有该 ssid；否则强制手写一次。
        if !self.conf_has_ssid(ssid) {
            log::warn!(
                "blufi: {} 未落盘 ssid={}（save_config 未生效），强制写入",
                self.conf, ssid
            );
            self.update_conf(ssid, pwd)?;
            return Ok(true);
        }
        Ok(changed)
    }

    /// conf 中是否已存在指定 ssid（用于落盘校验）。
    fn conf_has_ssid(&self, ssid: &str) -> bool {
        std::fs::read_to_string(&self.conf)
            .map(|t| t.contains(&format!("ssid=\"{ssid}\"")))
            .unwrap_or(false)
    }

    /// 通过 wpa_cli 配置：已存在同 ssid 的网络则**只更新密码**，否则新建网络。
    fn configure_via_cli(&self, ssid: &str, pwd: Option<&str>) -> Result<bool, WifiError> {
        let id = match self.find_network_id(ssid)? {
            Some(i) => i,
            None => self.add_network()?,
        };
        let id = id.to_string();
        // 已存在同 ssid：密码一致则无需改动（避免无谓重载导致断连）
        if self.network_matches(&id, pwd) {
            self.run_checked(&["enable_network", &id])?;
            return Ok(false);
        }
        // 更新/写入 ssid 与密码；开放网络用 key_mgmt=NONE
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
        // 让 wpa_supplicant 把运行时配置写回 conf，保持二者一致。
        // 必须校验输出：save_config 失败（如 update_config 未启用）时 wpa_cli 退出码仍为 0，
        // 只看退出码会误判成功、导致 conf 从未被写入。失败即回退到手写 conf。
        self.run_checked(&["save_config"])?;
        Ok(true)
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

    /// 判断某网络的密码是否已与期望一致（已一致就不改，避免不必要的重连）。
    fn network_matches(&self, id: &str, pwd: Option<&str>) -> bool {
        let cur_psk = self
            .run(&["get_network", id, "psk"])
            .ok()
            .map(|s| s.trim().trim_matches('"').to_string())
            .unwrap_or_default();
        match pwd {
            Some(p) if !p.is_empty() => cur_psk == p,
            _ => {
                // 开放网络：无 psk 且 key_mgmt=NONE
                let km = self
                    .run(&["get_network", id, "key_mgmt"])
                    .ok()
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                cur_psk.is_empty() && km.eq_ignore_ascii_case("NONE")
            }
        }
    }

    /// 直接改写 conf（wpa_cli 不可用时的回退）：
    /// 已存在同 ssid 的 network 块 → **只替换该块**（更新密码）；否则追加新块。
    /// 其它 network 块与全局配置一律保留（旧实现会误删块内行并清掉其它网络）。
    fn update_conf(&self, ssid: &str, pwd: Option<&str>) -> Result<bool, WifiError> {
        let path = &self.conf;
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if config_has_network(&text, ssid, pwd) {
            return Ok(false);
        }
        let block = build_network_block(ssid, pwd);
        let out = if let Some((start, end)) = find_network_block(&text, ssid) {
            // 同 ssid：替换该块（更新密码），其余原样保留
            let mut o = String::new();
            o.push_str(&text[..start]);
            o.push_str(&block);
            o.push_str(&text[end..]);
            o
        } else {
            // 新 ssid：保留全部内容后追加块；并确保 ctrl_interface/update_config 存在
            let mut o = ensure_conf_header(&text);
            if !o.ends_with('\n') {
                o.push('\n');
            }
            o.push('\n');
            o.push_str(&block);
            o
        };
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

    /// 读取配置文件内容用于日志排查（**`psk=` 明文打码**，避免密码落入日志）。
    /// 路径见 [`WifiManager::conf_path`]。
    pub fn dump_conf(&self) -> String {
        match std::fs::read_to_string(&self.conf) {
            Ok(text) => mask_psk(&text),
            Err(e) => format!("<读取失败: {e}>"),
        }
    }

    /// 配置文件路径（默认 `/data/wpa_supplicant.conf`），与 [`WifiManager::dump_conf`] 配合使用。
    pub fn conf_path(&self) -> &str {
        &self.conf
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

/// 从 `wpa_cli status` 输出解析 `bssid=xx:xx:xx:xx:xx:xx` → 6 字节。
/// 供连接状态报告 0xF 携带 BSSID（标准 BluFi 布局要求）。
fn extract_bssid(status: &str) -> Option<[u8; 6]> {
    for line in status.lines() {
        if let Some(rest) = line.trim().strip_prefix("bssid=") {
            let mut out = [0u8; 6];
            let mut i = 0usize;
            for part in rest.trim().split(':') {
                if i >= 6 {
                    return None;
                }
                out[i] = u8::from_str_radix(part, 16).ok()?;
                i += 1;
            }
            if i == 6 {
                return Some(out);
            }
        }
    }
    None
}

/// 把 `psk="明文"` / `psk=hex` 打码为 `psk="***(N位)"`，避免明文密码进日志；
/// 保留位数便于确认「同 ssid 是否真的替换了密码」。其余行原样保留。
fn mask_psk(text: &str) -> String {
    text.lines()
        .map(|line| {
            let t = line.trim_start();
            match t.strip_prefix("psk=") {
                Some(rest) => {
                    let v = rest.trim().trim_matches('"');
                    let indent = &line[..line.len() - t.len()];
                    format!("{indent}psk=\"***({}位)\"", v.chars().count())
                }
                None => line.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// wpa_cli 的 `set_network` 要求字符串值自带引号：`ssid "abc"`。
fn quote(s: &str) -> String {
    format!("\"{s}\"")
}

/// 生成一个 network 块文本（有密码用 psk，无密码用 key_mgmt=NONE）。
fn build_network_block(ssid: &str, pwd: Option<&str>) -> String {
    let mut b = String::from("network={\n");
    b.push_str(&format!("    ssid=\"{ssid}\"\n"));
    match pwd {
        Some(p) if !p.is_empty() => b.push_str(&format!("    psk=\"{p}\"\n")),
        _ => b.push_str("    key_mgmt=NONE\n"),
    }
    b.push_str("}\n");
    b
}

/// 定位包含指定 ssid 的 network 块，返回其字节范围 `[start, end)`。
/// 按 `network={` 起始、第一个 `}` 结束（块内无嵌套）。
fn find_network_block(text: &str, ssid: &str) -> Option<(usize, usize)> {
    let needle = format!("ssid=\"{ssid}\"");
    let mut from = 0usize;
    while let Some(rel) = text[from..].find("network={") {
        let start = from + rel;
        match text[start..].find('}') {
            Some(rel_end) => {
                let end = start + rel_end + 1;
                if text[start..end].contains(&needle) {
                    return Some((start, end));
                }
                from = end;
            }
            None => break,
        }
    }
    None
}

/// 确保 conf 含 `update_config`（wpa_cli 依赖 `ctrl_interface`，`update_config` 让配置可持久化）。
fn ensure_conf_header(text: &str) -> String {
    if text.contains("update_config") {
        return text.to_string();
    }
    let mut o = String::from("ctrl_interface=DIR=/var/run/wpa_supplicant GROUP=netdev\n");
    o.push_str("update_config=1\n");
    o.push_str(text);
    o
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
    fn find_and_replace_same_ssid_block() {
        let conf = "ctrl_interface=DIR=/var/run/wpa_supplicant GROUP=netdev\n\
                    update_config=1\n\
                    network={\n    ssid=\"home\"\n    psk=\"oldpwd\"\n}\n\
                    network={\n    ssid=\"office\"\n    psk=\"opwd\"\n}\n";
        // 能定位到同 ssid 的块，且只覆盖该块
        let (s, e) = find_network_block(conf, "home").expect("应找到 home 块");
        assert_eq!(&conf[s..e], "network={\n    ssid=\"home\"\n    psk=\"oldpwd\"\n}");

        // 替换后：home 密码更新，office 与其它配置保留
        let block = build_network_block("home", Some("newpwd"));
        let mut out = String::new();
        out.push_str(&conf[..s]);
        out.push_str(&block);
        out.push_str(&conf[e..]);
        assert!(out.contains("psk=\"newpwd\""));
        assert!(!out.contains("oldpwd"), "旧密码应被替换，而非新增条目");
        assert!(out.contains("ssid=\"office\""), "其它网络必须保留");
        assert_eq!(out.matches("ssid=\"home\"").count(), 1, "同 ssid 不得重复");
    }

    #[test]
    fn conf_has_ssid_detects_saved_network() {
        let dir = std::env::temp_dir().join("blufi_conf_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wpa_supplicant.conf");
        std::fs::write(
            &path,
            "update_config=1\nnetwork={\n    ssid=\"home\"\n    psk=\"x\"\n}\n",
        )
        .unwrap();

        let mut cfg = crate::blufi::BluFiConfig::default();
        cfg.wpa_conf = path.to_string_lossy().to_string();
        let wm = WifiManager::new(&cfg);

        assert!(wm.conf_has_ssid("home"), "已保存的 ssid 应能检出");
        assert!(!wm.conf_has_ssid("other"), "未保存的 ssid 应返回 false");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ensure_conf_header_adds_update_config() {
        let text = "network={\n    ssid=\"a\"\n}\n";
        let out = ensure_conf_header(text);
        assert!(out.contains("update_config=1"), "必须补 update_config=1");
        assert!(out.contains("ctrl_interface="), "wpa_cli 依赖 ctrl_interface");
        assert!(out.contains("ssid=\"a\""), "原有内容须保留");
        // 已有时不重复添加
        assert_eq!(ensure_conf_header(&out).matches("update_config=1").count(), 1);
    }

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

    #[test]
    fn dump_conf_masks_psk() {
        let conf = "ctrl_interface=DIR=/var/run/wpa_supplicant\nupdate_config=1\nnetwork={\n    ssid=\"home\"\n    psk=\"secret123\"\n}\n";
        let masked = mask_psk(conf);
        assert!(!masked.contains("secret123"), "明文密码不得进日志");
        assert!(masked.contains("psk=\"***(9位)\""), "应打码并保留位数");
        assert!(masked.contains("ssid=\"home\""), "其它行应原样保留");
        assert!(masked.contains("update_config=1"));
    }

    #[test]
    fn build_block_open_network_uses_key_mgmt() {
        assert_eq!(
            build_network_block("open", None),
            "network={\n    ssid=\"open\"\n    key_mgmt=NONE\n}\n"
        );
    }

    #[test]
    fn config_has_network_matches_open() {
        let conf = "network={\n    ssid=\"open\"\n    key_mgmt=NONE\n}";
        assert!(config_has_network(conf, "open", None));
        assert!(!config_has_network(conf, "open", Some("x")));
    }
}
