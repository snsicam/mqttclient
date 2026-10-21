//! MXS 协议（V2.1.2）上下行 12 类包编解码（LLD-003 §8）。
//!
//! 说明：
//! - 字段名保持 R1 协议原样（驼峰），上行用 `serde_json::json!` 组装，下行宽松反序列化。
//! - 时间戳 `ts` 为 UNIX 秒（u64）。

use std::fmt;

use serde::Deserialize;
use serde_json::{json, Value};

/// 当前 UNIX 秒。
pub fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug)]
pub struct ProtocolError(pub String);
impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "protocol: {}", self.0) }
}
impl std::error::Error for ProtocolError {}

// ---------------------------------------------------------------------------
// 下行（云 → 设备）统一信封
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DownlinkMsg {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub ts: Option<u64>,
    // login 回复
    #[serde(rename = "bindState", default)]
    pub bind_state: Option<u8>,
    #[serde(default)]
    pub account: Option<String>,
    // gcode
    #[serde(rename = "gcodeCmd", default)]
    pub gcode_cmd: Option<String>,
    // download
    #[serde(rename = "fileType", default)]
    pub file_type: Option<u8>,
    #[serde(rename = "serverIp", default)]
    pub server_ip: Option<String>,
    #[serde(rename = "fileName", default)]
    pub file_name: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(rename = "ackState", default)]
    pub ack_state: Option<String>,
    // upgrade 回复
    #[serde(rename = "mcuFile", default)]
    pub mcu_file: Option<String>,
    #[serde(rename = "espFile", default)]
    pub esp_file: Option<String>,
}

impl DownlinkMsg {
    pub fn parse(payload: &[u8]) -> Result<Self, ProtocolError> {
        serde_json::from_slice(payload).map_err(|e| ProtocolError(e.to_string()))
    }
}

/// 下行包类型常量。
pub const KIND_LOGIN_REPLY: &str = "login";
pub const KIND_STATUS_QUERY: &str = "status_query";
pub const KIND_GCODE: &str = "gcode";
pub const KIND_DOWNLOAD_BEGIN: &str = "download_begin";
pub const KIND_DOWNLOAD_END_ACK: &str = "download_end";
pub const KIND_UPGRADE_QUERY: &str = "upgrade_query";
pub const KIND_FILE_LIST: &str = "file_list";
pub const KIND_SERVER_UNBIND: &str = "server_unbind";
pub const KIND_DEVICE_UNBIND_REPLY: &str = "device_unbind";

// ---------------------------------------------------------------------------
// 上行（设备 → 云）构建
// ---------------------------------------------------------------------------

pub struct UplinkMsg;

impl UplinkMsg {
    /// login：设备登录（R1 §5.1）。
    pub fn login(device: &crate::config::DeviceConfig, ip: &str, ts: u64) -> Vec<u8> {
        json_to_bytes(json!({
            "type": "login",
            "id": device.id,
            "mb": device.mb,
            "sf1": device.sf1,
            "sf2": device.sf2,
            "wf": device.wf,
            "ip": ip,
            "lang": device.lang,
            "ts": ts,
        }))
    }

    /// alarm：设备告警（errType 1~20）。
    pub fn alarm(id: &str, err_type: u8, err_msg: &str, ts: u64) -> Vec<u8> {
        json_to_bytes(json!({
            "type": "alarm",
            "id": id,
            "errType": err_type,
            "errMsg": err_msg,
            "ts": ts,
        }))
    }

    /// status_hardware：硬件状态（runout/beep/light/Breathing/sd/level/door）。
    pub fn status_hardware(id: &str, s: &crate::AppState, ts: u64) -> Vec<u8> {
        json_to_bytes(json!({
            "type": "status_hardware",
            "id": id,
            "runout": if s.runout { 1 } else { 0 },
            "beep": if s.beep { 1 } else { 0 },
            "light": if s.light { 1 } else { 0 },
            "Breathing": if s.breathing { 1 } else { 0 },
            "sd": if s.sd_ok { 1 } else { 0 },
            "level": if s.level_ok { 1 } else { 0 },
            "door": if s.door_open { 1 } else { 0 },
            "ts": ts,
        }))
    }

