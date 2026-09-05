# M2 APP ↔ 设备 蓝牙（BluFi）协议与指令清单

> 版本 V5（2026-09-05）：以 V2 结构为基础重整，保留完整章节与代码依据；核心是把 **「谁发起 → 谁回复 → 怎么回 → 不回会怎样」** 讲清楚。按此前约定，仍不含 MCU 与 ESP32 之间的内部 AT 指令与状态机。
> APP 侧依据：`WifiListConfig.java`（`com.geeetech.app.device`，基于 `blufi.espressif.BlufiClient`）
> 协议依据：Espressif BluFi 规范；`MXS MQTT 通信协议（V2.1.2）`第 二 章
> 设备端（ESP32 + MCU）在本协议中视为一个整体「设备」。

---

## 0. 结论速览

| 项目 | 结论 |
|---|---|
| 蓝牙用途 | 仅用于 **Wi-Fi 配网**，不承担业务数据；配网完成后蓝牙关闭，业务转 MQTT |
| 协议 | 乐鑫 **BluFi**（BLE/GATT），设备为 GATT Server，APP 为 GATT Client |
| APP 侧实现 | 使用官方 `blufi.espressif.BlufiClient`，非自研 BLE 协议 |
| 热点列表 | APP 发控制帧 `0x9` → 设备自行扫描 → 回数据帧 `0x11`（结构化 ssid + rssi） |
| 配网数据 | APP 发数据帧 `0x13` Custom Data（明文 `SSID:..,PWD:..`）→ 设备解析并连 AP |
| 结果通知 | 设备回数据帧 `0x13` 回执 + 数据帧 `0xF` 连接状态报告 |
| 链路关闭 | APP 发控制帧 `0x8` 断开 BLE GATT 链路 |

---

## 1. 系统角色与数据通路

```
APP (GATT Client)                        设备 (GATT Server：ESP32 + MCU)
      │                                            │
      │ ======== BLE / GATT，Service 0xFFFF ======= │
      │  写   0xFF01：控制帧 0x9 / 数据帧 0x13 / 控制帧 0x8
      │  通知 0xFF02：数据帧 0x11 / 0x13 / 0xF / 0x10 / 0x12
      │                                            │
      │                                            │ （设备内部：ESP32 与 MCU 之间另有一套串口通信，不属于本协议范围）
      │                                            │
      │  配网成功后：MQTT over Wi-Fi（与蓝牙无关）  │
```

- APP 与设备之间是**标准 BluFi 帧**，加密、校验、分片由 `BlufiClient` 与设备侧协议栈完成，双方均已封装。
- 本协议只描述**蓝牙链路上 APP 与设备交互**的部分。

---

## 2. 蓝牙通道定位

### 2.1 GATT 服务与特征（BluFi 标准）

| 项 | 值 | 方向 | 属性 |
|---|---|---|---|
| Service UUID | `0xFFFF`（16 bit） | — | BluFi 服务 |
| 特征 | `0xFF01` | APP → 设备 | 可写 |
| 特征 | `0xFF02` | 设备 → APP | 可读 + 可通知 |

### 2.2 帧格式

```
| Type (1B) | Frame Control (1B) | Sequence (1B) | Data Length (1B) | Data (N B) | CheckSum (2B) |
```

- **Type**：低 2 位为包类型（`0x0` 控制帧 / `0x1` 数据帧），高 6 位为 Subtype（见 §4）。
- **Frame Control** 位定义：

| 位 | 含义 |
|---|---|
| `0x01` | 是否加密（控制帧恒不加密） |
| `0x02` | 是否含校验（CheckSum 对 `Sequence + Data Length + 明文数据` 计算） |
| `0x04` | 方向：`0` = APP→设备，`1` = 设备→APP |
| `0x08` | **是否要求对方回 ACK** |
| `0x10` | 是否有后续分片（Data 前 2 字节为总长度，最大 64 KB） |

- **Sequence**：每发一帧自增 1，防重放，重连后清零。
- **CheckSum**：2 字节，校验 `Sequence + Data Length + 明文数据`。

### 2.3 广播名与设备 ID

