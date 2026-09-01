# MQTT 客户端数据流与命令清单

> 适用范围：`m2-software/Marlin-2.1.2.7-M2`（M2 / M1S 机型）
> 视角：**设备端 MQTT 客户端**（我们这一侧）
> 本文只描述 MQTT 数据流，不含 Wi-Fi 配网、BluFi、AT 指令细节。
> 代码根目录：`Marlin/src/lcd/extui/geeetech_lvgl8_ui/`

---

## 一、角色与主题

| 项 | 取值 | 代码位置 |
|---|---|---|
| 角色 | MQTT 3.1.1 客户端 | — |
| 客户端 ID / 用户名 | 设备 ID | `wifi_AT_cmd.cpp:805` |
| Broker | `cloud_hostUrl` : **1883** | `wifi_AT_cmd.cpp:64,68` |
| QoS | **1**（全部包） | `mqtt_config.mqtt_qos = 1` |
| 心跳 KeepAlive | 60 s | `CMD_MQTTCONNCFG` |
| Clean Session | `false` | `wifi_AT_cmd.cpp:70` |
| 负载上限 | **1024 字节** | `MQTT_BUFF_SIZE`，`wifi_AT_cmd.cpp:216` |
| 编码 | 紧凑 JSON（无空格无换行）、UTF-8 | `cJSON_PrintUnformatted` |

### 主题

| 方向 | 主题 | 权限 |
|---|---|---|
| 上行 | `GT/M2/UP/{deviceId}` | 设备发布 / 服务器订阅 |
| 下行 | `GT/M2/DOWN/{deviceId}` | 服务器发布 / 设备订阅 |
| 遗嘱 | `GT/M2/LWT/{deviceId}`，payload `offline` | 设备设置 / 服务器感知 |

> `DEBUG_M1S_WIFI_CONFIG` 打开时主题前缀变为 `GT/M1S/`（`wifi_AT_cmd.h:66-71`）。
> 《MXS MQTT 通信协议 V2.1.2》文档写的是 `GT/MXS/`，**与本固件不一致**，联调前需与云端确认。

---

## 二、MQTT 软件分层（只列 MQTT 相关）

```
┌──────────────────────────────────────────────────────────────┐
│ 业务触发点：UI 操作 / 打印状态变化 / 告警 / 定时器              │
└───────────────┬──────────────────────────────────────────────┘
                │ send_xxx_to_cloud()
┌───────────────▼──────────────────────────────────────────────┐
│ ④ 上行队列层  wifi_mqtt_fifo.cpp                              │
│    10 槽环形 FIFO：MQTTData{ type[32], value, msg[64] }        │
│    满则丢弃最旧；wifi_send_dev_state_cycle() 每 1s 出队 1 个    │
└───────────────┬──────────────────────────────────────────────┘
                │ mqtt_publish(type, value, msg)
┌───────────────▼──────────────────────────────────────────────┐
│ ③ 协议编解码层  wifi_AT_cmd.cpp                                │
│    mqtt_publish()      cJSON 组包 → 长度校验 → 发 AT 头        │
│    mqtt_receive_parse() 从 +MQTTSUBRECV 中抠出 JSON           │
│    process_mqtt_message()  cJSON 解包 → 按 type 分发           │
└───────┬──────────────────────────────────┬───────────────────┘
        │ 上行：AT 头 → 等 '>' → JSON       │ 下行：JSON → handler
┌───────▼──────────────────────────────────▼───────────────────┐
│ ② 业务处理层  wifi_cloud_protocol.cpp                          │
│    msg_login_handle / msg_gcode_handle /                       │
│    msg_cloud_download_start_handle / msg_download_end_handle / │
│    msg_query_new_firmware_handle / msg_filelist_handle         │
│    send_devstate_to_cloud / send_alarm_package_to_cloud        │
│    cloud_login_handle / cloud_download_file_handle /            │
│    cloud_check_update_newfirm_handle                           │
└──────────────────────────────────────────────────────────────┘
┌──────────────────────────────────────────────────────────────┐
│ ① 收发驱动层  wifi_module.cpp                                  │
│    mqtt_data_parser_state()  行组装 + '>' 补发 JSON            │
│    espGcodeFifo  GCODE 环形缓冲（288 字节）                    │
│    get_wifi_commands()  FIFO → Marlin 命令队列                 │
└──────────────────────────────────────────────────────────────┘
```