    /// status_temp_fan：温度与风扇。
    pub fn status_temp_fan(id: &str, s: &crate::AppState, ts: u64) -> Vec<u8> {
        json_to_bytes(json!({
            "type": "status_temp_fan",
            "id": id,
            "preheatType": s.preheat_type,
            "preheatState": s.preheat_state,
            "heatState": s.heat_state,
            "nozzleTargetTemp": (s.nozzle_target as i64),
            "nozzleActualTemp": (s.nozzle_actual as i64),
            "bedTargetTemp": (s.bed_target as i64),
            "bedActualTemp": (s.bed_actual as i64),
            "mainFanSpeed": s.main_fan_pct,
            "boardFanSpeed": s.board_fan_pct,
            "auxFanSpeed": s.aux_fan_pct,
            "ts": ts,
        }))
    }

    /// status_level：调平状态。
    pub fn status_level(id: &str, s: &crate::AppState, ts: u64) -> Vec<u8> {
        json_to_bytes(json!({
            "type": "status_level",
            "id": id,
            "levelingStatus": s.leveling_status,
            "levelingPoint": s.leveling_point,
            "levelingTotalPoints": s.leveling_total,
            "ts": ts,
        }))
    }

    /// status_print：打印状态（elapsed/remain 拆分为 h/m/s）。
    pub fn status_print(id: &str, s: &crate::AppState, ts: u64) -> Vec<u8> {
        let el = split_hms(s.print_elapsed_secs);
        let rm = split_hms(s.print_remain_secs);
        json_to_bytes(json!({
            "type": "status_print",
            "id": id,
            "printState": s.print_state,
            "zOffset": s.zoffset_x100,
            "printProgress": s.print_progress,
            "printElapsedTime": { "hour": el.0, "min": el.1, "sec": el.2 },
            "printRemainTime": { "hour": rm.0, "min": rm.1, "sec": rm.2 },
            "currentPrintFile": s.current_file,
            "ts": ts,
        }))
    }

    /// gcode：执行结果回复。
    pub fn gcode_reply(id: &str, cmd_type: &str, exec_result: &str, ts: u64) -> Vec<u8> {
        json_to_bytes(json!({
            "type": "gcode",
            "id": id,
            "cmdType": cmd_type,
            "execResult": exec_result,
            "ts": ts,
        }))
    }

    /// download_begin 回复 / download_end 上报。
    pub fn download_report(id: &str, kind: &str, file_name: &str, file_type: u8, trans_state: &str, err_code: u8, ts: u64) -> Vec<u8> {
        json_to_bytes(json!({
            "type": kind,
            "id": id,
            "fileName": file_name,
            "fileType": file_type,
            "transState": trans_state,
            "errCode": err_code,
            "ts": ts,
        }))
    }

    /// device_unbind：设备解绑（上行）。
    pub fn device_unbind(id: &str, ts: u64) -> Vec<u8> {
        json_to_bytes(json!({ "type": "device_unbind", "id": id, "ts": ts }))
    }

    /// file_list：文件列表回复（分片，fileIndex 从 0 起）。
    pub fn file_list_reply(id: &str, file_total: usize, file_index: usize, files: &[(String, u64)], ts: u64) -> Vec<u8> {
        let list: Vec<Value> = files
            .iter()
            .map(|(n, sz)| json!({ "fileName": n, "fileSize": sz }))
            .collect();
        json_to_bytes(json!({
            "type": "file_list",
            "id": id,
            "fileTotal": file_total,
            "fileIndex": file_index,
            "fileList": list,
            "ts": ts,
        }))
    }