| 项 | 说明 |
|---|---|
| 广播名 | 协议文档记为 `MXS-<deviceId>`；APP 实测为 `M2-<deviceId>`。**本 rv1106 实现**广播名 = `{model}-{deviceId}`（跟随 `[mqtt].model`，板子实测 `M1S-Ge33700a6620dfddc`），见详细设计 §4.3.2 |
| 设备 ID | MXS 协议文档描述「`H`+MAC 末位+2」。**本 rv1106 实现**沿用 MQTT 客户端既有逻辑 `G`+`/proc/cpuinfo` Serial（与 `config.rs` 一致），不采用 `H` 前缀 |
| 用途 | 广播名供 APP 识别设备；设备 ID 同时作为后续 MQTT ClientID 与主题后缀 |

---

## 3. APP 侧调用链与运行规则

### 3.1 连接与扫描调用链（`WifiListConfig.java`）

```
connect()                                    :119-133   启动 10s 列表超时
  └─ BlufiClient.connect()
       └─ onGattPrepared(STATUS_SUCCESS)     :269-288   请求 MTU
            └─ onMtuChanged(成功/失败)       :239-257
                 └─ requestDeviceWifiScan()  :155-169   ── 发起扫描 A4
                      └─ onDeviceScanResult():343-376   ◄── 收到列表 B1

（用户选热点、输密码）
postCustumData("SSID:..,PWD:..")             :191-197   ── 下发配网 A5
  ├─ onPostCustomDataResult(SUCCESS)         :383-403   2s 后未收到任何 B2 → 重发一次 A6
  ├─ onReceiveCustomData(...)                :406-421   ◄── 收到 B2（回执 / 失败）
  └─ onDeviceStatusResponse(...)             :325-341   ◄── 收到 B3 状态报告
close() → requestCloseConnection()           :439-468   ── 断链 A7
```

### 3.2 超时、重传与并发规则

| 规则 | 取值 / 行为 | 行号 |
|---|---|---|
| GATT 写超时 | 10 s；超时提示 Timeout 并关闭连接 | :35, :131, :425-430 |
| 热点列表超时 | 10 s；收到任意扫描结果即取消计时（空列表视为「无可用热点」） | :48, :81-113 |
| 配置数据重传 | 下发成功 2 s 后若未收到任何设备自定义数据 → **重发 1 次** | :387-395 |
| 扫描防重入 | `mScanning` 标志，扫描中忽略重复刷新 | :160-163 |
| 刷新不重连 | 刷新复用已建立的连接，避免旧 GATT 断连回调与新连接竞争 | :140-147 |
| 陈旧回调过滤 | 客户端 / GATT 实例三重判等，旧连接的异步回调一律丢弃 | :207-209, :240, :270, :344 |
| 状态响应门控 | **未提交配置前**收到的状态报告直接忽略 | :327-330 |
| 分包上限 | MTU 协商失败时 `setPostPackageLengthLimit(20)` | :250, :285 |
| 失败判定 | 自定义数据含 `"failed"` 子串，或状态报告非 `STATUS_SUCCESS` | :411, :336 |

---

## 4. 交互协议：发起与回复（核心）

### 4.1 请求-回复配对总表

先看全局：谁发起、谁回复、不回会怎样。

| 配对 | 发起方 | 请求 | 回复方 | 回复内容 | 强制回复 | 不回复的后果 |
|---|---|---|---|---|---|---|
| **P1** | APP | **A4** 控制帧 `0x9` 请求扫描热点 | 设备 | **B1** 数据帧 `0x11` 热点列表；扫描失败回 **B5** 错误码 `0x0b` | **是** | APP 等 10 s → 列表获取失败，提示重试 |
| **P2** | APP | **A5** 数据帧 `0x13` 下发配网信息 | 设备 | ① **B2** 回执 `Received SSID and password`（3 帧）② 最终 **B3** 数据帧 `0xF` 状态报告 | **是** | APP 2 s 后重发一次（A6）；**重发后无二次超时保护**，设备若始终不回，APP 会一直停在配网等待界面 |
| **P3** | APP | **A6** 数据帧 `0x13` 重发配置 | 设备 | 同 P2 | 是 | 同上（设备需幂等处理） |
| P4 | APP | **A1** GATT 连接 + 服务发现 | 设备 | 连接状态 → `onGattPrepared(status)`；失败时 APP 主动断开 | 是（BLE 层） | APP 无法进入配网 |
| P5 | APP | **A2** MTU 协商 | 设备 | `onMtuChanged(mtu, status)` | 是（BLE 层） | 部分设备不回调，APP 有兜底：按 20 字节分包继续扫描 |
| P6 | 双向 | **A8** 控制帧 `0x1` + 数据帧 `0x0` 安全协商 | 双向 | 互发数据帧 `0x0` 协商数据；APP 收 `onNegotiateSecurityResult` | 是 | 连接不可用；由库自动完成 |
| P7 | APP | **A7** 控制帧 `0x8` 断开链路 | 设备 | **无应用层回复**，设备直接断开 GATT | 否 | — |
| P8 | 设备 | **B4** 数据帧 `0x10` 版本信息 | APP | 无（APP 仅记录日志） | 否 | — |
| P9 | 双向 | **B6** 控制帧 `0x0` ACK | — | 仅当请求帧置位 `0x08` 时产生 | 条件触发 | 由 BluFi 库自动处理 |