### 上行发送队列（关键约束）

**除 `login`、`upgrade_query`、`download_begin(OK)`、`download_end` 外，所有上行包必须入 FIFO**，不得直接 `mqtt_publish()`，否则会破坏串行化、与文件列表发布冲突。

| 函数 | 方式 |
|---|---|
| `send_ok_to_cloud()` | FIFO |
| `send_alarm_package_to_cloud()` | FIFO |
| `send_devstate_to_cloud()` | FIFO |
| `send_user_unbind_to_cloud()` | FIFO |
| `send_download_ok_to_cloud(..., now=true)` | **直接发布** |
| `mqtt_publish(MQTT_TYPE_LOGIN, ...)` | **直接发布**（`cloud_login_handle:701`） |
| `mqtt_publish(MQTT_TYPE_UPGRADE_QUERY, ...)` | **直接发布**（`:1342`） |
| `mqtt_publish_filelist()` | **直接发布 + 内部阻塞等 `>`（3s 超时）** |

---

## 三、完整链路

### 3.1 下行链路：APP 请求 → 服务器 → 客户端执行 → 回复

```
① APP 操作（点"开始打印"）
      │
      ▼  HTTPS / 自有协议
② 业务服务器：鉴权 → 查设备在线状态 → 组 JSON
      │
      ▼  MQTT PUBLISH，QoS=1
③ Broker → 推送到 GT/M2/DOWN/{deviceId}
      │
      ▼  ESP32 收到 → 串口吐出 "+MQTTSUBRECV:0,"GT/M2/DOWN/xxx",{...}"
④ wifi_rcv_isr_drain()      (SysTick 1ms，搬 UART 硬件缓冲 → 2KB 软缓冲)
      │
      ▼
⑤ mqtt_data_parser_state()  (wifi_module.cpp:750)
      ├─ 若 WIFI_CLOUD_CONNECTED 且本行含 '>' → 补发待发的 JSON 负载
      └─ 按 \r / \n 切行，逐字节拼入 esp_msg_buf[1024]
      │
      ▼  一行结束
⑥ mqtt_receive_parse()      (wifi_AT_cmd.cpp:328)
      ├─ strstr("+MQTTSUBRECV:")
      ├─ 从第一个 '{' 起做花括号配对，定位 JSON 边界
      ├─ 刷新 last_mqtt_recv_time、清零 mqtt_feedback_restart_cnt
      └─ process_mqtt_message(tempMsg)
      │
      ▼
⑦ process_mqtt_message()    (wifi_AT_cmd.cpp:359)
      ├─ cJSON_Parse()
      ├─ 取 "type" 字段
      └─ switch 分发到对应 msg_xxx_handle()
      │
      ▼
⑧ 业务处理（以 gcode 为例）
   msg_gcode_handle()                      (wifi_cloud_protocol.cpp:180)
      ├─ 装卸料中(load_filament_state) → 直接丢弃
      ├─ 补 \r\n
      └─ wifi_gcode_exec()                 (wifi_module.cpp:259)
            ├─ 已知 M 码 → 直接执行 + 组回复
            └─ 其他 → 写入 espGcodeFifo
                        └─ get_wifi_commands()  (gcode/queue.cpp:629)
                              └─ queue.enqueue_one() → Marlin 执行
      │
      ▼
⑨ 回复入队：send_ok_to_cloud(MQTT_TYPE_GCODE, MQTT_ACK_OK)
      │
      ▼
⑩ wifi_send_dev_state_cycle() 每 1s 出队 1 个 → mqtt_publish()
      │
      ▼
⑪ mqtt_publish()            (wifi_AT_cmd.cpp:93)
      ├─ cJSON 组包 → cJSON_PrintUnformatted
      ├─ strlen >= 1024 → 丢弃并报错
      ├─ 存入 mqtt_publish_buff
      └─ sprintf(AT+MQTTPUBRAW, id, len, qos) → 发送（第一段：只有 AT 头）
      │
      ▼
⑫ mqtt_data_parser_state() 检测到 ESP32 回 '>'
      └─ raw_send_to_wifi(mqtt_publish_buff)（第二段：裸 JSON）
      │
      ▼
⑬ Broker → GT/M2/UP/{deviceId} → 服务器 → APP
```

