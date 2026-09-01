# rv1106-mqtt-client

基于 [myrtio-mqtt](../myrtio-mqtt) 库、面向 **RV1106（Rockchip 平台）** 的 MQTT 客户端项目。

## 项目定位

- **目标平台**：RV1106 Linux（armv7l / armv7-unknown-linux-gnueabihf）
- **基础库**：[myrtio-mqtt](../myrtio-mqtt)（no_std 异步 MQTT 客户端库，支持 MQTT v3.1.1 / v5，模块化 MqttModule）
- **协议依据**：
  - 《MXS MQTT 通信协议（V2.1.2）》（见 [../MXS MQTT 通信协议（V2.1.2）.md](../MXS%20MQTT%20通信协议（V2.1.2）.md)）
  - 《MQTT 客户端数据流与命令清单》（见 [../MQTT 客户端数据流与命令清单.md](../MQTT%20客户端数据流与命令清单.md)）
- **开发环境参考**：rust-libp2p（交叉编译工具链、构建脚本、目录规范，见 [../rust-libp2p](../rust-libp2p)）
- **设备交互**：本机运行 Klipper + Moonraker（`ws://127.0.0.1:7125`），**所有硬件读取/配置/控制均经 Moonraker 完成**（参考 [../guppyscreen](../guppyscreen)），客户端零硬件耦合
- **平台定位**：本项目为 **Klipper 平台**；[../m2-software](../m2-software)（Marlin）仅作**行为参考**，其 GCODE 命令与硬件行为需转换到 Klipper 生态

## 文档目录

| 文档 | 说明 | 状态 |
| --- | --- | --- |
| [docs/需求规格说明书.md](docs/需求规格说明书.md) | SPC 流程阶段 1：需求规格 | 完成 |
| [docs/概要设计说明书.md](docs/概要设计说明书.md) | SPC 流程阶段 2：概要设计 | 完成 |
| [docs/详细设计说明书.md](docs/详细设计说明书.md) | SPC 流程阶段 3：详细设计 | 完成 |
| [docs/详细设计说明书.md](docs/详细设计说明书.md) §13 | SPC 流程阶段 4：编码实现（骨架 + 模块 + 单测） | 完成 |
| docs/测试计划.md | SPC 流程阶段 5：测试（待产出） | 计划 |

## SPC 流程

本项目按 SPC（软件过程控制）流程推进：

1. 需求规格（本文档阶段）
2. 概要设计
3. 详细设计
4. 编码实现
5. 单元测试
6. 集成测试
7. 系统测试 / 验收

## 目录结构

```
rv1106-mqtt-client/
├── Cargo.toml              # workspace 清单（members: crates/mqtt-client）
├── .cargo/config.toml      # 交叉编译 linker 配置（模板）
├── crates/
│   └── mqtt-client/
│       ├── Cargo.toml
│       └── src/
│           ├── main.rs             # 线程装配 + MQTT 会话（重连循环）
│           ├── lib.rs
│           ├── config.rs           # TOML 配置
│           ├── transport.rs        # StdTcpTransport（MqttTransport 实现）
│           ├── moonraker.rs        # WS 客户端 + JSON-RPC + state_bridge
│           ├── protocol.rs         # 12 类包编解码
│           ├── state.rs            # 连接状态机 / 10 槽 FIFO / 事件
│           ├── app_state.rs        # 共享状态
│           ├── gcode_translator.rs # Marlin→Klipper 行为转换
│           ├── modules.rs          # 8 业务模块 + AppModule（impl MqttModule）
│           └── downlink.rs         # 下行分发（gcode/下载/文件列表）
├── config/mqtt-client.toml # 示例配置
├── docs/                   # 项目文档
└── target/
```

> 模块划分依据 [docs/概要设计说明书.md](docs/概要设计说明书.md) §3.2；`myrtio-mqtt` 为工作区同级 path 依赖（`../myrtio-mqtt`）。

## 构建与测试

```bash
cd rv1106-mqtt-client
cargo check                 # 编译检查（零警告）
cargo test                  # 单元测试（31 例，host 原生）

# 交叉编译到 RV1106
# 1) 导出 Luckfox 工具链到 PATH（arm-rockchip830-linux-uclibcgnueabihf）
export PATH=/home/song/samba/work/rv1106/luckfox-pico/tools/linux/toolchain/arm-rockchip830-linux-uclibcgnueabihf/bin:$PATH
# 2) 交叉编译（已配好 .cargo/config.toml，linker + sysroot + getauxval 桩）
#    推荐用构建脚本（自动探测工具链、设置 linker、校验产物）：
cargo build --release --target armv7-unknown-linux-gnueabihf
# 或: ./scripts/build.sh release            # release 交叉编译
#     ./scripts/build.sh release deploy     # 编译 + scp 到板子 (RV1106_HOST 可覆盖 IP)
# 产物：target/armv7-unknown-linux-gnueabihf/release/mqtt-client（ARM 32-bit, uclibc）
```

> 工具链为 Luckfox Pico（RV1106）官方 `arm-rockchip830-linux-uclibcgnueabihf`（uclibc 1.0.31）。
> Rust 无对应 uclibceabihf target，采用 `armv7-unknown-linux-gnueabihf` + uclibc gcc 作 linker/symroot。
> 因 uclibc 1.0.31 缺失 `getauxval` 符号，已预编译 `cross/libgetauxval.a` 桩（见 `.cargo/config.toml`）。

运行：`mqtt-client [配置文件路径]`，默认 `config/mqtt-client.toml`。

## 对 myrtio-mqtt 的补丁

| 编号 | 补丁 | 文件 |
| --- | --- | --- |
| P1 | 透传 embassy-time `std` feature（std 时钟驱动） | `myrtio-mqtt/Cargo.toml` |
| **P1b** | `MqttOptions` 支持 `clean_session`（R1 要求 Clean Session=0；原 `connect` 中写死 `true`） | `myrtio-mqtt/src/client.rs` |
| **P2** | `embassy-net` 改为 optional（默认不启用）；`TcpTransport` 及相关导出 gate 在 `embassy-net` feature 下。本项目走 std `StdTcpTransport`，不拉入 embassy-net / embassy-executor-timer-queue | `myrtio-mqtt/Cargo.toml`、`src/transport.rs`、`src/lib.rs` |
| **P3** | `embassy-time` 启用 `generic-queue-8`，改用通用定时器队列，避免 `embassy-time-queue-utils` 集成队列对 `__embassy_time_queue_item_from_waker`（需 embassy-executor 提供）的未定义引用 | `myrtio-mqtt/Cargo.toml` |
| **P4** | 新增 `cross/libgetauxval.a` 桩（uclibc 1.0.31 缺失 `getauxval`），经 `.cargo/config.toml` 链接 | `cross/getauxval.c`、`cross/libgetauxval.a`、`.cargo/config.toml` |
| **P5** | 重连延时由 `embassy_time::Timer::after` 改为 `std::thread::sleep`（本项目用 `futures::block_on` 而非 embassy executor，无定时器队列驱动） | `crates/mqtt-client/src/main.rs` |

> 详见 [docs/详细设计说明书.md](docs/详细设计说明书.md) §2.2 与 §14（实现偏差说明）。
