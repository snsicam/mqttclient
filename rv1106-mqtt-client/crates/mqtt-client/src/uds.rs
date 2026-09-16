//! UI 控制通道（Unix domain socket，LLD-003 §x）。
//!
//! klipper_screen（C++ UI）经此 socket 下发云侧操作（下载 / 解绑），
//! 由 `Dispatcher` 经 Moonraker / 云端执行后回传结果。该通道与 MQTT 下行通道并列，
//! 二者都汇入 `Dispatcher`，保持单一的指令执行点（避免双写 `State` 竞争）。
//!
//! ## 协议
//! - 传输：Unix domain socket，默认 `/run/mqtt-client/ui.sock`。
//! - 封帧：每条消息 = `[u32 LE 长度][UTF-8 JSON]`，单条上限 64 KiB。
//! - 请求（UI → mqtt-client）：
//!   ```json
//!   { "id": "u1", "method": "download",
//!     "params": { "file_type": 0, "file_name": "a.gcode", "url": "https://..." } }
//!   ```
//! - 响应（mqtt-client → UI，匹配 `id`）：
//!   ```json
//!   { "id": "u1", "ok": true,  "result": { "err_code": 0, "dest": "~/printer_data/gcodes/cloud/a.gcode" } }
//!   { "id": "u1", "ok": false, "error": "download failed: code 1" }
//!   ```
//!
//! ## 方法
//! | method        | params                                  | result                          |
//! |---------------|-----------------------------------------|---------------------------------|

//! | `bind_status` | —                                       | 云端/绑定状态（本地应答）       |
//! | `download`    | `file_type,file_name,url`               | `{"err_code","dest?"}`          |
//! | `unbind`      | —                                       | `{"ok": true}`（异步上发解绑包）|

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::UdsConfig;
use crate::downlink::{UiCmd, UiReply};
use crate::state::SharedUiState;

/// 单条消息最大长度（64 KiB），防止畸形/超大包撑爆内存。
const MAX_MSG_LEN: usize = 64 * 1024;

/// 启动 UDS 服务线程（配置关闭时直接返回，不监听）。
pub fn spawn(cfg: UdsConfig, ui_cmd_tx: mpsc::Sender<UiCmd>, ui_state: SharedUiState) {
    if !cfg.enabled {
        log::info!("UDS control channel disabled (uds.enabled=false)");
        return;
    }
    std::thread::Builder::new()
        .name("uds-server".into())
        .spawn(move || run_server(&cfg, ui_cmd_tx, ui_state))
        .expect("spawn uds-server");
}

fn run_server(cfg: &UdsConfig, ui_cmd_tx: mpsc::Sender<UiCmd>, ui_state: SharedUiState) {
    let path = cfg.socket_path.clone();
    if let Some(parent) = Path::new(&path).parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                log::warn!("UDS mkdir {parent:?}: {e}");
            }
            // AV-3/OQ-U8：限制 socket 目录为 0770（owner+group），避免本机其他用户访问。
            // 组归属需部署侧（systemd `Group=`/`UMask=`）保证两进程同组。
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Err(e) = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o770)) {
                    log::warn!("UDS chmod 0770 {parent:?}: {e}");
                }
            }
        }
    }
    // 清理上次残留 socket（已存在的 socket 文件会导致 bind 失败）。
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            log::error!("UDS bind {path} failed: {e}");
            return;
        }
    };
    log::info!("UDS control channel listening on {path}");
    // AV-3：socket 文件 0660（owner+group 可读写）。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660)) {
            log::warn!("UDS chmod 0660 {path}: {e}");
        }
    }
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let tx = ui_cmd_tx.clone();
                let st = ui_state.clone();
                std::thread::spawn(move || handle_conn(stream, tx, st));
            }
            Err(e) => log::warn!("UDS accept: {e}"),
        }
    }
}

fn handle_conn(mut stream: UnixStream, ui_cmd_tx: mpsc::Sender<UiCmd>, ui_state: SharedUiState) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    loop {
        let req = match read_frame(&mut stream) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return, // 对端关闭
            Err(e) => {
                let _ = write_error(&mut stream, "", &format!("frame error: {e}"));
                return;
            }
        };
        let resp = dispatch(&req, &ui_cmd_tx, &ui_state);
        if write_frame(&mut stream, &resp).is_err() {
            return;
        }
    }
}