**两段式发布是硬约束**：`AT+MQTTPUBRAW` 先声明长度，ESP32 回 `>` 后才能灌 JSON。第一段和第二段分散在两次主循环里，`mqtt_publish_buff` 是全局中转。

### 3.2 上行链路：设备主动上报

```
事件源（打印状态变化 / 温度 / 告警 / 定时器 / SD 卡插拔）
   → send_xxx_to_cloud() → fifo_enqueue(&mqtt_fifo)
   → [wifi_send_dev_state_cycle，每 1s 出队 1 个]
   → mqtt_publish() → 两段式发送 → Broker → 服务器
```

### 3.3 时序全景（登录为例）

```
设备                                    Broker/服务器
  │                                          │
  │──── PUBLISH UP  {type:"login", ...} ────►│
  │                                          │ 校验设备、查绑定关系
  │◄─── PUBLISH DOWN {type:"login",          │
  │        bindState:0, account:"138..."} ───┤
  │                                          │
  │ msg_login_handle(0)                      │
  │   ├─ login_success_flag = true           │
  │   ├─ 保存 account 到 Flash               │
  │   └─ send_devstate_to_cloud(ALL) 入队 4 包│
  │                                          │
  │──── PUBLISH UP {status_hardware}   ────► │  1s 间隔依次发出
  │──── PUBLISH UP {status_temp_fan}   ────► │
  │──── PUBLISH UP {status_level}      ────► │
  │──── PUBLISH UP {status_print}      ────► │
  │                                          │
  │ ... 之后进入周期上报（忙 3s / 闲 60s）...
```

---

## 四、下行命令清单（服务器 → 设备）

主题 `GT/M2/DOWN/{deviceId}`，QoS=1。全部由 `process_mqtt_message()`（`wifi_AT_cmd.cpp:359-452`）分发。

| # | type | 关键字段 | 处理函数 | 客户端行为 | 回复 |
|---|---|---|---|---|---|
| 1 | `login` | `bindState`, `account` | `msg_login_handle` | `0`→登录成功，存账号，开启周期上报；`1`→未绑定，退回配网 | 全量 4 个状态包 |
| 2 | `status_query` | — | `send_devstate_to_cloud` | 立即上报全量状态 | 4 个状态包（1s 内依次发出） |
| 3 | `gcode` | `gcodeCmd` | `msg_gcode_handle` | 执行 G/M 码 | `gcode` + `execResult`（见第五节） |
| 4 | `download_begin` | `fileType`, `serverIp`, `fileName`, `url` | `msg_cloud_download_start_handle` | 校验 SD 卡/忙闲，启动 HTTP 下载 | `download_begin` + `transState`/`errCode` |
| 5 | `download_end` | `ackState` | `msg_download_end_handle` | `"OK"`→结束；`"ERROR"`→标记失败 | 无（终止重发） |
| 6 | `upgrade_query` | `serverIp`, `mcuFile`, `espFile` | `msg_query_new_firmware_handle` | 判断 MCU/ESP 有无新固件，启动下载 | 无直接回复（后续走 download 流程） |
| 7 | `file_list` | — | 置 `report_filelist_flag` | 延迟 500ms（等 FIFO 排空）后扫描 SD 卡 | 若干包 `file_list` |
| 8 | `server_unbind` | — | 清 bind_state + bind_account，存 Flash | 解绑 | 无 |
| 9 | `device_unbind` | — | 同上 | 解绑 | 无 |

