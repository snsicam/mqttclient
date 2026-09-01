//! Moonraker WebSocket 客户端（LLD-003 §5）。
//!
//! - JSON-RPC 请求/响应关联：`pending: HashMap<id, oneshot>`；
//! - 通知分发：`notify_status_update` → state_bridge 写 AppState + 事件；
//! - 运行于独立线程（非阻塞 WS 轮询），提供同步 `MrHandle::request` 供 dispatcher 使用。

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tungstenite::client::IntoClientRequest;
use tungstenite::{Error as WsError, Message, WebSocket};

use crate::app_state::AppState;
use crate::config::MoonrakerConfig;
use crate::state::Event;

// ---------------------------------------------------------------------------
// JSON-RPC 结构
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct RpcRequest {
    pub id: u64,
    pub jsonrpc: &'static str,
    pub method: String,
    pub params: Value,
}

#[derive(Deserialize)]
pub struct RpcResponse {
    pub id: Option<u64>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<Value>,
}

#[derive(Deserialize)]
pub struct RpcNotification {
    #[serde(default)]
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

/// 订阅对象清单（LLD-003 §5.3，覆盖温度/风扇/打印/调平/硬件）。
pub const SUBSCRIBE_OBJECTS: [&str; 12] = [
    "extruder",
    "heater_bed",
    "fan",
    "heater_fan",
    "print_stats",
    "gcode_move",
    "virtual_sdcard",
    "output_pin",
    "led",
    "runout_sensor",
    "bed_mesh",
    "configfile",
];

// ---------------------------------------------------------------------------
// WebSocket 客户端（非阻塞）
// ---------------------------------------------------------------------------

pub struct MoonrakerClient {
    ws: Option<WebSocket<std::net::TcpStream>>,
    cfg: MoonrakerConfig,
}

impl MoonrakerClient {
    pub fn new(cfg: &MoonrakerConfig) -> Self {
        Self { ws: None, cfg: cfg.clone() }
    }

    pub fn is_connected(&self) -> bool { self.ws.is_some() }
    pub fn close(&mut self) { self.ws = None; }

    /// 建立连接：blocking TCP + WS 握手（读超时保护），成功后转非阻塞。
    pub fn connect(&mut self) -> Result<(), String> {
        let stream = std::net::TcpStream::connect((self.cfg.host.as_str(), self.cfg.port))
            .map_err(|e| format!("tcp connect: {e}"))?;
        stream.set_read_timeout(Some(Duration::from_secs(3))).map_err(|e| e.to_string())?;
        let url = format!("ws://{}:{}/websocket", self.cfg.host, self.cfg.port);
        let req = url.into_client_request().map_err(|e| format!("req: {e}"))?;
        let (mut ws, _) = tungstenite::client(req, stream).map_err(|e| format!("handshake: {e}"))?;
        // 握手完成：清除读超时并转非阻塞（后续由 worker 轮询）
        ws.get_mut().set_read_timeout(None).map_err(|e| e.to_string())?;
        ws.get_mut().set_nonblocking(true).map_err(|e| e.to_string())?;
        self.ws = Some(ws);
        Ok(())
    }