/// 解析请求 JSON 并分发到对应处理（本地应答或转发 Dispatcher）。
fn dispatch(req: &[u8], ui_cmd_tx: &mpsc::Sender<UiCmd>, ui_state: &SharedUiState) -> Vec<u8> {
    let v: Value = match serde_json::from_slice(req) {
        Ok(v) => v,
        Err(e) => return error_json("", &format!("invalid json: {e}")),
    };
    let id = v.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let method = v.get("method").and_then(Value::as_str).unwrap_or("");
    let params = v.get("params").cloned().unwrap_or(Value::Null);

    log::info!("[uds] recv: method={method} id={id}");

    match method {
        "bind_status" => {
            let s = ui_state.lock().unwrap();
            ok_json(&id, json!({
                "cloud_connected": s.cloud_connected,
                "moonraker_connected": s.moonraker_connected,
                "bound": s.bound,
                "bind_state": s.bind_state,
                "account": s.account,
            }))
        }
        "download" => {
            // 忙闲守卫（AV-1）：与 MQTT `download_begin`（或另一次 UDS 下载）共用
            // `UiState.downloading`，已有下载进行中则直接回 `err_code=1`（协议忙语义）。
            {
                let mut s = ui_state.lock().unwrap();
                if s.downloading {
                    return ok_json(&id, json!({ "err_code": 1 }));
                }
                s.downloading = true;
            }
            let file_type = params.get("file_type").and_then(Value::as_u64).unwrap_or(0) as u8;
            let file_name = params.get("file_name").and_then(Value::as_str).unwrap_or("").to_string();
            let url = params.get("url").and_then(Value::as_str).map(|s| s.to_string());
            let resp = match request(ui_cmd_tx, |tx| UiCmd::Download { file_type, file_name, url, reply_tx: tx }) {
                Some(reply) => reply_to_json(&id, reply),
                None => error_json(&id, "dispatcher unavailable"),
            };
            // 无论成功/失败/超时都清位，避免永久占用。
            ui_state.lock().unwrap().downloading = false;
            resp
        }
        "unbind" => {
            log::info!("[uds] unbind: 已下发 device_unbind 上行");
            match request(ui_cmd_tx, |tx| UiCmd::Unbind { reply_tx: tx }) {
                Some(reply) => reply_to_json(&id, reply),
                None => error_json(&id, "dispatcher unavailable"),
            }
        }
        other => error_json(&id, &format!("unknown method: {other}")),
    }
}

/// 构造 reply 通道，将 `UiCmd` 发给 Dispatcher 并等待结果（60s 超时）。
fn request<F>(ui_cmd_tx: &mpsc::Sender<UiCmd>, build: F) -> Option<UiReply>
where
    F: FnOnce(mpsc::Sender<UiReply>) -> UiCmd,
{
    let (tx, rx) = mpsc::channel::<UiReply>();
    if ui_cmd_tx.send(build(tx)).is_err() {
        return None;
    }
    rx.recv_timeout(Duration::from_secs(60)).ok()
}

fn reply_to_json(id: &str, reply: UiReply) -> Vec<u8> {
    match reply {
        UiReply::Ok(result) => ok_json(id, result),
        UiReply::Err(e) => error_json(id, &e),
    }
}

fn ok_json(id: &str, result: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({ "id": id, "ok": true, "result": result })).unwrap_or_default()
}

fn error_json(id: &str, msg: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({ "id": id, "ok": false, "error": msg })).unwrap_or_default()
}

fn write_error(stream: &mut UnixStream, id: &str, msg: &str) -> std::io::Result<()> {
    write_frame(stream, &error_json(id, msg))
}

/// 读一帧：`[u32 LE 长度][payload]`。返回 `None` 表示对端已关闭（读到 EOF）。
fn read_frame(stream: &mut UnixStream) -> Result<Option<Vec<u8>>, String> {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.to_string()),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 || len > MAX_MSG_LEN {
        return Err(format!("bad frame len {len}"));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).map_err(|e| e.to_string())?;
    Ok(Some(buf))
}

/// 写一帧：`[u32 LE 长度][payload]`。
fn write_frame(stream: &mut UnixStream, payload: &[u8]) -> std::io::Result<()> {
    if payload.len() > MAX_MSG_LEN {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "payload too large"));
    }
    let len = (payload.len() as u32).to_le_bytes();
    stream.write_all(&len)?;
    stream.write_all(payload)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn frame_roundtrip() {
        let (mut a, mut b) = unix_pair();
        let payload = br#"{"id":"x","ok":true}"#;
        write_frame(&mut a, payload).unwrap();
        let got = read_frame(&mut b).unwrap().unwrap();
        assert_eq!(got, payload);
    }

    #[test]
    fn dispatch_unknown() {
        let st: SharedUiState = Arc::new(Mutex::new(Default::default()));
        let (tx, _rx) = mpsc::channel::<UiCmd>();
        let resp = dispatch(br#"{"id":"2","method":"nope"}"#, &tx, &st);
        let v: Value = serde_json::from_slice(&resp).unwrap();
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("unknown method"));
    }

    /// 创建一对互联的 UnixStream（仅测试用）。
    fn unix_pair() -> (UnixStream, UnixStream) {
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir();
        let path = dir.join(format!("uds_test_{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = UnixStream::connect(&path).unwrap();
        let (server, _addr) = listener.accept().unwrap();
        let _ = std::fs::remove_file(&path);
        (client, server)
    }
}