### 4.1 `download_begin` 处理细节

```
msg_cloud_download_start_handle()           (wifi_cloud_protocol.cpp:195)
├─ download_step_state != NOCMD   → 忽略（已有下载在进行）
├─ !card.isMounted()              → download_begin(errCode=2, ERROR) + alarm(11 无SD卡)
├─ login_step_state > LOGIN_NOCMD → download_begin(errCode=1, ERROR)（忙）
├─ url 非空                       → cloud_down_flag=true，文件名取 url 最后一段
├─ 否则 serverIp + fileName       → cloud_down_flag=false
└─ → download_begin(errCode=0, OK)【直接发布】→ 1s 后启动 HTTP 下载
```

`errCode` 定义：`0` 成功 / `1` 打印机忙 / `2` 无 SD 卡

### 4.2 `upgrade_query` 回复处理

```
msg_query_new_firmware_handle()             (:265)
├─ serverIp == "127.0.0.1"  → 清空，后续用 MQTT broker IP 兜底
├─ mcuFile/espFile 长度 > 2（即不等于 "NA"）
│    ├─ 两者都有 → FIRMWARE_RET_NEW_MCU_ESP
│    ├─ 仅 MCU   → FIRMWARE_RET_NEW_MCU
│    └─ 仅 ESP   → FIRMWARE_RET_NEW_ESP
└─ 否则 → FIRMWARE_RET_NONEW，恢复周期上报
```

超时：有固件 120 s / 无固件 60 s。

### 4.3 `file_list` 处理细节

```
置 report_filelist_flag = true
   → wifi_send_filelist_cycle()              (:609)
       ├─ 等 FIFO 排空（最多等 2s 超时强制发）
       ├─ 再延迟 500ms
       └─ msg_filelist_handle() → send_devstate_filelist_to_cloud()
             ├─ SD 已挂载 → card.send_SD_filelist_to_cloud() 分页扫描并逐页发布
             └─ 未挂载   → mqtt_publish_filelist(0, nullptr, 0, 0) 发空列表
```

---

## 五、GCODE 子命令完整清单

由 `wifi_gcode_exec()`（`wifi_module.cpp:259-709`）处理。`load_filament_state != 0`（装卸料中）时**全部 GCODE 被丢弃**。

| 指令 | 功能 | 前置条件 | 回复 execResult | 附带上报 |
|---|---|---|---|---|
| `M20` | 查询 SD 卡文件列表 | 非打印中 | **无 gcode 回复** | 若干 `file_list` 包 |
| `M21` | 初始化 SD 卡 | — | `OK` | — |
| `M23 <file>` | 选中文件（不打印） | — | 走 default：`OK` | — |
| `M24` | 开始/恢复打印 | — | `OK` | `status_print`（state=1） |
| `M25` | 暂停打印 | — | `OK` | `status_print`（state=3） |
| `M26` | 停止打印文件 | 打印中/暂停中 | `OK` | — |
| `M27` | 上报打印速率 | — | 无回复（空实现） | — |
| `M28 <file>` | 开始传输文件到文件系统 | `print_state == IDLE` | 无回复 | — |
| `M29` | 自动调平 | — | **无回复** | `status_level`（调平中） |
| `M30 <file>` | 删除 SD 卡文件 | SD 已挂载 | `OK` / `Delete failed` | 失败时 `alarm`(11) |
| `M32 <file>` | 选中并开始打印 | 非打印、调平已激活 | `File selected OK` / `File open failed` | `status_print` |
| `M106 P<n> S<0-255>` | 风扇控制 P1散热/P2主板/P3辅助 | P 存在 | `OK` | `status_temp_fan` |
| `M115` | 获取固件信息 | — | `OK` | — |
| `M145 S<0-3>` | 材料预热 0停/1PLA/2TPU/3ABS | — | `OK` | `status_temp_fan`（+`status_print`） |
| `M150 S<0/1>` | 呼吸灯开关 | — | `OK` | `status_hardware` |
| `M230 S<0/1>` | 蜂鸣器开关 | — | `OK` | `status_hardware` |
| `M524` | 终止打印 | — | `OK` | `status_print`（state=5） |
| `M992` | （预留）暂停相关 | 打印中/暂停中 | 无回复 | — |
| `M994` | 上报文件路径与大小 | 打印中/暂停中 | 无回复 | — |
| `M997` | 固件升级 | — | 无回复（转发 `M997r`） | — |
| `M998` | 文件系统切换 | — | 无回复（空实现） | — |
| `M2000 S<0/1>` | 断电续打 0取消/1开启 | — | `OK` / `ERROR` | `status_print` |
| `M2001 S<0/1>` | APP 在线/离线 | — | **`ERROR`**（代码固定，见下方注） | — |
| `G29` / `G29N` | 自动调平 | `print_state == IDLE` | **无回复** | `status_level` |
| **其他任意 G/M/T** | 透传给 Marlin | FIFO 有空间 | `OK`（空间 ≥ len+96 时） | — |

