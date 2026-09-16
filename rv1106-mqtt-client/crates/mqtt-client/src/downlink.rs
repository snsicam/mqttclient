//! 下行分发（LLD-003 §7.4）：AppModule on_message → Dispatcher（Moonraker 执行）→ 事件回传。

use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::AppConfig;
use crate::gcode_translator::{GcodeKind, translate};
use crate::moonraker::MrHandle;
use crate::state::Event;

/// AppModule → Dispatcher 的下行命令。
#[derive(Debug, Clone)]
pub enum DownlinkCmd {
    /// gcode 执行（cmd_type 原样回传）。
    Gcode { cmd_type: String, raw: String },
    /// 文件下载（fileType: 0 GCODE / 1 主控固件 / 2 ESP 固件）。
    DownloadBegin { file_type: u8, file_name: String, url: Option<String>, server_ip: Option<String> },
    /// 删除文件（M30 路径）。
    DeleteFile { file_name: String },
    /// 文件列表查询（M20）。
    ListFiles,
    /// 服务器解绑。
    ServerUnbind,
}

/// UI（klipper_screen）经 UDS 下发的控制请求。每个请求携带 reply 通道回传结果
/// （由 UDS 服务线程创建并等待）。UI 侧触发的云侧操作（下载/解绑）经此进入 Dispatcher。
#[derive(Debug)]
pub enum UiCmd {
    /// 文件下载（fileType: 0 GCODE；Klipper 仅支持 gcode，见 D16）。
    Download { file_type: u8, file_name: String, url: Option<String>, reply_tx: mpsc::Sender<UiReply> },
    /// 设备解绑（触发 device_unbind 上行）。
    Unbind { reply_tx: mpsc::Sender<UiReply> },
    /// 固件升级查询：由 AppModule 发布 `upgrade_query` 上行，回复经 `reply_tx` 回 UDS/UI。
    UpgradeQuery { reply_tx: mpsc::Sender<UiReply> },
}

/// Dispatcher 经 reply 通道回传给 UDS 服务的执行结果。
#[derive(Debug)]
pub enum UiReply {
    Ok(serde_json::Value),
    Err(String),
}

pub struct Dispatcher {
    cfg: AppConfig,
    cmd_rx: mpsc::Receiver<DownlinkCmd>,
    ui_rx: mpsc::Receiver<UiCmd>,
    mr: MrHandle,
    event_tx: mpsc::Sender<Event>,
    /// Moonraker `gcodes` 根路径（首次查询 `server.files.roots` 成功后缓存；AV-4 校验用）。
    gcode_root: Option<String>,
}

impl Dispatcher {
    pub fn spawn(
        cfg: AppConfig,
        cmd_rx: mpsc::Receiver<DownlinkCmd>,
        ui_rx: mpsc::Receiver<UiCmd>,
        mr: MrHandle,
        event_tx: mpsc::Sender<Event>,
    ) {
        std::thread::Builder::new()
            .name("dispatcher".into())
            .spawn(move || {
                let mut d = Self { cfg, cmd_rx, ui_rx, mr, event_tx, gcode_root: None };
                d.run();
            })
            .expect("spawn dispatcher");
    }

