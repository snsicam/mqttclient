//! Moonraker WebSocket 客户端（LLD-003 §5）。
//!
//! - JSON-RPC 请求/响应关联：`pending: HashMap<id, oneshot>`；
//! - 通知分发：`notify_status_update` → state_bridge 写 AppState + 事件；
//! - 运行于独立线程（非阻塞 WS 轮询），提供同步 `MrHandle::request` 供 dispatcher 使用。

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

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
                Ok(()) => break,
                Err(WsError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(WsError::Io(e)) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(format!("write: {e}")),
            }
        }
        self.flush()
    }

    /// tungstenite 0.26 默认 write_buffer_size=128KB：小帧只入内部缓冲、不落 socket，
    /// 必须显式 flush。否则请求与自动应答的 Pong 全部滞留 —— Moonraker 每 10s Ping
    /// 收不到 Pong，30s ping 超时踢连接，表现为「连接上但无任何响应/数据」。
    fn flush(&mut self) -> Result<(), String> {
        let ws = self.ws.as_mut().ok_or_else(|| "not connected".to_string())?;
        loop {
            match ws.flush() {
                Ok(()) => return Ok(()),
                Err(WsError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(WsError::Io(e)) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(format!("flush: {e}")),
            }
        }
    }

    /// 非阻塞读一帧：无数据返回 Ok(None)。
    pub fn read_text(&mut self) -> Result<Option<String>, String> {
        let ws = self.ws.as_mut().ok_or_else(|| "not connected".to_string())?;
        match ws.read() {
            Ok(Message::Text(t)) => Ok(Some(t.as_str().to_string())),
            Ok(Message::Ping(_)) => {
                // read() 已自动排队 Pong，flush 将其送出（不回应答会被 Moonraker 踢）
                if let Err(e) = ws.flush() {
                    log::warn!("moonraker pong flush: {e}");
                }
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
    /// 响应回传端：`None` 表示 fire-and-forget（不注册 pending，响应按未知 id 丢弃）。
    pub resp: Option<mpsc::Sender<Result<Value, String>>>,
    /// no-wait 语义：`resp` 只回传「写入 websocket」的结果（成功即回 `Ok`），
    /// 不注册 pending；随后的 RPC 响应按未知 id 处理（错误记日志）。
    pub no_wait: bool,
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
            .send(MrCommand { id, method: method.into(), params, resp: Some(tx), no_wait: false })
            .map_err(|_| "moonraker worker stopped".to_string())?;
        rx.recv_timeout(timeout).map_err(|_| format!("moonraker timeout: {method}"))?
    }

    /// 发起请求但不等待 RPC 响应（fire-and-forget）：等待「写入 websocket」回执
    /// （≤5s），写入失败立即返回 Err。用于长耗时命令（如 BED_MESH_CALIBRATE）——
    /// 执行结果经状态通知上报，不能阻塞应答；RPC 层错误（如 Klippy 未连接）按
    /// 未知 id 记日志，由调用侧的调平看门狗兜底。
    pub fn request_no_wait(&self, method: &str, params: Value) -> Result<(), String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.cmd_tx
            .send(MrCommand { id, method: method.into(), params, resp: Some(tx), no_wait: true })
            .map_err(|_| "moonraker worker stopped".to_string())?;
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(format!("moonraker timeout: {method}")),
        }
    }
}

pub struct MoonrakerWorker {
    cfg: MoonrakerConfig,
    state: Arc<Mutex<AppState>>,
    event_tx: mpsc::Sender<Event>,
    client: MoonrakerClient,
    pending: HashMap<u64, mpsc::Sender<Result<Value, String>>>,
    /// 内部请求（objects.list/subscribe）id：与 MrHandle 的 id 空间隔离（≥1e6），
    /// 避免与 dispatcher 并发请求的 pending 条目冲突。
    internal_next_id: u64,
    /// 待回包的 objects.list / objects.subscribe 响应接收端。
    pending_list: Option<mpsc::Receiver<Result<Value, String>>>,
    pending_sub: Option<mpsc::Receiver<Result<Value, String>>>,
    /// 订阅流程失败后的下次重试时刻（None=无需重试）。
    sub_retry_at: Option<Instant>,
}

/// 内部请求 id 基址（MrHandle 从 1 起计数，二者不相交）。
const INTERNAL_ID_BASE: u64 = 1_000_000;

/// 订阅流程失败后的重试间隔。
const SUB_RETRY_INTERVAL: Duration = Duration::from_secs(3);