### 异常分支（走 `alarm` 包而非 gcode 回复）

| 场景 | errType | errMsg |
|---|---|---|
| `M32` 文件非 `.g`/`.G` | 20 `ERR_FILE_FORMAT` | `Unsupported file format` |
| `M32` 正在打印 | 13 `ERR_PRINTER_BUSY` | `Printer is busy printing` |
| `M32` 状态为 WORKING | 13 | `Printer is already working` |
| `M32` 未调平 | 21 `ERR_LEVELING_NEEDED` | `Please level the bed first` |
| `M30` 无 SD 卡 | 11 `ERR_NO_SDCARD` | `No SD card` |
| `M145 S` 越界（非 0/1/2） | 12 `ERR_PREHEAT_OUT` | `Index: 1-PLA, 2-TPU, 3-ABS\r\n` |
| `G29` 打印机非空闲 | 13 | `MSG_ERROR_BUSY` |

### 注

- **`M2001` 回复固定为 `ERROR`**（`wifi_module.cpp:655`），但会正确切换 `network_state` 到 `NET_SERVER_LOGINNED` / `NET_APP_ONLINE`。若 APP 依赖此回复判断在线状态，需修代码。
- **`M32` 成功后会自动 `queue.inject("M24")`**，即选中即开始打印。
- **未被 switch 捕获的 M 码**走 default 分支进 `espGcodeFifo`，由 `get_wifi_commands()` 注入 Marlin 队列；`M104`/`M140`/`M851`/`M355` 等走此路径。
- 协议文档 V2.0.9 要求 gcode 回复带 `cmdType`，**本固件未实现**（`wifi_AT_cmd.cpp:177-181` 只有 `execResult`）。

---

## 六、上行命令清单（设备 → 服务器）

主题 `GT/M2/UP/{deviceId}`，QoS=1。全部由 `mqtt_publish()`（`wifi_AT_cmd.cpp:93-228`）组包。

