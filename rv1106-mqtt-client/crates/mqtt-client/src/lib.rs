//! RV1106 MQTT client（MXS 协议 V2.1.2），桥接 Moonraker/Klipper。
//!
//! 结构（对应《详细设计说明书》LLD-003 §3）：
//! - `transport`：StdTcpTransport（myrtio-mqtt 的 MqttTransport 实现，非阻塞 + yield 轮询）
//! - `moonraker`：Moonraker WS 客户端（JSON-RPC/通知分发/状态桥）
//! - `protocol`：MXS 上行/下行 12 类包编解码
//! - `state`：连接状态机 / 10 槽上行 FIFO / 事件
//! - `downlink`：下行分发器 + Marlin→Klipper 行为转换
//! - `modules`：8 个业务模块聚合（AppModule，实现 myrtio-mqtt 的 MqttModule）
//! - `app_state`：共享状态（温度/打印/调平/硬件）
//! - `config`：TOML 配置
//! - `gcode_translator`：M 码转换表

pub mod app_state;
pub mod config;
pub mod downlink;
pub mod gcode_translator;
pub mod modules;
pub mod moonraker;
pub mod protocol;
pub mod state;
pub mod transport;

pub use app_state::AppState;
pub use config::AppConfig;