### 4.2 APP 发起的指令（APP → 设备）

| # | 帧 | 名称与含义 | 数据字段 | APP 调用 | 期望回复 |
|---|---|---|---|---|---|
| A1 | — | GATT 连接与服务发现（`0xFFFF`） | — | `BlufiClient.connect()` | 连接状态 + `onGattPrepared` |
| A2 | — | MTU 协商 | 目标 MTU | `gatt.requestMtu()` | `onMtuChanged` |
| A3 | — | 设置分包上限 20 字节（本地行为） | — | `setPostPackageLengthLimit(20)` | 无 |
| **A4** | **控制帧 `0x9`** | **Get Wi-Fi List：请求扫描周边热点** | 无 | `requestDeviceWifiScan()` | **B1**（失败则 B5） |
| **A5** | **数据帧 `0x13`** | **Custom Data：下发配网信息**（明文见 §5） | 见 §5 | `postCustomData()` | **B2 → B3** |
| **A6** | **数据帧 `0x13`** | 同 A5 内容重发一次 | 同 A5 | 定时重发 | **B2 → B3** |
| A7 | **控制帧 `0x8`** | **断开 BLE GATT 链路** | 无 | `requestCloseConnection()` | 无（GATT 断开） |
| A8 | 控制帧 `0x1` + 数据帧 `0x0` | 安全模式设置与密钥协商 | 协商数据 | 库自动 | 数据帧 `0x0` 协商数据 |
| A9 | 数据帧 `0x2`/`0x3` | BluFi 标准 SSID / Password 帧 | — | **未使用**（回调为空实现） | — |

> M2 **不使用** BluFi 标准的 SSID(`0x2`)/Password(`0x3`) 帧，配网信息全部走 Custom Data `0x13`。

### 4.3 设备上报的报文（设备 → APP）

| # | 帧 | 名称 | 数据字段 | 响应哪条请求 | APP 处理 |
|---|---|---|---|---|---|
| **B1** | **数据帧 `0x11`** | Wi-Fi List：热点列表（数据来源见 §4.5） | 逐条 `长度(1B) + RSSI(1B) + SSID(N)`，支持分片 | A4 | 组装 `ssid/rssi` 列表刷新 UI，取消列表计时；**无需回复** |
| **B2** | **数据帧 `0x13`** | Custom Data：配网回执 / 结果（明文见 §5） | 见 §5 | A5 / A6 | 回执 → 标记已收到、停止重发；含 `"failed"` → 判失败；**无需回复** |
| **B3** | **数据帧 `0xF`** | Wi-Fi Connection State Report：连接状态报告（最终结果） | `data[0]` opmode（`0x01`=STA）；`data[1]` STA 状态（`0x0` 已连有 IP / `0x1` 断开 / `0x2` 连接中 / `0x3` 已连无 IP）；`data[2]` SoftAP 连接数；`data[3]+` SSID/BSSID | A5 / A6 | `STATUS_SUCCESS` → 配网成功，否则失败（仅在已提交配置后生效）；**无需回复** |
| B4 | 数据帧 `0x10` | Version：版本信息 | `data[0]` 主版本、`data[1]` 次版本 | 无（设备主动） | 仅记录日志 |
| B5 | 数据帧 `0x12` | Report Error：错误报告 | `0x00` 序号错、`0x01` 校验错、`0x02` 解密错、`0x03` 加密错、`0x04` 安全初始化错、`0x05~0x09` DH/参数错、`0x0a` MD5 错、**`0x0b` Wi-Fi 扫描错** | A4（失败时） | `0x0b` → 提示「Scan failed, please retry later」 |
| B6 | 控制帧 `0x0` | ACK | 被确认帧的 Sequence | 条件触发 | 库内部处理 |