| # | type | 触发时机 | JSON 字段 | 发送方式 |
|---|---|---|---|---|
| 1 | `login` | MQTT 订阅成功后 | `type,id,mb,sf1,sf2,wf,ip,lang,ts` | 直接，超时 5s 重发 |
| 2 | `alarm` | 异常发生 | `type,id,errType,errMsg,ts` | FIFO |
| 3 | `status_hardware` | 周期/事件/查询 | `type,id,runout,beep,light,Breathing,sd,level,door,ts` | FIFO |
| 4 | `status_temp_fan` | 周期/事件/查询 | `type,id,preheatType,preheatState,heatState,nozzleTargetTemp,nozzleActualTemp,bedTargetTemp,bedActualTemp,mainFanSpeed,boardFanSpeed,auxFanSpeed,ts` | FIFO |
| 5 | `status_level` | 周期/事件/查询 | `type,id,levelingStatus,levelingPoint,levelingTotalPoints,ts` | FIFO |
| 6 | `status_print` | 周期/事件/查询 | `type,id,printState,zOffset,printProgress,printElapsedTime{hour,min,sec},printRemainTime{hour,min,sec},currentPrintFile,ts` | FIFO |
| 7 | `gcode` | 执行完下行 GCODE | `type,id,execResult,ts` | FIFO |
| 8 | `download_begin` | 收到下载指令后校验完毕 | `type,id,fileType,filename,transState,errCode,ts` | `OK` 直接发 / 错误走 FIFO |
| 9 | `download_end` | HTTP 下载结束 | `type,id,fileType,filename,transState,errCode,ts` | 直接发，**每 1s 重发，最多 10 次** |
| 10 | `upgrade_query` | 开机 2s 后 / UI 触发 | `type,id,ts` | 直接 |
| 11 | `file_list` | 收到 `file_list` 查询 / M20 | `type,id,fileTotal,fileIndex,fileList[{fileName,fileSize}],ts` | 直接 + 阻塞等 `>` |
| 12 | `device_unbind` | 设备端发起解绑 | `type,id,ts` | FIFO |

### 6.1 状态上报周期

`wifi_send_dev_state_cycle()`（`wifi_cloud_protocol.cpp:644-681`）

| 条件 | 间隔 | 代码 |
|---|---|---|
| 打印中 / 加热中 / 喷嘴 > 50℃ / 调平中 | **3 s** | `:670-671` |
| 空闲 | **60 s** | `:672-673` |
| 登录成功、收到 `status_query`、GCODE 触发状态变化 | 立即 | — |

> 协议文档写忙 10s / 闲 5min，与代码不符。

发送前置条件（任一不满足则跳过）：`login_success_flag == true`、无登录流程、无下载流程、无固件流程、无文件列表发送。

### 6.2 上报暂停时机

`wifi_send_pause`（M997 / 固件下载时置 1）和 `send_filelist_to_cloud_flag` 会暂停周期上报。

### 6.3 字段映射（`refresh_devstate_data()`，`:403-555`）

| JSON 字段 | 数据来源 |
|---|---|
| `runout` | `runout.enabled` |
| `beep` | `buzzer.onoff` |
| `light` | `caselight.on` |
| `Breathing` | `leds.lights_on` |
| `sd` | `card.isMounted()` |
| `level` | `leveling_is_valid()` |
| `door` | `!door_status` |
| `nozzleTargetTemp` / `nozzleActualTemp` | `nozzle_target_temp` / `thermalManager.wholeDegHotend(0)` |
| `bedTargetTemp` / `bedActualTemp` | `bed_target_temp` / `thermalManager.wholeDegBed()` |
| `heatState` | `nozzle_target_temp > 0 \|\| bed_target_temp > 0` |
| `mainFanSpeed` | `hotend_status.fan0_speed`（0-100%） |
| `boardFanSpeed` | `hotend_status.fan1_speed` |
| `auxFanSpeed` | 恒为 **0** |
| `preheatType` / `preheatState` | 调平中或打印中时强制归 0 |
| `levelingStatus` | `level_state`：0未调平/1调平中/2完成/3失败 |
| `levelingTotalPoints` | `GRID_MAX_POINTS_X * GRID_MAX_POINTS_Y` |
| `zOffset` | `(int16_t)(probe.offset.z * 100)` |
| `printProgress` | `ui.get_progress_percent()` |
| `printElapsedTime` | `print_job_timer.duration()` |
| `printRemainTime` | `elapsed * (100-progress) / progress`（仅 0<progress<100） |
| `currentPrintFile` | 续打取 `recovery.info.sd_filename`，否则 `card.longest_filename()` |
| `printState` | 0空闲/1打印/2完成/3暂停/4失败/5取消/6断电续打/7换料 |
| `ts` | `millis()`（**不是 Unix 时间戳**） |

> 完成/取消状态持续 **30 s** 后自动回落为空闲（`:455-464`）。