    pub fn send_text(&mut self, text: &str) -> Result<(), String> {
        let ws = self.ws.as_mut().ok_or_else(|| "not connected".to_string())?;
        let msg = Message::text(text);
        loop {
            match ws.write(msg.clone()) {
                Ok(()) => return Ok(()),
                Err(WsError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(WsError::Io(e)) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(format!("write: {e}")),
            }
        }
    }

    /// 非阻塞读一帧：无数据返回 Ok(None)。
    pub fn read_text(&mut self) -> Result<Option<String>, String> {
        let ws = self.ws.as_mut().ok_or_else(|| "not connected".to_string())?;
        match ws.read() {
            Ok(Message::Text(t)) => Ok(Some(t.as_str().to_string())),
            Ok(Message::Ping(p)) => {
                let _ = ws.write(Message::Pong(p));
                Ok(None)
            }
            Ok(Message::Pong(_)) => Ok(None),
            Ok(Message::Close(_)) => Err("closed by peer".into()),
            Ok(_) => Ok(None),
            Err(WsError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(WsError::Io(e)) if e.kind() == io::ErrorKind::Interrupted => Ok(None),
            Err(e) => Err(format!("read: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Worker 线程 + 命令句柄
// ---------------------------------------------------------------------------

/// 发给 worker 的 JSON-RPC 请求。
pub struct MrCommand {
    pub id: u64,
    pub method: String,
    pub params: Value,
    pub resp: mpsc::Sender<Result<Value, String>>,
}

/// dispatcher 使用的同步命令句柄。
#[derive(Clone)]
pub struct MrHandle {
    cmd_tx: mpsc::Sender<MrCommand>,
    next_id: Arc<AtomicU64>,
}

impl MrHandle {
    /// 发起 JSON-RPC 请求并阻塞等待响应（timeout）。
    pub fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.cmd_tx
            .send(MrCommand { id, method: method.into(), params, resp: tx })
            .map_err(|_| "moonraker worker stopped".to_string())?;
        rx.recv_timeout(timeout).map_err(|_| format!("moonraker timeout: {method}"))?
    }
}

pub struct MoonrakerWorker {
    cfg: MoonrakerConfig,
    state: Arc<Mutex<AppState>>,
    event_tx: mpsc::Sender<Event>,
    client: MoonrakerClient,
    pending: HashMap<u64, mpsc::Sender<Result<Value, String>>>,
}

impl MoonrakerWorker {
    pub fn spawn(cfg: MoonrakerConfig, state: Arc<Mutex<AppState>>, event_tx: mpsc::Sender<Event>) -> MrHandle {
        let (cmd_tx, cmd_rx) = mpsc::channel::<MrCommand>();
        let next_id = Arc::new(AtomicU64::new(1));
        let mut worker = Self {
            cfg: cfg.clone(),
            state,
            event_tx,
            client: MoonrakerClient::new(&cfg),
            pending: HashMap::new(),
        };
        std::thread::Builder::new()
            .name("moonraker".into())
            .spawn(move || worker.run(&cmd_rx))
            .expect("spawn moonraker worker");
        MrHandle { cmd_tx, next_id }
    }

    fn run(&mut self, cmd_rx: &mpsc::Receiver<MrCommand>) {
        let mut beat_counter: u32 = 0;
        loop {
            if !self.client.is_connected() {
                log::info!("connecting moonraker {}:{}", self.cfg.host, self.cfg.port);
                match self.client.connect() {
                    Ok(()) => {
                        self.after_connect();
                        let _ = self.event_tx.send(Event::MrConnected);
                    }
                    Err(e) => {
                        log::warn!("moonraker connect failed: {e}");
                        std::thread::sleep(Duration::from_secs(3));
                        continue;
                    }
                }
            }

            // 1. 命令队列批量发送
            let mut reqs: Vec<MrCommand> = Vec::new();
            while let Ok(c) = cmd_rx.try_recv() {
                reqs.push(c);
            }
            for c in reqs {
                self.send_request(c);
            }

            // 2. 非阻塞读
            match self.client.read_text() {
                Ok(Some(text)) => self.handle_frame(&text),
                Ok(None) => {}
                Err(e) => {
                    log::warn!("moonraker read error: {e}");
                    self.client.close();
                    self.state.lock().unwrap().moonraker_connected = false;
                    let _ = self.event_tx.send(Event::MrDisconnected);
                }
            }

            // 3. 心跳：每 ~10s 一次 server.info（保活 + 探活；id=0 不参与响应关联）
            beat_counter += 1;
            if beat_counter >= 50 {
                beat_counter = 0;
                if let Ok(text) = serde_json::to_string(&RpcRequest {
                    id: 0,
                    jsonrpc: "2.0",
                    method: "server.info".into(),
                    params: json!({}),
                }) {
                    if let Err(e) = self.client.send_text(&text) {
                        log::warn!("moonraker heartbeat write: {e}");
                        self.client.close();
                        self.state.lock().unwrap().moonraker_connected = false;
                        let _ = self.event_tx.send(Event::MrDisconnected);
                    }
                }
            }

            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn send_request(&mut self, c: MrCommand) {
        let text = serde_json::to_string(&RpcRequest {
            id: c.id,
            jsonrpc: "2.0",
            method: c.method,
            params: c.params,
        });
        match text {
            Ok(t) => match self.client.send_text(&t) {
                Ok(()) => {
                    self.pending.insert(c.id, c.resp);
                }
                Err(e) => {
                    let _ = c.resp.send(Err(e));
                }
            },
            Err(e) => {
                let _ = c.resp.send(Err(e.to_string()));
            }
        }
    }

    fn handle_frame(&mut self, text: &str) {
        // 响应（带 id）优先
        if let Ok(resp) = serde_json::from_str::<RpcResponse>(text) {
            if let Some(id) = resp.id {
                if let Some(tx) = self.pending.remove(&id) {
                    let r = match resp.result {
                        Some(v) => Ok(v),
                        None => Err(resp.error.map(|e| e.to_string()).unwrap_or_else(|| "no result".into())),
                    };
                    let _ = tx.send(r);
                }
                return;
            }
        }
        // 通知
        if let Ok(notif) = serde_json::from_str::<RpcNotification>(text) {
            self.handle_notification(&notif);
        }
    }

    fn handle_notification(&mut self, n: &RpcNotification) {
        match n.method.as_str() {
            "notify_status_update" => {
                if let Some(params) = &n.params {
                    if let Some(map) = params.get(0).and_then(|v| v.as_object()) {
                        state_bridge::apply(&mut self.state.lock().unwrap(), map);
                    }
                }
                let _ = self.event_tx.send(Event::StatusUpdated);
            }
            "notify_klippy_ready" => {
                self.state.lock().unwrap().moonraker_connected = true;
                let _ = self.event_tx.send(Event::MrConnected);
            }
            "notify_klippy_disconnected" => {
                self.state.lock().unwrap().moonraker_connected = false;
                let _ = self.event_tx.send(Event::MrDisconnected);
            }
            "notify_gcode_response" => { /* gcode 结果走 RPC 响应，忽略 */ }
            "notify_filelist_changed" => { /* file_list 用 RPC 查询，忽略 */ }
            _ => {}
        }
    }

    fn after_connect(&mut self) {
        // 订阅对象（id=0，响应不关联）
        let sub = RpcRequest {
            id: 0,
            jsonrpc: "2.0",
            method: "printer.objects.subscribe".into(),
            params: json!({ "objects": SUBSCRIBE_OBJECTS }),
        };
        if let Ok(t) = serde_json::to_string(&sub) {
            let _ = self.client.send_text(&t);
        }
        // 主动全量查询一次（补齐订阅通知外的状态）
        let q = RpcRequest {
            id: 0,
            jsonrpc: "2.0",
            method: "printer.objects.query".into(),
            params: json!({ "objects": SUBSCRIBE_OBJECTS }),
        };
        if let Ok(t) = serde_json::to_string(&q) {
            let _ = self.client.send_text(&t);
        }
        self.state.lock().unwrap().moonraker_connected = true;
    }
}

// ---------------------------------------------------------------------------
// state_bridge：notify_status_update → AppState
// ---------------------------------------------------------------------------

pub mod state_bridge {
    use super::*;

    /// 将 notify_status_update 的对象表写入 AppState。
    pub fn apply(s: &mut AppState, map: &Map<String, Value>) {
        for (obj, v) in map {
            let Some(attrs) = v.as_object() else { continue };
            match obj.as_str() {
                "extruder" => {
                    num(attrs, "temperature", &mut s.nozzle_actual);
                    num(attrs, "target", &mut s.nozzle_target);
                }
                "heater_bed" => {
                    num(attrs, "temperature", &mut s.bed_actual);
                    num(attrs, "target", &mut s.bed_target);
                }
                "fan" => pct(attrs, "speed", &mut s.main_fan_pct),
                "heater_fan" => pct(attrs, "speed", &mut s.board_fan_pct),
                "print_stats" => {
                    if let Some(ps) = str_attr(attrs, "state") {
                        s.print_state = crate::app_state::PrintState::from_klipper(&ps) as u8;
                    }
                    if let Some(f) = str_attr(attrs, "filename") {
                        if !f.is_empty() {
                            s.current_file = f;
                        }
                    }
                    if let Some(pr) = num_opt(attrs, "progress") {
                        s.print_progress = (pr.clamp(0.0, 1.0) * 100.0).round() as u8;
                    }
                    if let Some(pd) = num_opt(attrs, "print_duration") {
                        s.print_elapsed_secs = pd as u64;
                    }
                    if let Some(td) = num_opt(attrs, "total_duration") {
                        s.print_remain_secs = (td - s.print_elapsed_secs as f64).max(0.0) as u64;
                    }
                }
                "virtual_sdcard" => {
                    if let Some(p) = num_opt(attrs, "progress") {
                        s.print_progress = (p.clamp(0.0, 1.0) * 100.0).round() as u8;
                    }
                    // sd 可用性由 Moonraker 连接状态决定（见 is_sd_ok）
                    s.sd_ok = s.moonraker_connected;
                }
                "gcode_move" => {
                    if let Some(origin) = attrs.get("homing_origin").and_then(Value::as_array) {
                        if origin.len() >= 3 {
                            if let Some(z) = origin[2].as_f64() {
                                s.zoffset_x100 = (z * 100.0) as i32;
                            }
                        }
                    }
                }
                "output_pin" => {
                    for (pin, pv) in attrs {
                        let on = pv
                            .as_object()
                            .and_then(|m| m.get("value"))
                            .and_then(Value::as_f64)
                            .unwrap_or(0.0)
                            > 0.5;
                        if pin.contains("light") {
                            s.light = on;
                        } else if pin.contains("beep") {
                            s.beep = on;
                        }
                    }
                }
                "led" => {
                    for (_name, lv) in attrs {
                        if let Some(st) = lv.as_object().and_then(|m| m.get("state")).and_then(Value::as_str) {
                            if st != "off" {
                                s.breathing = true;
                            }
                        }
                    }
                }
                "runout_sensor" => {
                    for (_name, rv) in attrs {
                        if let Some(r) = rv.as_object().and_then(|m| m.get("runout")).and_then(Value::as_bool) {
                            s.runout = r;
                        }
                    }
                }
                "bed_mesh" => {
                    if let Some(profiles) = attrs.get("profiles").and_then(Value::as_array) {
                        s.level_ok = !profiles.is_empty();
                    }
                }
                _ => {}
            }
        }
    }

    fn num(m: &Map<String, Value>, k: &str, out: &mut f64) {
        if let Some(v) = m.get(k).and_then(Value::as_f64) {
            *out = v;
        }
    }
    fn num_opt(m: &Map<String, Value>, k: &str) -> Option<f64> {
        m.get(k).and_then(Value::as_f64)
    }
    fn str_attr(m: &Map<String, Value>, k: &str) -> Option<String> {
        m.get(k).and_then(Value::as_str).map(str::to_string)
    }
    fn pct(m: &Map<String, Value>, k: &str, out: &mut u8) {
        if let Some(v) = m.get(k).and_then(Value::as_f64) {
            *out = (v.clamp(0.0, 1.0) * 100.0).round() as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::state_bridge;
    use crate::app_state::AppState;
    use serde_json::json;

    #[test]
    fn bridge_applies_temps_and_print() {
        let mut s = AppState::default();
        let map = json!({
            "extruder": {"temperature": 201.5, "target": 200.0},
            "heater_bed": {"temperature": 60.2, "target": 60.0},
            "print_stats": {"state": "printing", "progress": 0.42, "print_duration": 100.0, "total_duration": 300.0, "filename": "a.gcode"},
            "fan": {"speed": 0.75},
        });
        state_bridge::apply(&mut s, map.as_object().unwrap());
        assert_eq!(s.nozzle_actual, 201.5);
        assert_eq!(s.nozzle_target, 200.0);
        assert_eq!(s.bed_actual, 60.2);
        assert_eq!(s.print_state, 1);
        assert_eq!(s.print_progress, 42);
        assert_eq!(s.print_elapsed_secs, 100);
        assert_eq!(s.print_remain_secs, 200);
        assert_eq!(s.current_file, "a.gcode");
        assert_eq!(s.main_fan_pct, 75);
    }

    #[test]
    fn bridge_handles_runout_and_mesh() {
        let mut s = AppState::default();
        let map = json!({
            "runout_sensor": {"sensor": {"runout": true, "enabled": true}},
            "bed_mesh": {"profiles": ["default"]},
            "gcode_move": {"homing_origin": [0.0, 0.0, -0.15]},
        });
        state_bridge::apply(&mut s, map.as_object().unwrap());
        assert!(s.runout);
        assert!(s.level_ok);
        assert_eq!(s.zoffset_x100, -15);
    }

    #[test]
    fn rpc_request_serializes() {
        let req = super::RpcRequest { id: 7, jsonrpc: "2.0", method: "printer.gcode.script".into(), params: json!({"script": "G28"}) };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"id\":7"));
        assert!(s.contains("\"method\":\"printer.gcode.script\""));
    }
}