    fn run(&mut self) {
        // 收命令：200ms 超时仅作为「周期性唤醒」，不可作为退出条件——否则空闲后
        // 线程退出，后续 gcode/下载命令会因接收端已 drop 被静默丢弃（cmd_tx.send 返回 Err）。
        // 仅当两个发送端（AppModule/MQTT 下行 与 UDS 控制通道）都彻底断开（Disconnected）时才退出。
        let mut cmd_dead = false;
        let mut ui_dead = false;
        loop {
            if !cmd_dead {
                match self.cmd_rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(cmd) => self.handle(cmd),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => cmd_dead = true,
                }
            }
            if !ui_dead {
                loop {
                    match self.ui_rx.try_recv() {
                        Ok(cmd) => self.handle_ui(cmd),
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            ui_dead = true;
                            break;
                        }
                    }
                }
            }
            if cmd_dead && ui_dead {
                break;
            }
        }
    }

    fn handle(&mut self, cmd: DownlinkCmd) {
        match cmd {
            DownlinkCmd::Gcode { cmd_type, raw } => {
                let result = self.exec_gcode(&raw);
                let _ = self.event_tx.send(Event::GcodeResult { cmd_type, result });
            }
            DownlinkCmd::DownloadBegin { file_type, file_name, url, server_ip } => {
                let (err_code, _dest) = self.download(file_type, &file_name, url.as_deref(), server_ip.as_deref());
                let _ = self.event_tx.send(Event::DownloadFinished { file_type, file_name, err_code, from_ui: false });
            }
            DownlinkCmd::DeleteFile { file_name } => {
                let result = self.delete_file(&file_name);
                let _ = self.event_tx.send(Event::GcodeResult { cmd_type: "M30".into(), result });
            }
            DownlinkCmd::ListFiles => {
                let files = self.list_files();
                let _ = self.event_tx.send(Event::FileListResult { files });
            }
            DownlinkCmd::ServerUnbind => {
                log::warn!("server_unbind received: local binding reset");
                let _ = self.event_tx.send(Event::GcodeResult { cmd_type: "server_unbind".into(), result: "OK".into() });
            }
        }
    }

    /// 处理 UI（klipper_screen）经 UDS 下发的控制请求，结果经 reply_tx 回传。
    fn handle_ui(&mut self, cmd: UiCmd) {
        match cmd {
            UiCmd::Download { file_type, file_name, url, reply_tx } => {
                log::info!("[uds] download: 开始 file_type={file_type} file_name={file_name} url={:?}", url);
                // 协议 §5.5/§5.6：先上行 download_begin（受理），完成后上行 download_end，形成配对。
                let _ = self.event_tx.send(Event::DownloadStarted { file_type, file_name: file_name.clone() });
                let (err_code, dest) = self.download(file_type, &file_name, url.as_deref(), None);
                let _ = self.event_tx.send(Event::DownloadFinished { file_type, file_name, err_code, from_ui: true });
                let reply = if err_code == 0 {
                    let result = match dest {
                        Some(p) => json!({ "err_code": 0, "dest": p }),
                        None => json!({ "err_code": 0 }),
                    };
                    UiReply::Ok(result)
                } else {
                    UiReply::Err(format!("download failed: code {err_code}"))
                };
                log::info!("[uds] download: 完成 err_code={err_code}");
                let _ = reply_tx.send(reply);
            }
            UiCmd::Unbind { reply_tx } => {
                log::info!("[uds] unbind: 触发 device_unbind 上行");
                // 触发 device_unbind 上行（AppModule 在 drain_events 中组包并强制发布）。
                let _ = self.event_tx.send(Event::UiUnbind);
                let _ = reply_tx.send(UiReply::Ok(json!({ "ok": true })));
            }
            UiCmd::UpgradeQuery { reply_tx } => {
                // 转交 AppModule：由其发布 upgrade_query 上行并暂存 reply_tx，待服务器回包。
                let _ = self.event_tx.send(Event::UpgradeQueryRequested { reply_tx: Some(reply_tx) });
            }
        }
    }

    fn exec_gcode(&mut self, raw: &str) -> String {
        match translate(raw) {
            GcodeKind::Empty => "OK".into(),
            GcodeKind::Native(text) => self.script(&text),
            GcodeKind::Leveling => self.script("BED_MESH_CALIBRATE"),
            GcodeKind::PrintStart(file) => match self.mr.request("printer.print.start", json!({ "filename": file }), Duration::from_secs(10)) {
                Ok(_) => "File selected OK".into(),
                Err(e) => format!("File open failed: {e}"),
            },
            GcodeKind::Resume => self.print_ctrl("printer.print.resume"),
            GcodeKind::Pause => self.print_ctrl("printer.print.pause"),
            GcodeKind::Cancel => self.print_ctrl("printer.print.cancel"),
            GcodeKind::ListFiles => {
                let files = self.list_files();
                let _ = self.event_tx.send(Event::FileListResult { files });
                "OK".into()
            }
            GcodeKind::DeleteFile(file) => self.delete_file(&file),
            GcodeKind::Unsupported(s) => format!("ERR: unsupported: {s}"),
        }
    }

    fn script(&mut self, script: &str) -> String {
        match self.mr.request("printer.gcode.script", json!({ "script": script }), Duration::from_secs(30)) {
            Ok(_) => "OK".into(),
            Err(e) => format!("ERR: {e}"),
        }
    }

    /// 查询并缓存 Moonraker `gcodes` 根路径（`server.files.roots`）；查询失败返回 `None`
    /// 且**不缓存**，供下次重试（Moonraker 可能尚未连接）。
    fn gcode_root(&mut self) -> Option<String> {
        if let Some(r) = &self.gcode_root {
            return Some(r.clone());
        }
        match self.mr.request("server.files.roots", json!({}), Duration::from_secs(5)) {
            Ok(v) => {
                let found = v
                    .as_array()
                    .and_then(|arr| {
                        arr.iter().find(|it| it.get("name").and_then(Value::as_str) == Some("gcodes"))
                    })
                    .and_then(|it| it.get("path").and_then(Value::as_str))
                    .map(|s| s.to_string());
                if let Some(p) = &found {
                    log::info!("Moonraker gcodes root: {p}");
                    self.gcode_root = Some(p.clone());
                } else {
                    log::warn!("server.files.roots has no 'gcodes' root");
                }
                found
            }
            Err(e) => {
                log::warn!("query server.files.roots failed: {e}");
                None
            }
        }
    }

    fn print_ctrl(&mut self, method: &str) -> String {
        match self.mr.request(method, json!({}), Duration::from_secs(10)) {
            Ok(_) => "OK".into(),
            Err(e) => format!("ERR: {e}"),
        }
    }

    fn delete_file(&mut self, file: &str) -> String {
        match self.mr.request("server.files.delete_file", json!({ "path": file }), Duration::from_secs(5)) {
            Ok(_) => "File deleted".into(),
            Err(e) => format!("Deletion failed: {e}"),
        }
    }

    fn list_files(&mut self) -> Vec<(String, u64)> {
        match self.mr.request("server.files.list", json!({ "root": "gcodes" }), Duration::from_secs(5)) {
            Ok(v) => {
                let mut out = Vec::new();
                if let Some(arr) = v.as_array() {
                    for item in arr {
                        let name = item.get("path").and_then(Value::as_str).unwrap_or("").to_string();
                        let size = item.get("size").and_then(Value::as_u64).unwrap_or(0);
                        if !name.is_empty() {
                            out.push((name, size));
                        }
                    }
                }
                out
            }
            Err(_) => Vec::new(),
        }
    }

    /// HTTP 下载到 Moonraker `gcodes` 受监控目录（共享目录方案 D16）。返回 `(err_code, dest)`：
    /// err_code 0 成功 / 1 忙或网络错误 / 2 无目录或写失败 / 3 传输超时或超限 / 4 不支持的 file_type
    /// （Klipper 侧仅 `0`=gcode 受支持，`1/2` 固件不处理）；成功时 `dest` 为目标绝对路径。
    ///
    /// 落盘即被 Moonraker 自动发现，UI 经 `server.files.list` 可见即可打印（**不再调用
    /// `server.files.upload`**——该接口为 HTTP multipart 端点，WS JSON-RPC 无法传字节，见 SPC §9.3 #2）。
    ///
    /// 流式写入磁盘：按 `download.chunk_size` 分块读取并边读边落盘，同时在读取过程中累加校验
    /// 总量，超过 `max_file_bytes` 立即中止并删除残留文件。避免「先整文件读进内存再校验 512MB」
    /// 导致大文件撑爆内存；也让配置项 `chunk_size` 真正生效。
    fn download(&mut self, file_type: u8, file_name: &str, url: Option<&str>, _server_ip: Option<&str>) -> (u8, Option<String>) {
        let Some(url) = url.filter(|u| !u.is_empty()) else {
            return (1, None);
        };
        let safe = file_name.rsplit(['/', '\\']).next().unwrap_or(file_name).to_string();
        if safe.is_empty() {
            return (1, None);
        }
        // 落地目录：gcode(file_type=0) 走 Moonraker `gcodes` 受监控目录；固件(1=主控/2=ESP)
        // 走独立的 firmware_dir，不进打印列表（D16）。
        let (dir, is_gcode) = match file_type {
            0 => (expand_tilde(&self.cfg.download.dir), true),
            1 | 2 => (expand_tilde(&self.cfg.download.firmware_dir), false),
            _ => {
                log::warn!("download: unsupported file_type {file_type}");
                return (4, None);
            }
        };
        // AV-4/OQ-U1：仅 gcode 需校验落在 Moonraker `gcodes` 根内；不一致则文件不会被
        // Moonraker 发现（不进打印列表）——返回 err_code=2 而非静默"成功"。查询不到根（Moonraker
        // 未连接）时放行并告警，下次下载重试。固件不走此校验。
        if is_gcode {
            if let Some(root) = self.gcode_root() {
                if !std::path::Path::new(&dir).starts_with(&root) {
                    log::error!("download.dir {dir} 不在 Moonraker gcodes 根 {root} 内 -> 文件不会被 Moonraker 发现（AV-4/OQ-U1）");
                    return (2, None);
                }
            }
        }
        if std::fs::create_dir_all(&dir).is_err() {
            log::warn!("mkdir failed: {dir}");
            return (2, None);
        }
        let dest = std::path::Path::new(&dir).join(&safe);

        // ureq 3.x：超时通过 Config + Agent 设置（无 RequestBuilder::timeout）
        let config = ureq::config::Config::builder()
            .timeout_global(Some(Duration::from_secs(60)))
            .build();
        let agent: ureq::Agent = config.into();
        let mut resp = match agent.get(url).call() {
            Ok(r) => r,
            Err(e) => {
                log::warn!("download {url}: {e}");
                return (1, None);
            }
        };

        let max_bytes = self.cfg.download.max_file_bytes;
        let mut reader = resp.body_mut().as_reader();
        let mut buf = vec![0u8; self.cfg.download.chunk_size.max(1)];
        let mut file = match std::fs::File::create(&dest) {
            Ok(f) => f,
            Err(e) => {
                log::warn!("create {dest:?}: {e}");
                return (2, None);
            }
        };
        let mut total: u64 = 0;
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    log::warn!("download read {url}: {e}");
                    let _ = std::fs::remove_file(&dest);
                    return (3, None);
                }
            };
            total += n as u64;
            if total > max_bytes {
                log::warn!("download too large: {total} > max {max_bytes}");
                let _ = std::fs::remove_file(&dest);
                return (3, None);
            }
            if let Err(e) = file.write_all(&buf[..n]) {
                log::warn!("write {dest:?}: {e}");
                let _ = std::fs::remove_file(&dest);
                return (2, None);
            }
        }
        if let Err(e) = file.flush() {
            log::warn!("flush {dest:?}: {e}");
            let _ = std::fs::remove_file(&dest);
            return (2, None);
        }

        // 共享目录方案（D16）：落盘即被 Moonraker 自动发现，无需调用 server.files.upload。
        log::info!("download ok: {safe} -> {dest:?} ({total} bytes)");
        (0, Some(dest.to_string_lossy().into_owned()))
    }
}

/// 将 `~/...` 展开为 `$HOME/...`（配置项 `[download] dir` 可能含 `~`）。
fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dl_cmd_type_matches() {
        let c = DownlinkCmd::Gcode { cmd_type: "G28".into(), raw: "G28".into() };
        match c {
            DownlinkCmd::Gcode { cmd_type, raw } => {
                assert_eq!(cmd_type, "G28");
                assert_eq!(raw, "G28");
            }
            _ => panic!("wrong variant"),
        }
    }
}