### 6.4 `file_list` 分页

- 每包最多 **10** 个文件（`MQTT_FILELIST_PUB_NUMBER`）
- `fileTotal` = 总页数 = `ceil(文件总数 / 10)`；`fileIndex` = 当前页号（从 1 开始）
- SD 卡未挂载时发 `fileTotal=0, fileIndex=0, fileList=[]`
- 发送期间 `mqtt_publishing_filelist_flag = true`，会**禁止 ISR 排空 UART**，避免 `>` 被吞

---

## 七、告警上报（`alarm`）

`send_alarm_package_to_cloud(errType, errMsg)` → FIFO。**服务器不回复**。

| errType | 含义 | 当前代码触发点 |
|---|---|---|
| 0 | 未定义 | — |
| 1 | 喷嘴加热失败 | 未接入 |
| 2 | 热床加热失败 | 未接入 |
| 3 | 断料检测，已自动暂停 | 未接入 |
| 4 | 打印中异常断电 | 未接入 |
| 5-7 | X/Y/Z 归位失败 | 未接入 |
| 8 | 最低温度 | 未接入 |
| 9 | 最高温度 | 未接入 |
| 10 | 耗材用完 | 未接入 |
| 11 | 无 SD 卡 | M30/M32 前置失败、固件下载前 |
| 12 | 预热索引越界 | `M145 S` 非法 |
| 13 | 打印机忙 | `M32`/`G29` 忙 |
| 14 | 数据包过长 | 未接入 |
| 15 | 数据包无尾 | 未接入 |
| 16 | 探针探测失败 | 未接入 |
| 17 | 加热失败 | 未接入 |
| 18 | 温度失控 | 未接入 |
| 19 | 电力中断 | 断电续打恢复时（`status_print` 中检测） |
| 20 | **文件格式错误**（文档为"门打开"） | `M32` 非 `.g` 文件 |
| 21 | 需要调平（代码扩展） | `M32` 未调平 |

> 温度/断料/限位等 Marlin 原生告警目前**未接入 MQTT**，需要时在对应位置调用 `send_alarm_package_to_cloud()` 即可。

---

## 八、下载/升级流程中的 MQTT 部分

MQTT 只负责**握手**，实际数据走 HTTP（非本文范围）。

```
服务器                                     客户端
  │── download_begin{fileType,url/serverIp,fileName} ──►│
  │                                                     │ 校验
  │◄── download_begin{transState:"OK",errCode:0} ───────┤ 【立即发】
  │                                                     │
  │        ... HTTP 下载（ESP32 侧），MQTT 已断开 ...     │
  │                                                     │
  │◄── download_end{transState,errCode} ────────────────┤ 【每 1s 重发，最多 10 次】
  │── download_end{ackState:"OK"} ────────────────────► │ 收到即停止重发
```

- `download_end` 重发 10 次仍未收到 `ackState` → 放弃，记 `log_mqtt_reconnect("Download ack timeout")`，重置登录状态
- `errCode`（end）：`0` 成功 / `1` 传输超时 / `2` SD 卡弹出
- 下载结束后需由服务器再下发 `M32`+`M24`（打印）或 `M997`（升级）才会真正执行

---

## 九、超时与重传

| 场景 | 阈值 | 动作 |
|---|---|---|
| `login` 等回复 | 5 s（`login_wifi_overtime_ms`） | 重发；累计 3 次超时记 1 次失败；失败 5 次 → `AT+MQTTCLEAN=0` + 重建 MQTT 连接 |
| `upgrade_query` 等回复 | 无固件 60 s / 有固件 120 s | 退出固件流程，恢复周期上报 |
| `download_end` 等 ACK | 1 s × 10 次 | 放弃，记录日志 |
| FIFO 出队 | 1 s / 包 | — |
| 无任何 `+MQTTSUBRECV` | **3 min** | 分级重启：1-2 次重建 MQTT → 3-4 次清参数重配 → 5+ 次硬件复位 |
| 文件列表发布等 `>` | 3 s | 超时放弃并清理缓冲 |