/// 每轮循环最多排空的 WS 帧数（防止对端洪泛时饿死循环其余逻辑）。
/// 打印进行中 notify_status_update 随每行 gcode 推送，速率远超单帧轮询吞吐，
/// 不排空会导致 TCP 缓冲积压、状态上报滞后可达分钟级。
const WS_DRAIN_MAX_FRAMES: usize = 200;

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
            internal_next_id: INTERNAL_ID_BASE,
            pending_list: None,
            pending_sub: None,
            sub_retry_at: None,
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

            // 2. 非阻塞读：排空 socket 内已就绪的全部帧（上限 WS_DRAIN_MAX_FRAMES）
            for _ in 0..WS_DRAIN_MAX_FRAMES {
                match self.client.read_text() {
                    Ok(Some(text)) => self.handle_frame(&text),
                    Ok(None) => break,
                    Err(e) => {
                        log::warn!("moonraker read error: {e}");
                        self.client.close();
                        // 旧连接上的未应答请求永远不会收到响应，条目若不清除
                        // 会随断线累积泄漏（Sender 连同其堆内存滞留至进程退出）。
                        self.pending.clear();
                        self.state.lock().unwrap().moonraker_connected = false;
                        let _ = self.event_tx.send(Event::MrDisconnected);
                        break;
                    }
                }
            }

            // 2.5 内部订阅流程：objects.list → 订阅交集 → 应用初始状态。
            // 失败（如 Klippy 尚未就绪时 objects.list 报 -32601 Method not found）
            // 不再放弃，按 SUB_RETRY_INTERVAL 退避重试，直至订阅成功。
            self.poll_internal();
            if let Some(t) = self.sub_retry_at {
                if Instant::now() >= t {
                    self.sub_retry_at = None;
                    log::info!("moonraker: retrying subscription flow");
                    self.pending_list =
                        Some(self.internal_request("printer.objects.list", json!({})));
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
                        self.pending.clear();
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
                Ok(()) => match (c.no_wait, c.resp) {
                    // no-wait：写入即回执（request_no_wait 等的就是这个结果），
                    // 不注册 pending，RPC 响应按未知 id 处理。
                    (true, Some(tx)) => {
                        let _ = tx.send(Ok(Value::Null));
                    }
                    (false, Some(tx)) => {
                        self.pending.insert(c.id, tx);
                    }
                    (_, None) => {}
                },
                Err(e) => {
                    if let Some(tx) = c.resp {
                        let _ = tx.send(Err(e));
                    } else {
                        log::warn!("moonraker rpc (no-wait) send failed: {e}");
                    }
                }
            },
            Err(e) => {
                if let Some(tx) = c.resp {
                    let _ = tx.send(Err(e.to_string()));
                }
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
                    return;
                }
                // 未关联响应（id=0 心跳 / 重连前遗留请求）：错误不得静默丢弃，
                // 否则订阅失败等问题完全无迹可循。
                if let Some(err) = &resp.error {
                    log::warn!("moonraker rpc error (id={id}): {err}");
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
                // Klippy 就绪（含晚于本连接启动、FIRMWARE_RESTART 重启的场景）：
                // klippy 方法此刻才可用，重建订阅流程以恢复 notify_status_update。
                self.sub_retry_at = None;
                self.pending_sub = None;
                self.pending_list =
                    Some(self.internal_request("printer.objects.list", json!({})));
                let _ = self.event_tx.send(Event::MrConnected);
            }
            "notify_klippy_disconnected" => {
                {
                    let mut s = self.state.lock().unwrap();
                    // 调平中 Klippy 掉线视为调平失败（对齐 Marlin G29_RETURN 失败分支）。
                    if s.leveling_status == 1 {
                        s.leveling_status = 3;
                    }
                    s.moonraker_connected = false;
                }
                let _ = self.event_tx.send(Event::MrDisconnected);
            }
            "notify_gcode_response" => {
                // 调平结束检测（对齐 Marlin G29_RETURN 结束即推 status_level）：
                // - 成功：Klipper 输出 "Mesh Bed Leveling Complete"（extras/bed_mesh.py:786）
                // - 失败：`!!` 前缀且错误与调平相关（探针/归位/网格/Klipper shutdown）。
                //   不能匹配一切 `!!`：调平期间平台/屏幕下发的无关命令报错会把
                //   "调平中"误判为失败，随后调平正常完成又翻回 2，状态来回抖动。
                if let Some(params) = &n.params {
                    if let Some(resp) = params.get(0).and_then(Value::as_str) {
                        let mut s = self.state.lock().unwrap();
                        if resp.contains("Mesh Bed Leveling Complete") {
                            s.leveling_status = 2;
                            s.leveling_point = s.leveling_total;
                            drop(s);
                            let _ = self.event_tx.send(Event::StatusUpdated);
                        } else if resp.starts_with("!!")
                            && s.leveling_status == 1
                            && is_leveling_error(resp)
                        {
                            s.leveling_status = 3;
                            drop(s);
                            let _ = self.event_tx.send(Event::StatusUpdated);
                        }
                    }
                }
            }
            "notify_filelist_changed" => { /* file_list 用 RPC 查询，忽略 */ }
            _ => {}
        }
    }

    fn after_connect(&mut self) {
        // 两步订阅：先 `printer.objects.list` 查实际存在的对象，再对交集发起订阅。
        //
        // 不能直接订阅固定清单：Klipper 对含任一未知对象的订阅请求会整体报错
        // （heater_fan/output_pin/led/runout_sensor 是带后缀的多实例对象，裸名在
        // 多数 printer.cfg 中并不存在），而订阅帧 id=0 的错误响应此前被静默丢弃
        // —— 表现为永远收不到 notify_status_update，app 端温度/风扇恒 0。
        self.state.lock().unwrap().moonraker_connected = true;
        // Klippy 可能晚于 Moonraker 就绪：此时 klippy 方法未注册，objects.list
        // 返回 -32601。失败由 poll_internal 安排退避重试。
        self.sub_retry_at = None;
        self.pending_list = Some(self.internal_request("printer.objects.list", json!({})));
    }

    /// 发送内部请求（id 与 MrHandle 隔离）；发送失败时返回已断开的接收端，
    /// poll_internal 视为失败并放弃本轮订阅（重连后重试）。
    fn internal_request(&mut self, method: &str, params: Value) -> mpsc::Receiver<Result<Value, String>> {
        self.internal_next_id += 1;
        let id = self.internal_next_id;
        let (tx, rx) = mpsc::channel();
        let frame = serde_json::to_string(&RpcRequest { id, jsonrpc: "2.0", method: method.into(), params });
        match frame {
            Ok(t) => {
                if let Err(e) = self.client.send_text(&t) {
                    log::warn!("moonraker {method} send failed: {e}");
                } else {
                    self.pending.insert(id, tx);
                }
            }
            Err(e) => log::warn!("moonraker {method} serialize failed: {e}"),
        }
        rx
    }

    /// 轮询内部请求回包：list → 订阅交集；subscribe → 应用初始状态。
    fn poll_internal(&mut self) {
        if let Some(rx) = self.pending_list.take() {
            match rx.try_recv() {
                Ok(Ok(v)) => self.subscribe_objects(subscription_subset(&v)),
                Ok(Err(e)) => {
                    // 典型场景：Moonraker 已连但 Klippy 未就绪 → klippy 方法未注册，
                    // 返回 -32601 "Method not found"。退避重试直至成功。
                    log::warn!("moonraker objects.list error: {e} (retry in 3s)");
                    self.sub_retry_at = Some(Instant::now() + SUB_RETRY_INTERVAL);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    log::warn!("moonraker objects.list: no reply (retry in 3s)");
                    self.sub_retry_at = Some(Instant::now() + SUB_RETRY_INTERVAL);
                }
                Err(mpsc::TryRecvError::Empty) => self.pending_list = Some(rx),
            }
        }
        if let Some(rx) = self.pending_sub.take() {
            match rx.try_recv() {
                Ok(Ok(v)) => self.apply_initial_status(&v),
                Ok(Err(e)) => {
                    log::warn!("moonraker subscribe error: {e} (restart flow in 3s)");
                    self.sub_retry_at = Some(Instant::now() + SUB_RETRY_INTERVAL);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    log::warn!("moonraker subscribe: no reply (restart flow in 3s)");
                    self.sub_retry_at = Some(Instant::now() + SUB_RETRY_INTERVAL);
                }
                Err(mpsc::TryRecvError::Empty) => self.pending_sub = Some(rx),
            }
        }
    }

    fn subscribe_objects(&mut self, objs: Vec<String>) {
        if objs.is_empty() {
            log::warn!("moonraker: no known object to subscribe");
            return;
        }
        log::info!("moonraker: subscribing {} objects: {objs:?}", objs.len());
        // Klipper 要求 objects 为字典 {对象名: null}；传数组会报
        // "dictionary update sequence element #0 has length N; 2 is required"。
        let objects: serde_json::Map<String, Value> =
            objs.into_iter().map(|o| (o, Value::Null)).collect();
        self.pending_sub = Some(self.internal_request(
            "printer.objects.subscribe",
            json!({ "objects": objects }),
        ));
    }

    /// 订阅响应携带订阅对象的全量初始状态：写入 AppState 并通知模块立即上报。
    fn apply_initial_status(&mut self, result: &Value) {
        let Some(status) = result.get("status").and_then(Value::as_object) else { return };
        log::info!("moonraker: initial status {} objects", status.len());
        {
            let mut s = self.state.lock().unwrap();
            state_bridge::apply(&mut s, status);
        }
        let _ = self.event_tx.send(Event::StatusUpdated);
    }
}

