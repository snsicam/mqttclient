//! 下行分发（LLD-003 §7.4）：AppModule on_message → Dispatcher（Moonraker 执行）→ 事件回传。

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
    /// 升级查询回复。
    UpgradeQuery,
    /// 服务器解绑。
    ServerUnbind,
}

pub struct Dispatcher {
    cfg: AppConfig,
    cmd_rx: mpsc::Receiver<DownlinkCmd>,
    mr: MrHandle,
    event_tx: mpsc::Sender<Event>,
}

impl Dispatcher {
    pub fn spawn(cfg: AppConfig, cmd_rx: mpsc::Receiver<DownlinkCmd>, mr: MrHandle, event_tx: mpsc::Sender<Event>) {
        std::thread::Builder::new()
            .name("dispatcher".into())
            .spawn(move || {
                let mut d = Self { cfg, cmd_rx, mr, event_tx };
                d.run();
            })
            .expect("spawn dispatcher");
    }

    fn run(&mut self) {
        while let Ok(cmd) = self.cmd_rx.recv_timeout(Duration::from_millis(500)) {
            self.handle(cmd);
        }
    }

    fn handle(&mut self, cmd: DownlinkCmd) {
        match cmd {
            DownlinkCmd::Gcode { cmd_type, raw } => {
                let result = self.exec_gcode(&raw);
                let _ = self.event_tx.send(Event::GcodeResult { cmd_type, result });
            }
            DownlinkCmd::DownloadBegin { file_type, file_name, url, server_ip } => {
                let err_code = self.download(file_type, &file_name, url.as_deref(), server_ip.as_deref());
                let _ = self.event_tx.send(Event::DownloadFinished { file_type, file_name, err_code });
            }
            DownlinkCmd::DeleteFile { file_name } => {
                let result = self.delete_file(&file_name);
                let _ = self.event_tx.send(Event::GcodeResult { cmd_type: "M30".into(), result });
            }
            DownlinkCmd::ListFiles => {
                let files = self.list_files();
                let _ = self.event_tx.send(Event::FileListResult { files });
            }
            DownlinkCmd::UpgradeQuery => {
                // 无升级服务器：保持协议兼容回复
                let _ = self.event_tx.send(Event::GcodeResult { cmd_type: "upgrade_query".into(), result: "NO_UPGRADE".into() });
            }
            DownlinkCmd::ServerUnbind => {
                log::warn!("server_unbind received: local binding reset");
                let _ = self.event_tx.send(Event::GcodeResult { cmd_type: "server_unbind".into(), result: "OK".into() });
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

    /// HTTP 下载到 U 盘。err_code：0 成功 / 1 忙或网络错误 / 2 无 U盘或写失败 / 3 传输超时或超限。
    fn download(&mut self, file_type: u8, file_name: &str, url: Option<&str>, _server_ip: Option<&str>) -> u8 {
        let Some(url) = url.filter(|u| !u.is_empty()) else {
            return 1;
        };
        let safe = file_name.rsplit(['/', '\\']).next().unwrap_or(file_name).to_string();
        if safe.is_empty() {
            return 1;
        }
        let dir = self.cfg.download.dir.clone();
        if std::fs::create_dir_all(&dir).is_err() {
            log::warn!("mkdir failed: {dir}");
            return 2;
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
                return 1;
            }
        };
        let body = match resp.body_mut().read_to_vec() {
            Ok(b) => b,
            Err(e) => {
                log::warn!("download read: {e}");
                return 3;
            }
        };
        if body.len() as u64 > self.cfg.download.max_file_bytes {
            log::warn!("download too large: {}", body.len());
            return 3;
        }
        if let Err(e) = std::fs::write(&dest, &body) {
            log::warn!("write {dest:?}: {e}");
            return 2;
        }
        // GCODE：上传到 Klipper 以支持打印（multipart 上传，简单实现）
        if file_type == 0 {
            if let Err(e) = self.mr.request("server.files.upload", json!({ "path": safe }), Duration::from_secs(10)) {
                log::warn!("klipper upload skipped: {e}");
            }
        }
        log::info!("download ok: {safe} ({} bytes)", body.len());
        0
    }
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
