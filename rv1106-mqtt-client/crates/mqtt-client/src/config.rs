//! TOML 配置加载与校验（LLD-003 §9）。

use std::fmt;
use std::net::SocketAddr;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppConfig {
    pub device: DeviceConfig,
    pub mqtt: MqttConfig,
    pub moonraker: MoonrakerConfig,
    #[serde(default)]
    pub download: DownloadConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            device: DeviceConfig::default(),
            mqtt: MqttConfig::default(),
            moonraker: MoonrakerConfig::default(),
            download: DownloadConfig::default(),
        }
    }
}

/// 占位设备 ID：读不到板载序列号时的兜底值。
pub const PLACEHOLDER_DEVICE_ID: &str = "G000000000000";

/// 读取板载序列号。
///
/// Linux 下从 `/proc/cpuinfo` 的 `Serial` 字段读取（单板机每块唯一，
/// 如 `e33700a6620dfddc`）；非 Linux 或读取失败返回 `None`。
/// 参考 `rust-libp2p/p2p-camera/device-cam/src/main.rs` 的 `read_board_serial`。
pub fn read_board_serial() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(content) = std::fs::read_to_string("/proc/cpuinfo") {
            for line in content.lines() {
                let line = line.trim();
                if let Some(rest) = line.strip_prefix("Serial") {
                    if let Some(val) = rest.split(':').nth(1) {
                        let val = val.trim();
                        if !val.is_empty() {
                            return Some(val.to_string());
                        }
                    }
                }
            }
        }
    }
    None
}

/// 由板载序列号生成设备 ID：`G` + 序列号（如 `Ge33700a6620dfddc`）。
pub fn device_id_from_serial(serial: &str) -> String {
    format!("G{}", serial.trim())
}

/// 默认设备 ID：优先取板载序列号，读不到则退回占位符。
pub fn default_device_id() -> String {
    read_board_serial()
        .map(|s| device_id_from_serial(&s))
        .unwrap_or_else(|| PLACEHOLDER_DEVICE_ID.to_string())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeviceConfig {
    /// 设备 ID，格式 `G{序列号}`（默认自动取板载序列号），构成 topic 的一部分。
    pub id: String,
    /// 主板型号版本。
    pub mb: String,
    /// MCU 软件版本。
    pub sf1: String,
    /// ESP 软件版本。
    pub sf2: String,
    /// WIFI 名称（用于 login 上报）。
    pub wf: String,
    /// 语言：0 中文 / 1 非中文。
    #[serde(default = "default_lang")]
    pub lang: u8,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            id: default_device_id(),
            mb: "MB_V2".into(),
            sf1: "1.0.0".into(),
            sf2: "1.0.0".into(),
            wf: "unknown".into(),
            lang: 0,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MqttConfig {
    pub broker: String,
    pub port: u16,
    /// MQTT 心跳（秒），MXS 要求 60。
    #[serde(default = "default_keepalive")]
    pub keepalive_secs: u16,
    /// Clean Session：false = 保持会话（MXS 要求 Clean Session 0）。
    #[serde(default)]
    pub clean_session: bool,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MoonrakerConfig {
    /// 本机 Klipper 主机地址（默认 127.0.0.1）。
    #[serde(default = "default_mr_host")]
    pub host: String,
    /// Moonraker WebSocket 端口（默认 7125）。
    #[serde(default = "default_mr_port")]
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DownloadConfig {
    /// U 盘挂载目录（GCODE/固件下载落地）。
    #[serde(default = "default_dl_dir")]
    pub dir: String,
    /// 最大下载字节数。
    #[serde(default = "default_max_file")]
    pub max_file_bytes: u64,
    /// 下载分块缓冲大小。
    #[serde(default = "default_chunk")]
    pub chunk_size: usize,
}

impl Default for MqttConfig {
    fn default() -> Self {
        Self {
            broker: "127.0.0.1".into(),
            port: 1883,
            keepalive_secs: 60,
            clean_session: false,
            username: None,
            password: None,
        }
    }
}
impl Default for MoonrakerConfig {
    fn default() -> Self {
        Self { host: "127.0.0.1".into(), port: 7125 }
    }
}
impl Default for DownloadConfig {
    fn default() -> Self {
        Self { dir: default_dl_dir(), max_file_bytes: default_max_file(), chunk_size: default_chunk() }
    }
}

fn default_lang() -> u8 { 0 }
fn default_keepalive() -> u16 { 60 }
fn default_mr_host() -> String { "127.0.0.1".into() }
fn default_mr_port() -> u16 { 7125 }
fn default_dl_dir() -> String { "/mnt/udisk".into() }
fn default_max_file() -> u64 { 512 * 1024 * 1024 }
fn default_chunk() -> usize { 64 * 1024 }

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(toml::de::Error),
    Serialize(toml::ser::Error),
    Invalid(String),
}
impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Parse(e) => write!(f, "toml: {e}"),
            Self::Serialize(e) => write!(f, "toml serialize: {e}"),
            Self::Invalid(m) => write!(f, "invalid config: {m}"),
        }
    }
}
impl std::error::Error for ConfigError {}
impl From<std::io::Error> for ConfigError { fn from(e: std::io::Error) -> Self { Self::Io(e) } }
impl From<toml::de::Error> for ConfigError { fn from(e: toml::de::Error) -> Self { Self::Parse(e) } }
impl From<toml::ser::Error> for ConfigError { fn from(e: toml::ser::Error) -> Self { Self::Serialize(e) } }

