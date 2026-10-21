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

/// 同 [`encode_scan_list`]，但编码时累计超过 `max_bytes` 即停止（保留前缀）。
/// 调用方须预先按优先级排序（如 RSSI 降序），以便截断时丢弃最不重要的条目。
///
/// 用途：某些对端 APP 未实现 BluFi 分片重组（收到带 `FRAGMENTED` 的首片会 `StopNotify`
/// 断开），而 BluFi 帧 `data_len` 为 u8（≤255）无法单帧承载大列表。此时把热点数压到单帧
/// 能装下，比发分片被对端断开、一个都收不到更好。APP 支持分片后，直接改用
/// [`encode_scan_list`] 全量即可——`send_data` 会自动按 255 切片（每片 data ≤ 255，
/// `data_len` 字段不截断）。
pub fn encode_scan_list_fitting(items: &[ScanItem], max_bytes: usize) -> Vec<u8> {
    let mut v = Vec::new();
    for it in items {
        let ssid = it.ssid.as_bytes();
        let len_field = (1 + ssid.len()) as u8; // len(1B) = 1(rssi) + ssid.len()
        let entry = 1 + 1 + ssid.len(); // 整条字节: len(1) + rssi(1) + ssid(N)
        // 至少保留第一条，避免空列表（空列表由调用方单独保证）
        if !v.is_empty() && v.len() + entry > max_bytes {
            break;
        }
        v.push(len_field);
        v.push(it.rssi as u8);
        v.extend_from_slice(ssid);
    }
    v
}

/// 连接状态报告 0xF 的 data 段：**3 字节**
/// `[0x01(opmode=STA), sta_state, 0x00(SoftAP 连接数)]`
/// （`0x0` 已连有 IP / `0x1` 断开 / `0x2` 连接中 / `0x3` 已连无 IP）
///
/// 依据 `BlufiClientImpl.parseWifiState()`：
/// 1. 先判 `data.length < 3` → 直接 `onStatusResponse(CODE_INVALID_DATA)`，
//     **因此不能只发 2 字节**（早期按 ESP32 实抓包 `3F 00 05 02 01 00` 只发 2 字节会失败）；
/// 2. 依次读 opMode / staConnectionStatus / softAPConnectionCount；
/// 3. 剩余字节按 **(infoType, len, value) 三元组** 循环解析（SSID/BSSID 属可选扩展，
///    infoType 见 `BlufiParameter.Type.Data.SUBTYPE_STA_WIFI_SSID` 等）。
///    所以「直接拼 SSID 字节」也会被当 infoType/len 而解析失败——两者都不对。
/// 不带扩展字段时解析结束即 `STATUS_SUCCESS`，APP 判配网成功。
pub fn encode_connect_state(sta_state: u8) -> Vec<u8> {
    vec![0x01, sta_state, 0x00]
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
        // 与库 parseWifiState 一致：3 字节 [opmode=STA, sta_state, softAP 连接数=0]
        assert_eq!(encode_connect_state(0x00), vec![1, 0, 0]); // 已连有 IP
        assert_eq!(encode_connect_state(0x01), vec![1, 1, 0]); // 断开
    }

    #[test]
    fn connect_state_has_at_least_three_bytes() {
        // 回归：库 parseWifiState 先判 data.length < 3 → CODE_INVALID_DATA，
        // 少于 3 字节（如只发 opmode+state）会导致 APP 判配网失败。
        assert_eq!(encode_connect_state(0x00).len(), 3);
    }

    #[test]
    fn scan_list_fitting_truncates() {
        // 调用方应已排序（RSSI 降序）；此处按传入顺序累加验证截断逻辑。
        // 编码: A len=2 entry=3B, BB len=3 entry=4B, CCC len=4 entry=5B
        let items = vec![
            ScanItem { ssid: "A".into(), rssi: -30 },
            ScanItem { ssid: "BB".into(), rssi: -40 },
            ScanItem { ssid: "CCC".into(), rssi: -50 },
        ];
        // 上限 6: A(3) 后 BB(4) 超界 -> 仅 A
        let d = encode_scan_list_fitting(&items, 6);
        assert_eq!(d, vec![2, (-30i8) as u8, b'A']);
        // 上限 7: A(3)+BB(4)=7 <= 7 -> A, BB
        let d2 = encode_scan_list_fitting(&items, 7);
        assert_eq!(d2, vec![2, (-30i8) as u8, b'A', 3, (-40i8) as u8, b'B', b'B']);
        // 上限 255: 全量
        let d3 = encode_scan_list_fitting(&items, 255);
        assert_eq!(d3, encode_scan_list(&items));
    }
}