### 4.4 回复的两层含义

| 层次 | 帧 | 何时产生 | 谁处理 |
|---|---|---|---|
| **链路层 ACK** | 控制帧 `0x0`，data = 被确认帧的 Sequence | 仅当请求帧的 Frame Control **置位 `0x08`** 时 | BluFi 库自动收发，**业务层不感知** |
| **应用层结果帧** | B1 / B2 / B3 / B5 | 业务语义要求 | **业务层必须处理** |

> ACK 的 data 长度：BluFi 规范正文描述为 1 字节（等于被确认帧 Sequence），帧格式表标注为 2 字节，存在歧义，以 BlufiClient 实现为准。

**设备侧必须遵守的回复约束**

1. 收到 A5/A6（`0x13`）后**立即回 B2** 回执；
2. 连 AP 无论成败都**必须回 B3** 状态报告（失败时至少回 B2 的失败文本）；
3. `0x13` 会被重复下发（APP 重发、用户重试），接收处理必须**幂等**；
4. 回执文本**不得包含 `"failed"` 子串**，否则 APP 会误判为失败。

### 4.5 设备侧扫描处理流程（A4 → B1，基于 `wpa_cli`）

设备收到控制帧 `0x9`（A4）后的处理链路：

```
收到 A4（控制帧 0x9 Get Wi-Fi List）
  │
  ├─（条件）若请求帧置位 0x08 → 先回 B6 ACK
  │
  ├─ ① 触发扫描      wpa_cli scan          异步，仅下发扫描命令
  ├─ ② 等待扫描完成   wpa_cli scan_results  需轮询/等待至结果就绪
  ├─ ③ 解析结果      取 signal level → RSSI，取 ssid → SSID
  └─ ④ 封装上传      逐条按「长度(1B) + RSSI(1B) + SSID(N)」组装为数据帧 0x11（B1）
                      结果过长时置位 Frame Control 0x10 分片发送
```

`wpa_cli scan_results` 输出与帧字段的对应关系：

| `wpa_cli scan_results` 列 | 示例 | 对应 BluFi `0x11` 字段 |
|---|---|---|
| bssid | `f8:8c:21:02:d7:08` | 不上传（协议未用） |
| frequency | `2437` | 不上传 |
| **signal level** | `-35` | **RSSI**（1 字节） |
| flags | `[WPA2-PSK-CCMP][ESS]` | 不上传 |
| **ssid** | `GEEETECH-OFFICE` | **SSID**（变长） |

封装示例（单条）：

```
长度 = 1(RSSI) + len(SSID)
示例：SSID="GEEETECH-OFFICE"(16B)、RSSI=-35(0xDD)
  → 0x11 | 0x11 | 0xDD | "GEEETECH-OFFICE"
```

**时序与实现约束**

1. `wpa_cli scan` 是**异步**的，两次命令之间必须等待扫描完成，否则 `scan_results` 返回的是上一次（可能为空或过期）的结果；
2. 全流程必须在 **APP 的 10 s 列表超时内**完成（§3.2），否则 APP 判定列表获取失败；扫描较慢时建议先回 ACK 再异步上传列表；
3. 无结果时仍需回一个**空列表**的 `0x11` 帧，让 APP 正常结束等待（APP 将空列表视为「无可用热点」，而非失败）；
4. 命令执行失败或无权限时，回 **B5**（数据帧 `0x12`，错误码 `0x0b` Wi-Fi 扫描错误）。

### 4.6 APP 侧如何解析列表（格式一致性核查）

结论：**APP 业务层不解析字节流，格式核对靠 `BlufiClient` 库兜底**。

| 核查项 | 结论 | 依据 |
|---|---|---|
| APP 是否自己解析 `长度+RSSI+SSID` | **否**。解析在 `blufi.espressif.BlufiClient` 库内完成 | `WifiListConfig.java:22` 仅 `import blufi.espressif.response.BlufiScanResult` |
| APP 拿到什么 | 结构化对象列表 `List<BlufiScanResult>`，只取 `getSsid()`、`getRssi()` | `:351-356` |
| RSSI 落到哪个字段 | `GTScanResult.level = result.getRssi()`（字段名叫 level，实际存 RSSI 负值） | `:354` |
| 格式不符会怎样 | 库解析失败 → `onDeviceScanResult` 的 `status != STATUS_SUCCESS` → 走失败分支回调 `onWifiListLoaded(list, false)` | `:367-375` |
| 列表为空如何表现 | 正常成功回调（`success=true`），由 UI 显示「无可用 WiFi」 | `:357-366` |