// ---------------------------------------------------------------------------
// state_bridge：notify_status_update → AppState
// ---------------------------------------------------------------------------

/// 取对象名首词：Klipper 多实例对象带后缀（如 "heater_fan xxx"），
/// 订阅与状态归类均按首词进行。
fn first_word(name: &str) -> &str {
    name.split(' ').next().unwrap_or(name)
}

/// `!!` 错误是否与调平相关（网格/探针/归位/掉电 shutdown）。调平执行期间
/// 平台或屏幕下发的无关命令报错不应把"调平中"误判为"调平失败"。
fn is_leveling_error(resp: &str) -> bool {
    const NEEDLES: [&str; 7] =
        ["BED_MESH", "bed_mesh", "probe", "Probe", "home", "shutdown", "Shutdown"];
    NEEDLES.iter().any(|k| resp.contains(k))
}

/// 解析 bed_mesh.probe_count 配置值。configfile.settings 中的值是原始字符串
/// （"5"、"5,5"、"5, 5, 3"），单值 n → n×n，多值取前两个 x×y。上限 255。
fn parse_probe_count(v: &Value) -> Option<u8> {
    let counts: Vec<u32> = match v {
        Value::String(s) => s.split(',').filter_map(|p| p.trim().parse().ok()).collect(),
        Value::Number(n) => vec![n.as_u64()? as u32],
        _ => return None,
    };
    let (x, y) = match counts.as_slice() {
        [n] => (*n, *n),
        [x, y, ..] => (*x, *y),
        _ => return None,
    };
    Some(((x * y) as usize).min(255) as u8)
}