**3 分钟无下行检测的前置条件**（避免误触发）：已登录 + 无登录流程 + 无下载流程 + 无固件流程 + 无 Wi-Fi 配置动作。

---

## 十、扩展开发 SOP

### 新增一个下行命令

1. `wifi_AT_cmd.h:91-105` 加 `#define MQTT_TYPE_XXX "xxx"`
2. `wifi_AT_cmd.cpp:359` `process_mqtt_message()` 加 `else if (strstr(pType, MQTT_TYPE_XXX))`，用 `cJSON_GetObjectItem` + `cJSON_IsString/IsNumber` 取字段
3. `wifi_cloud_protocol.cpp` 实现 `msg_xxx_handle(...)`
4. 需回复 → `send_ok_to_cloud(MQTT_TYPE_XXX, MQTT_ACK_OK)`（**必须走 FIFO**）

### 新增一个上行命令

1. 同上加 type 宏
2. `wifi_AT_cmd.cpp:93` `mqtt_publish()` 加 `else if (strstr(mqtt_type, MQTT_TYPE_XXX))` 分支组 JSON
3. 业务侧：
   ```cpp
   MQTTData data;
   data.mqtt_value = 0;
   ZERO(data.mqtt_msg);
   ZERO(data.mqtt_type);
   sprintf(data.mqtt_type, "%s", MQTT_TYPE_XXX);
   fifo_enqueue(&mqtt_fifo, &data);
   ```

### 新增一条远程 GCODE

`wifi_module.cpp:286` 的 `switch(cmd_value)` 加 `case N:`；用 `send_ok_to_cloud(MQTT_TYPE_GCODE, MQTT_ACK_*)` 回复；ACK 枚举见 `wifi_cloud_protocol.h:195-202`；如需回传自定义文本，扩展该枚举并在 `send_ok_to_cloud()`（`:322`）加分支。

### 调试开关

| 宏（`Configuration.h:31-41`） | 作用 |
|---|---|
| `ENABLE_MQTT_DEBUG`（`wifi_AT_cmd.cpp:57`） | 连本地 broker `192.168.31.107`，user/pwd=admin，打印配置 |
| `DEBUG_ENABLE_WIFI_CONFIG_STATUS` | 打印每条 AT 与应答 |
| `DEBUG_ENABLE_DOWNLOAD_OVERTIME` | 下载倒计时 |
| `mqtt_data_parser_state()` 内注释块（`:777-781`） | 取消注释可打印裸 MQTT 报文 |
| `log_mqtt_reconnect(reason, detail)` | 统一输出重连原因 + 各状态机快照 |

---

## 十一、与《MXS MQTT 通信协议 V2.1.2》的差异汇总

| 项 | 文档 | 本固件 | 位置 |
|---|---|---|---|
| 主题前缀 | `GT/MXS/` | `GT/M2/`（M1S 时 `GT/M1S/`） | `wifi_AT_cmd.h:66-87` |
| 设备 ID 前缀 | `G`+MAC | `H`+MAC，末位字符 +2 变换 | `wifi_module.cpp:852-878` |
| 状态上报周期 | 忙 10s / 闲 5min | 忙 3s / 闲 60s | `wifi_cloud_protocol.cpp:670` |
| gcode 回复 | 需带 `cmdType` | 未实现，只有 `execResult` | `wifi_AT_cmd.cpp:177` |
| download 字段 | `fileName` | `filename`（小写 n） | `wifi_AT_cmd.cpp:186,194` |
| errType=20 | 门打开 | 文件格式错误；另有 21=需调平 | `wifi_cloud_protocol.h:90-91` |
| ts 字段 | 时间戳 | `millis()` 毫秒计数 | `wifi_AT_cmd.cpp:109` 等 |
| 遗嘱 | V2.1.0 删除 JSON 遗嘱 | 已设 LWT 且额外订阅 LWT 主题 | `wifi_AT_cmd.h:82,85` |