因此设备侧必须**严格按 §4.5 的字节布局**上传，验证手段是观察 APP 侧回调：

- 回调 `success=true` 且条数/信号值与 `wpa_cli scan_results` 一致 → 格式正确；
- 回调 `success=false` 或列表为空但与 `scan_results` 不符 → 多为 RSSI 字节口径、长度字段或分片处理问题。

> 待确认：RSSI 字段是否为「有符号 1 字节原值」。`wpa_cli` 输出为负 dBm，若库实现按无符号或绝对值解析，信号值会异常。建议实机抓包比对一条已知热点（如 `-35`）确认。

---

## 5. 应用层负载（Custom Data `0x13` 承载的明文）

| 方向 | 报文 | 说明 |
|---|---|---|
| APP → 设备 | `SSID:<ssid>,PWD:<pwd>\r` | 必填；服务器地址/端口使用默认值 |
| APP → 设备 | `SSID:<ssid>,PWD:<pwd>,IP:<host>,PORT:<port>\r` | 可选扩展，用于指定服务器地址与端口 |
| 设备 → APP | `Received SSID and password` | 已收到配置（连发 **3 帧**，间隔约 500 ms） |
| 设备 → APP | `Wifi connection failed` | 连接该热点失败（连发 **3 帧**） |

字段顺序固定 `SSID:` → `PWD:` → `IP:` → `PORT:`，英文逗号分隔，`\r` 结束。

---

## 6. 端到端时序与失败路径

```
APP                                                设备
 │                                                    │
 ├── A1 GATT 连接 + 服务发现 ───────────────────────►│
 │◄── 连接状态 / onGattPrepared ──────────────────────┤
 ├── A2 MTU 协商 ───────────────────────────────────►│
 │◄── onMtuChanged ───────────────────────────────────┤
 ├── A8 安全协商（库自动）◄──────────────────────────►│
 │                                                    │
 ├── A4 控制帧 0x9  请求扫描 ───────────────────────►│
 │◄── B1 数据帧 0x11  热点列表 ───────────────────────┤   失败：B5 数据帧 0x12（0x0b）
 │      （无回复 → APP 10s 超时，提示重试）            │
 │                                                    │
 │   用户选择热点、输入密码                            │
 ├── A5 数据帧 0x13  "SSID:x,PWD:y" ────────────────►│
 │◄── B2 数据帧 0x13  "Received SSID and password" ───┤   ×3 帧
 │      （2s 内无任何 B2 → APP 重发一次 A6）           │   设备解析并连接 AP
 │◄── B3 数据帧 0xF   连接状态报告 ───────────────────┤   成功 → 配网完成
 │       失败路径：B2 "Wifi connection failed" ×3 ────┤   失败 → APP 判失败 → 重新下发
 │                                                    │
 ├── A7 控制帧 0x8  断开 BLE GATT 链路 ─────────────►│   蓝牙关闭，业务转 MQTT
```

---

## 7. 蓝牙实现的功能清单

| # | 功能 | 侧 | 对应条目 |
|---|---|---|---|
| F1 | 扫描并展示周边热点（设备侧由 `wpa_cli scan` + `wpa_cli scan_results` 获取，见 §4.5） | APP ↔ 设备 | A4 / B1 / B5 |
| F2 | 下发 Wi-Fi 账号密码 | APP → 设备 | A5 / A6 |
| F3 | 回执「已收到配置」 | 设备 → APP | B2 |
| F4 | 上报「连接热点失败」 | 设备 → APP | B2（失败文本） |
| F5 | 上报 Wi-Fi 连接状态（最终结果） | 设备 → APP | B3 |
| F6 | 配网失败后重新下发 | APP | A5（重试）/ A6（自动重发） |
| F7 | 断开蓝牙链路 | APP → 设备 | A7 |
| F8 | 安全协商、版本查询、错误报告 | 双向 | A8 / B4 / B5 |

**蓝牙不承载**：GCODE 收发、状态上报、报警、SD 卡文件列表、文件下载/打印、固件升级、绑定/解绑 —— 全部走 MQTT。