impl AppConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)?;
        let cfg: Self = toml::from_str(&text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// 将配置以 TOML 格式写入指定路径（自动创建父目录）。
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.device.id.is_empty() || self.device.id.len() > 64 {
            return Err(ConfigError::Invalid("device.id 必须为非空且 ≤64 字符".into()));
        }
        if self.mqtt.port == 0 || self.mqtt.keepalive_secs == 0 {
            return Err(ConfigError::Invalid("mqtt.port / keepalive_secs 非法".into()));
        }
        if self.download.chunk_size == 0 {
            return Err(ConfigError::Invalid("download.chunk_size 非法".into()));
        }
        Ok(())
    }

    pub fn broker_addr(&self) -> Result<SocketAddr, ConfigError> {
        let host = self.mqtt.broker.trim_matches(['[', ']']);
        use std::net::ToSocketAddrs;
        (host, self.mqtt.port)
            .to_socket_addrs()
            .map_err(|e| ConfigError::Invalid(format!("broker 解析失败: {e}")))?
            .next()
            .ok_or_else(|| ConfigError::Invalid("broker 无可用地址".into()))
    }

    pub fn up_topic(&self) -> String { format!("GT/MXS/UP/{}", self.device.id) }
    pub fn down_topic(&self) -> String { format!("GT/MXS/DOWN/{}", self.device.id) }
    pub fn lwt_topic(&self) -> String { format!("GT/MXS/LWT/{}", self.device.id) }
    pub fn moonraker_ws_url(&self) -> String {
        format!("ws://{}:{}/websocket", self.moonraker.host, self.moonraker.port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_example_config() {
        let text = r#"
[device]
id = "G1234"
mb = "MB_V2"
sf1 = "V1.0"
sf2 = "V1.0"
wf = "MY_WIFI"
lang = 0

[mqtt]
broker = "127.0.0.1"
port = 1883
keepalive_secs = 60
clean_session = false

[moonraker]
host = "127.0.0.1"
port = 7125

[download]
dir = "/mnt/udisk"
"#;
        let cfg: AppConfig = toml::from_str(text).expect("parse");
        assert_eq!(cfg.device.id, "G1234");
        assert!(!cfg.mqtt.clean_session);
        assert_eq!(cfg.moonraker.port, 7125);
        assert_eq!(cfg.up_topic(), "GT/MXS/UP/G1234");
        assert_eq!(cfg.down_topic(), "GT/MXS/DOWN/G1234");
        assert_eq!(cfg.lwt_topic(), "GT/MXS/LWT/G1234");
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn reject_empty_device_id() {
        let cfg = AppConfig {
            device: DeviceConfig { id: "".into(), mb: "".into(), sf1: "".into(), sf2: "".into(), wf: "".into(), lang: 0 },
            mqtt: MqttConfig::default(),
            moonraker: MoonrakerConfig::default(),
            download: DownloadConfig::default(),
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn device_id_from_serial_ok() {
        assert_eq!(device_id_from_serial("e33700a6620dfddc"), "Ge33700a6620dfddc");
        assert_eq!(device_id_from_serial(" e33700a6620dfddc "), "Ge33700a6620dfddc");
        // 默认 id 以 G 开头且非空（读不到序列号时退回占位符，仍满足校验）
        let id = default_device_id();
        assert!(id.starts_with('G') && id.len() >= 2);
        assert!(id.len() <= 64);
    }

    #[test]
    fn default_config_valid_and_roundtrip() {
        // 默认配置应通过校验（供首次部署自动生成）
        let cfg = AppConfig::default();
        assert!(cfg.validate().is_ok(), "default config must pass validation");

        // save -> load 往返一致
        let tmp = std::env::temp_dir().join("mqtt_client_cfg_test.toml");
        cfg.save(&tmp).expect("save default config");
        let reloaded = AppConfig::load(&tmp).expect("load saved config");
        assert_eq!(reloaded.device.id, cfg.device.id);
        assert_eq!(reloaded.mqtt.broker, cfg.mqtt.broker);
        assert_eq!(reloaded.mqtt.port, cfg.mqtt.port);
        let _ = std::fs::remove_file(&tmp);
    }
}