    /// upgrade_query：设备发起升级查询。
    pub fn upgrade_query(id: &str, ts: u64) -> Vec<u8> {
        json_to_bytes(json!({ "type": "upgrade_query", "id": id, "ts": ts }))
    }
}

fn json_to_bytes(v: Value) -> Vec<u8> {
    serde_json::to_vec(&v).unwrap_or_default()
}

fn split_hms(total_secs: u64) -> (u64, u64, u64) {
    (total_secs / 3600, (total_secs % 3600) / 60, total_secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::AppState;

    #[test]
    fn parse_downlink_gcode() {
        let raw = br#"{"type":"gcode","id":"A1","ts":1234567890,"gcodeCmd":"G28"}"#;
        let msg = DownlinkMsg::parse(raw).unwrap();
        assert_eq!(msg.kind, KIND_GCODE);
        assert_eq!(msg.gcode_cmd.as_deref(), Some("G28"));
        assert_eq!(msg.ts, Some(1234567890));
    }

    #[test]
    fn parse_downlink_download_begin() {
        let raw = br#"{"type":"download_begin","fileType":0,"serverIp":"10.0.0.1","fileName":"a.gcode","ts":1}"#;
        let msg = DownlinkMsg::parse(raw).unwrap();
        assert_eq!(msg.file_type, Some(0));
        assert_eq!(msg.file_name.as_deref(), Some("a.gcode"));
        assert_eq!(msg.url, None);
    }

    #[test]
    fn build_login() {
        let d = crate::config::DeviceConfig {
            id: "G9".into(), mb: "MB".into(), sf1: "1".into(), sf2: "2".into(), wf: "W".into(), lang: 0,
        };
        let v: Value = serde_json::from_slice(&UplinkMsg::login(&d, "192.168.1.1", 42)).unwrap();
        assert_eq!(v["type"], "login");
        assert_eq!(v["id"], "G9");
        assert_eq!(v["ip"], "192.168.1.1");
        assert_eq!(v["ts"], 42);
    }

    #[test]
    fn build_status_hardware() {
        let s = AppState { runout: true, beep: false, light: true, breathing: false, sd_ok: true, level_ok: false, door_open: true, ..Default::default() };
        let v: Value = serde_json::from_slice(&UplinkMsg::status_hardware("G9", &s, 7)).unwrap();
        assert_eq!(v["id"], "G9"); // id 必须带设备号，否则平台无法识别来源
        assert_eq!(v["runout"], 1);
        assert_eq!(v["Breathing"], 0); // 协议字段名保持大写 B
        assert_eq!(v["sd"], 1);
        assert_eq!(v["door"], 1);
    }

    #[test]
    fn build_status_print_hms() {
        let s = AppState { print_state: 1, print_elapsed_secs: 3661, print_remain_secs: 61, print_progress: 50, current_file: "x.gcode".into(), zoffset_x100: -15, ..Default::default() };
        let v: Value = serde_json::from_slice(&UplinkMsg::status_print("G9", &s, 0)).unwrap();
        assert_eq!(v["id"], "G9");
        assert_eq!(v["printState"], 1);
        assert_eq!(v["printElapsedTime"]["hour"], 1);
        assert_eq!(v["printElapsedTime"]["min"], 1);
        assert_eq!(v["printElapsedTime"]["sec"], 1);
        assert_eq!(v["printRemainTime"]["sec"], 1);
        assert_eq!(v["zOffset"], -15);
        assert_eq!(v["currentPrintFile"], "x.gcode");
    }

    #[test]
    fn build_file_list_reply() {
        let files = vec![("a.gcode".to_string(), 100u64), ("b.gcode".to_string(), 200u64)];
        let v: Value = serde_json::from_slice(&UplinkMsg::file_list_reply("G9", 2, 0, &files, 3)).unwrap();
        assert_eq!(v["id"], "G9");
        assert_eq!(v["fileTotal"], 2);
        assert_eq!(v["fileIndex"], 0);
        assert_eq!(v["fileList"][0]["fileName"], "a.gcode");
    }
}
