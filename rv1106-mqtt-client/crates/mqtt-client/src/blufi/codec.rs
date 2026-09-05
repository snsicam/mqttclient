//! 应用层负载编解码（详细设计 §4.6.6）。

pub const RECEIVED_MSG: &[u8] = b"Received SSID and password";
pub const FAILED_MSG: &[u8] = b"Wifi connection failed";

#[derive(Debug, Clone)]
pub struct Provisioning {
    pub ssid: String,
    pub pwd: String,
    pub ip: Option<String>,
    pub port: Option<u16>,
}

#[derive(Debug)]
pub enum CodecError {
    MissingField(&'static str),
    InvalidPort,
    Utf8(std::string::FromUtf8Error),
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodecError::MissingField(k) => write!(f, "missing field: {k}"),
            CodecError::InvalidPort => write!(f, "invalid port"),
            CodecError::Utf8(e) => write!(f, "utf8: {e}"),
        }
    }
}
impl std::error::Error for CodecError {}

#[derive(Debug, Clone)]
pub struct ScanItem {
    pub ssid: String,
    pub rssi: i8,
}

/// 序列化为 0x11 的 data 段（不含帧头）：每条 `len(1B)=1+ssid.len(), rssi(1B,i8), ssid(N)`。
pub fn encode_scan_list(items: &[ScanItem]) -> Vec<u8> {
    let mut v = Vec::new();
    for it in items {
        let ssid = it.ssid.as_bytes();
        v.push((1 + ssid.len()) as u8);
        v.push(it.rssi as u8);
        v.extend_from_slice(ssid);
    }
    v
}

/// 连接状态报告 0xF 的 data 段：`[0x01(opmode=STA), 0x00(已连有IP), 0x00(SoftAP连接数=0), <ssid bytes>...]`。
pub fn encode_connect_state(ssid: &str) -> Vec<u8> {
    let mut v = vec![0x01, 0x00, 0x00];
    v.extend_from_slice(ssid.as_bytes());
    v
}

/// 版本帧 0x10 的 data 段：`[major, minor]`。
pub fn encode_version(major: u8, minor: u8) -> Vec<u8> {
    vec![major, minor]
}

/// 解析 APP 下发配网信息：`SSID:<s>,PWD:<p>[,IP:<h>,PORT:<n>]\r`。
/// 字段顺序固定 SSID→PWD→IP→PORT；键名大小写敏感（严格 `SSID:`/`PWD:`/`IP:`/`PORT:`）。
pub fn parse_provisioning(raw: &[u8]) -> Result<Provisioning, CodecError> {
    let s = String::from_utf8(raw.to_vec()).map_err(CodecError::Utf8)?;
    let s = s.trim();
    let mut ssid = None;
    let mut pwd = None;
    let mut ip = None;
    let mut port = None;
    for p in s.split(',') {
        let p = p.trim();
        if let Some(v) = p.strip_prefix("SSID:") {
            ssid = Some(v.to_string());
        } else if let Some(v) = p.strip_prefix("PWD:") {
            pwd = Some(v.to_string());
        } else if let Some(v) = p.strip_prefix("IP:") {
            ip = Some(v.to_string());
        } else if let Some(v) = p.strip_prefix("PORT:") {
            port = Some(v.parse::<u16>().map_err(|_| CodecError::InvalidPort)?);
        }
    }
    let ssid = ssid.ok_or(CodecError::MissingField("SSID"))?;
    let pwd = pwd.ok_or(CodecError::MissingField("PWD"))?;
    Ok(Provisioning { ssid, pwd, ip, port })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic() {
        let p = parse_provisioning(b"SSID:home,PWD:12345678\r").unwrap();
        assert_eq!(p.ssid, "home");
        assert_eq!(p.pwd, "12345678");
        assert!(p.ip.is_none());
    }

    #[test]
    fn parse_empty_pwd_open_network() {
        let p = parse_provisioning(b"SSID:open,PWD:").unwrap();
        assert_eq!(p.ssid, "open");
        assert_eq!(p.pwd, "");
    }

    #[test]
    fn parse_ssid_with_space() {
        let p = parse_provisioning(b"SSID:My WiFi,PWD:ab cd").unwrap();
        assert_eq!(p.ssid, "My WiFi");
        assert_eq!(p.pwd, "ab cd");
    }

    #[test]
    fn parse_full_fields() {
        let p = parse_provisioning(b"SSID:s,PWD:p,IP:1.2.3.4,PORT:8883").unwrap();
        assert_eq!(p.ip, Some("1.2.3.4".to_string()));
        assert_eq!(p.port, Some(8883));
    }

    #[test]
    fn parse_missing_ssid_is_error() {
        assert!(parse_provisioning(b"PWD:x").is_err());
    }

    #[test]
    fn scan_list_encode() {
        let items = vec![ScanItem {
            ssid: "A".into(),
            rssi: -50,
        }];
        // len=1+1=2, rssi=-50 as u8 = 206, 'A'
        assert_eq!(encode_scan_list(&items), vec![2, 206, b'A']);
    }

    #[test]
    fn connect_state_encode() {
        assert_eq!(encode_connect_state("net"), vec![1, 0, 0, b'n', b'e', b't']);
    }
}