---

## 8. 代码索引（APP 侧）

| 位置（`WifiListConfig.java`） | 内容 |
|---|---|
| `:119-133` | `connect()`：建链、设 GATT 写超时、启动 10 s 列表超时 |
| `:140-169` | `refreshWifiList()` / `requestDeviceWifiScan()`（A4） |
| `:191-197` | `postCustumData()`：下发 Custom Data（A5/A6） |
| `:239-257` | `onMtuChanged()`（A2 回复） |
| `:269-288` | `onGattPrepared()`：请求 MTU（A1 回复） |
| `:325-341` | `onDeviceStatusResponse()`（B3） |
| `:343-376` | `onDeviceScanResult()`（B1） |
| `:379-381` | `onDeviceVersionResponse()`（B4） |
| `:383-421` | `onPostCustomDataResult()` / `onReceiveCustomData()`（A6 重发、B2 判定） |
| `:424-433` | `onError()`：写超时、扫描失败（B5） |
| `:439-476` | `close()` / `destroy()`（A7） |

---

## 9. 与协议文档（MXS V2.1.2）的差异与待确认

| # | 项 | 协议文档描述 | 实际 | 结论 |
|---|---|---|---|---|
| D1 | 热点列表格式 | 设备回传 `+CWLAP:(...)` 文本由 APP 解析 | APP 用控制帧 `0x9` + 数据帧 `0x11` 结构化结果，**不解析 `+CWLAP:`** | 文档 2.2 节已过时 |
| D2 | 配网数据通道 | 「APP 通过蓝牙发送 `SSID:xxx,PWD:yyy`」 | 走 Custom Data `0x13` 明文 | 一致，本文补全帧类型 |
| D3 | 配网成功判定 | 「返回 `OpMode: Station` / `got IP`」 | APP 依据数据帧 `0xF` 状态报告 | 文档描述的是设备内部日志，非 APP 判定依据 |
| D4 | 失败判定 | 「返回 `Wifi connection failed`」 | APP 匹配 `contains("failed")` | 一致，但判定过宽易误判 |
| D5 | BLE 广播名 | `MXS-<deviceId>` | 设备当前为 `M2-<deviceId>`；**本实现取值见 D11** | 待确认发布分支 |
| D6 | 设备 ID | `G` + BLE MAC | 设备当前为 `H` + MAC 且末位字符 +2；**本实现取值见 D12** | 影响 MQTT 主题与云端绑定 |
| D7 | 设备 ID 二维码 | 要求 UI 展示供 APP 扫码绑定 | 设备未实现 | APP 只能靠广播名识别设备 |
| D8 | 回执长度口径 | — | `Received...` 按含 `\0` 长度发送；`Wifi connection failed` 首帧与重发口径不一 | 端到端可能多/少 1 字节，建议统一 |
| D9 | 代码片段完整性 | — | `:280` 使用 `BlufiConstants.DEFAULT_MTU_LENGTH` 但文件内无对应 import | 疑为从 APP 工程摘出的片段，非独立可编译文件 |
| D10 | 设备侧热点获取方式 | 未描述 | 设备收到 `0x9` 后用 `wpa_cli scan` + `wpa_cli scan_results` 获取并封装为 `0x11` 上传（见 §4.5） | 属设备侧实现；需确认扫描耗时满足 APP 10 s 超时、RSSI 字节口径与库一致 |
| D11 | 广播名实际取值 | 文档 `MXS-` / APP 实测 `M2-` | 本 rv1106 实现 = `{model}-{deviceId}`（实测 `M1S-Ge33700a6620dfddc`），随型号自动变化 | 与现状一致；落地见详细设计 §4.3.2 |
| D12 | 设备 ID 前缀 | 文档 `H`+MAC | 本 rv1106 实现 `G`+`/proc/cpuinfo` Serial（复用 `config.rs::device_id_from_serial`） | 不采用 MXS 的 `H` 前缀；MQTT 主题与云端绑定以本实现为准 |

---

## 10. 一句话总结

APP 与设备的蓝牙交互只有两类：**APP 请求扫描**（控制帧 `0x9` → 设备必回 `0x11` 列表），**APP 下发配网**（数据帧 `0x13` 明文串 → 设备必回 `0x13` 回执 + `0xF` 状态报告）；配网一结束 APP 发控制帧 `0x8` 关闭蓝牙，业务全部转 MQTT。