/// 从 `printer.objects.list` 结果中筛出可订阅对象：对象名精确命中或首词命中
/// SUBSCRIBE_OBJECTS（如 "heater_fan main_fan" → 首词 heater_fan）。返回实际
/// 存在的对象名——订阅裸名 "heater_fan" 在 Klipper 中同样会被整体拒绝。
fn subscription_subset(list_result: &Value) -> Vec<String> {
    list_result
        .get("objects")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .filter(|name| SUBSCRIBE_OBJECTS.contains(&first_word(name)))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

pub mod state_bridge {
    use super::*;

    /// 将 notify_status_update 的对象表写入 AppState。
    pub fn apply(s: &mut AppState, map: &Map<String, Value>) {
        for (obj, v) in map {
            let Some(attrs) = v.as_object() else { continue };
            // 按首词归类，兼容 "heater_fan xxx" 等带后缀对象名
            match first_word(obj) {
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
                        // 暂停时 total_duration 仍在走表（暂停语义只冻结 print_duration），
                        // 继续计算会让剩余时间"越暂停越多"；冻结在暂停前的值。
                        if s.print_state != crate::app_state::PrintState::Paused as u8 {
                            s.print_remain_secs =
                                (td - s.print_elapsed_secs as f64).max(0.0) as u64;
                        }
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
                    // level（status_hardware）= 网格有效，对齐 Marlin `leveling_is_valid()`：
                    // 活动网格（BED_MESH_CALIBRATE 完成即生效，无需保存 profile）或已保存
                    // profile；`probed_matrix` 为空（[[]]，如 BED_MESH_CLEAR 后）→ 仅看 profiles。
                    // 注意：Klipper 的 profiles 是字典（如 {"default": {...}}），非数组。
                    let profiles_non_empty = attrs.get("profiles").map(|p| match p {
                        Value::Array(a) => !a.is_empty(),
                        Value::Object(o) => !o.is_empty(),
                        _ => false,
                    });
                    if let Some(pm) = attrs.get("probed_matrix").and_then(Value::as_array) {
                        let cols = pm.first().and_then(Value::as_array).map(|r| r.len()).unwrap_or(0);
                        let mesh_active = !pm.is_empty() && cols > 0;
                        s.level_ok = mesh_active || profiles_non_empty.unwrap_or(s.level_ok);
                        // 探测点阵（如 5×5）→ 调平总点数（status_level.levelingTotalPoints）
                        if mesh_active {
                            s.leveling_total = ((pm.len() * cols).min(255)) as u8;
                        }
                    } else if let Some(pe) = profiles_non_empty {
                        s.level_ok = pe;
                    }
                }
                "configfile" => {
                    // 首次调平（无网格、probed_matrix 为空）时 leveling_total 仍为 0，
                    // 平台调平进度显示 0/0。从 bed_mesh.probe_count（configfile 配置）
                    // 初始化总点数；已有网格推得的实际点数优先，不覆盖。
                    let Some(bm) = attrs.get("settings").and_then(|st| st.get("bed_mesh")) else { continue };
                    let Some(total) = bm.get("probe_count").and_then(parse_probe_count) else { continue };
                    if s.leveling_total == 0 {
                        s.leveling_total = total;
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
    use super::subscription_subset;
    use crate::app_state::AppState;
    use serde_json::json;

    /// 订阅对象须按 objects.list 交集筛选：不存在对象会导致整个订阅被 Klipper 拒绝。
    #[test]
    fn subscription_subset_filters_unknown_objects() {
        let list = json!({
            "objects": ["extruder", "heater_bed", "fan", "heater_fan main_fan",
                        "print_stats", "gcode_move", "virtual_sdcard", "configfile",
                        "output_pin light", "bed_mesh"]
        });
        let subs = subscription_subset(&list);
        for must in ["extruder", "heater_bed", "fan", "heater_fan main_fan", "output_pin light", "bed_mesh"] {
            assert!(subs.iter().any(|s| s == must), "missing {must}");
        }
        assert!(!subs.iter().any(|s| s == "led"), "不存在的裸名应被剔除");
        assert!(!subs.iter().any(|s| s == "runout_sensor"));
    }

    /// 回归：带后缀对象名（"heater_fan xxx"）也应归入风扇状态。
    #[test]
    fn bridge_matches_prefixed_heater_fan() {
        let mut s = AppState::default();
        let map = json!({ "heater_fan board_fan": {"speed": 0.5} });
        state_bridge::apply(&mut s, map.as_object().unwrap());
        assert_eq!(s.board_fan_pct, 50);
    }

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
            "bed_mesh": {"profiles": {"default": {}}, "probed_matrix": [[0.1, 0.2], [0.3, 0.4]]},
            "gcode_move": {"homing_origin": [0.0, 0.0, -0.15]},
        });
        state_bridge::apply(&mut s, map.as_object().unwrap());
        assert!(s.runout);
        assert!(s.level_ok);
        assert_eq!(s.leveling_total, 4, "2×2 探测矩阵 → 总点数 4");
        assert_eq!(s.zoffset_x100, -15);
    }

    /// level 语义对齐 Marlin leveling_is_valid()：校准完成即有活动网格（无需保存
    /// profile）；无网格时 level 仅由已保存 profiles 决定。
    #[test]
    fn bridge_mesh_valid_without_saved_profile() {
        let mut s = AppState::default();
        // 校准完成：有活动网格、无保存 profile → level=1
        let calibrated = json!({
            "bed_mesh": {"profile_name": null, "profiles": {},
                         "probed_matrix": [[0.0, 0.0, 0.0], [0.0, 0.0, 0.0], [0.0, 0.0, 0.0]]}
        });
        state_bridge::apply(&mut s, calibrated.as_object().unwrap());
        assert!(s.level_ok, "活动网格即视为已调平");
        assert_eq!(s.leveling_total, 9);

        // BED_MESH_CLEAR 后：probed_matrix=[[]]、无 profile → level=0
        let cleared = json!({ "bed_mesh": {"profile_name": "", "profiles": {}, "probed_matrix": [[]]} });
        state_bridge::apply(&mut s, cleared.as_object().unwrap());
        assert!(!s.level_ok, "网格清除且无 profile 应视为未调平");
    }

    /// 回归（首次调平总点数为 0）：configfile.settings.bed_mesh.probe_count
    /// 应初始化 leveling_total（首次调平时 probed_matrix 尚为空，平台显示 0/0）。
    #[test]
    fn bridge_inits_leveling_total_from_probe_count() {
        let mut s = AppState::default();
        let cfg = json!({
            "configfile": {"settings": {"bed_mesh": {"probe_count": "5, 5"}}}
        });
        state_bridge::apply(&mut s, cfg.as_object().unwrap());
        assert_eq!(s.leveling_total, 25, "probe_count 5,5 → 总点数 25");

        // 已有网格推得的实际点数优先，不被 configfile 覆盖
        let mesh = json!({ "bed_mesh": {"profiles": {}, "probed_matrix": [[0.1, 0.2], [0.3, 0.4]]} });
        state_bridge::apply(&mut s, mesh.as_object().unwrap());
        assert_eq!(s.leveling_total, 4);

        let cfg2 = json!({
            "configfile": {"settings": {"bed_mesh": {"probe_count": "9,9"}}}
        });
        state_bridge::apply(&mut s, cfg2.as_object().unwrap());
        assert_eq!(s.leveling_total, 4, "非 0 的实际点数不应被配置值覆盖");
    }

    /// probe_count 支持单值（n→n×n）与多值（x,y[,z]）；非法/缺失返回 None。
    #[test]
    fn parse_probe_count_variants() {
        let p = super::parse_probe_count;
        assert_eq!(p(&json!("5")), Some(25));
        assert_eq!(p(&json!("5,5")), Some(25));
        assert_eq!(p(&json!("5, 5")), Some(25));
        assert_eq!(p(&json!("3,3,5")), Some(9));
        assert_eq!(p(&json!(5)), Some(25));
        assert_eq!(p(&json!("abc")), None);
        assert_eq!(p(&json!("")), None);
        assert_eq!(p(&json!(null)), None);
    }

    /// 回归（暂停剩余时间持续增长）：暂停时 total_duration 仍在走表，
    /// printRemainTime 必须冻结在暂停前的值，恢复打印后才继续更新。
    #[test]
    fn bridge_freezes_remain_time_while_paused() {
        let mut s = AppState::default();
        // 打印中：elapsed 180s，total 600s → remain 420s
        let printing = json!({
            "print_stats": {"state": "printing", "filename": "a.gcode",
                            "progress": 0.3, "print_duration": 180.0, "total_duration": 600.0}
        });
        state_bridge::apply(&mut s, printing.as_object().unwrap());
        assert_eq!(s.print_state, crate::app_state::PrintState::Printing as u8);
        assert_eq!(s.print_remain_secs, 420);

        // 暂停：state 变 paused，total_duration 继续增长到 700s → remain 保持 420
        let paused = json!({
            "print_stats": {"state": "paused", "filename": "a.gcode",
                            "progress": 0.3, "print_duration": 180.0, "total_duration": 700.0}
        });
        state_bridge::apply(&mut s, paused.as_object().unwrap());
        assert_eq!(s.print_state, crate::app_state::PrintState::Paused as u8);
        assert_eq!(s.print_remain_secs, 420, "暂停时剩余时间应冻结");

        // 恢复打印：total_duration 800s → remain = 800 - 180 = 620，重新计算
        let resumed = json!({
            "print_stats": {"state": "printing", "filename": "a.gcode",
                            "progress": 0.3, "print_duration": 180.0, "total_duration": 800.0}
        });
        state_bridge::apply(&mut s, resumed.as_object().unwrap());
        assert_eq!(s.print_remain_secs, 620, "恢复打印后剩余时间应重新计算");
    }

    /// 调平失败判定只认调平相关错误：调平期间无关命令的 `!!` 报错
    /// （如 M420 参数错误）不应把"调平中"改判为失败。
    #[test]
    fn leveling_error_matching_is_scoped() {
        let f = super::is_leveling_error;
        assert!(f("!! Must home axis first"));
        assert!(f("!! Probe triggered prior to movement"));
        assert!(f("!! BED_MESH_CALIBRATE: invalid parameter"));
        assert!(f("!! Klipper shutdown: MCU 'probe' shutdown"));
        assert!(!f("!! M420: unknown parameter 'S'"), "无关命令报错不判调平失败");
        assert!(!f("!! Extrude below minimum temp"));
    }

    #[test]
    fn rpc_request_serializes() {
        let req = super::RpcRequest { id: 7, jsonrpc: "2.0", method: "printer.gcode.script".into(), params: json!({"script": "G28"}) };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"id\":7"));
        assert!(s.contains("\"method\":\"printer.gcode.script\""));
    }
}
