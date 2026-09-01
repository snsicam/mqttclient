# MXS MQTT 通信协议（V2\.1\.1）

发布单位：深圳市捷泰技术有限公司
发布日期：2026年03月04日

## 修订记录

|修订日期|版本号|修订内容|
|---|---|---|
|2026\-03\-04|V2\.0\.0|初版发行|
|2026\-03\-06|V2\.0\.1|校验版本|
|2026\-03\-07|V2\.0\.2|校验版本|
|2026\-03\-11|V2\.0\.3|1、主题增加设备ID；<br>2、JSON结构加入时间戳；|
|2026\-03\-12|V2\.0\.4|1、JSON结构中加入设备ID；<br>2、JSON数据中去除” \\r\\n”字符；|
|2026\-03\-13|V2\.0\.5|主题类型移到JSON中|
|2026\-03\-13|V2\.0\.6|状态包过大拆分成：<br>1、硬件状态包；<br>2、温度风扇状态包；<br>3、调平状态包；<br>4、打印状态包；|
|2026\-03\-16|V2\.0\.7|1、登录添加lang字段：0表示中文版，1表示非中文版<br>2、登录回复添加account字段：根据登录信息回复手机号或者邮箱<br>3、调平消息需要保持旧字段名字：levelingStatus，levelingPoint，levelingTotalPoints|
|20260317|V2\.0\.8|1、打印状态zoffset浮点值改带符号整数=浮点值×100；<br>2、更新遗嘱消息为：GT/MXS/LWT/\{deviceId\}；|
|20260320|V2\.0\.9|1、Gcode命令添加\&\#34;cmdType\&\#34;:\&\#34;Gcode代码，不包括变量\&\#34;，用来区分哪条命令的执行结果；<br>2、遗嘱订阅命令加入列表；|
|20260320|V2\.1\.0|1、 download\_begin/download\_end协议增加文件名和文件类型；<br>2、 删除遗嘱JSON协议，模块不支持遗嘱长文件包。|
|20260616|V2\.1\.1|1、download\_begin 增加 url 字段：云端文件优先使用 url 直接从 COS 下载；<br>2、url 字段为空或不存在时，设备回退到旧逻辑（serverIp + fileName 拼接切片服务器地址）；<br>3、TCP 设备不受影响，继续走切片服务器 302 重定向。|
|20260721|V2\.1\.2|1、设备状态中预热状态从打印状态项移入风扇加热状态项|

## 一、打印机设备ID

设备ID用于设备联网服务器认证，格式为G\+BLE MAC地址（去除MAC地址中的冒号）；设备固件需将设备ID生成二维码显示在UI界面，供手机APP扫描绑定。

- 示例：BLE MAC为00:11:22:33:44:55 → 设备ID为G001122334455

- 生成规则：查询ESP32通信模块的BLE MAC地址，拼接前缀G后生成二维码。

## 二、配网及重新配网

设备通过ESP32的BluFi功能（基于蓝牙通道的Wi\-Fi配置）实现手机APP配网，全程保持蓝牙连接，具体操作步骤如下：

### 2\.1 设备开启BluFi配网

未配网/手动激活「配置网络」后，设备进入BLUFI模式，蓝牙广播名：MXS\-设备ID（如MXS\-G001122334455）。

### 2\.2 手机创建BluFi连接并获取热点信息

手机开启蓝牙\+GPS，通过EspBluFi应用连接上述BLE设备；设备通过蓝牙发送周边WiFi热点信息，格式：

|热点信息格式|
|---|
|Plaintext \+CWLAP:\(,\&lt;\&\#34;ssid\&\#34;\&gt;,,\&lt;\&\#34;mac\&\#34;\&gt;,,\&lt;freq\_offset\&gt;,\&lt;freqcal\_val\&gt;,\&lt;pairwise\_cipher\&gt;,\&lt;group\_cipher\&gt;,,\)|

- 核心参数：ssid（热点名称）、rssi（信号强度，负值，值越大信号越强），APP需按rssi从强到弱排列热点。

- 示例：\+CWLAP:\(4,\&\#34;GEEETECH\-OFFICE\&\#34;,\-35,\&\#34;f8:8c:21:02:d7:08\&\#34;,1,\-1,\-1\.5,3,7\.0\)

### 2\.3 APP发送WiFi配置信息

APP通过蓝牙向设备发送SSID和密码，指令格式：SSID:xxxx,PWD:yyyy（IP、PORT为可选参数，省略则用默认值）。

- 设备成功接收后回复：Received SSID and password，无回复则判定发送失败。

### 2\.4 设备连接WiFi热点

设备不中断蓝牙连接，根据配置信息连接WiFi，向APP反馈两种状态：

- 连接成功：返回OpMode: Station、Station connect Wi\-Fi now, got IP等信息，包含bssid、ssid；

- 连接失败：返回Wifi connection failed。

### 2\.5 模块连接远程服务器

设备根据MQTT服务器账号密码登入服务器，完成远程登录认证。

## 三、正常联机通信内容

设备与服务器联机后，支持以下核心通信功能：

1. GCODE指令收发、设备状态上报与查询；

2. 设备异常错误信息主动上传；

3. SD卡文件列表的查询与操作；

4. GCODE文件下载（HTTP）\+ 打印（MQTT指令）分阶段执行；

5. 固件下载（HTTP）\+ 升级（MQTT指令）分阶段执行。

## 四、MQTT协议基本规范

### 4\.1 MQTT基础配置

|配置项|取值/说明|
|---|---|
|版本|MQTT 3\.1\.1（兼容MQTT 5\.0）|
|连接方式|TCP/IP（默认1883端口），支持TLS/SSL加密（8883端口）|
|客户端ID|设备唯一标识，格式为G\{蓝牙MAC地址\}（与设备ID一致）|
|清理会话|Clean Session = 0（保持会话，重连后接收离线消息）|
|心跳保活|Keep Alive = 60s（MQTT底层心跳，保障链路连接检测）|
|遗嘱消息（LWT）|异常断连时自动发送，主题GT/MXS/LWT/\{deviceId\}, QoS=1|

### 4\.2 主题设计规范

采用分层级只读/只写主题，区分设备上行（设备→服务器）、服务器下行（服务器→设备），避免双向通信冲突：

|通信方向|主题格式|权限|说明|
|---|---|---|---|
|设备上行|GT/MXS/UP/\{deviceId\}|设备发布、服务器订阅|\{deviceId\}为蓝牙MAC地址；|
|服务器下行|GT/MXS/DOWN/\{deviceId\}|服务器发布、设备订阅|与上行\{deviceId\}一一对应|
|示例|设备发送登录包 → 发布主题GT/MXS/UP/G1afe3598d37b；服务器回复登录包 → 发布主题GT/MXS/DOWN/G1afe3598d37b|\-|\-|

### 4\.3 消息QoS等级定义

根据业务重要性划分QoS，保障消息传输可靠性：

|QoS等级|适用场景|
|---|---|
|QoS 1|绑定包、错误报警包、设备状态包、GCODE指令包、文件/固件传输包、SD卡查询包、解绑包（核心业务，确保至少送达一次）|

### 4\.4 消息负载格式

1. 抛弃原二进制格式，采用无缩进、无空格的JSON格式，保留原协议核心字段；

2. 16/32位数据遵循小端模式，所有字符串字段采用UTF\-8编码；

3. 单条MQTT消息负载大小不超过1024字节，文件传输相关内容分段处理。

### 4\.5 协议限制

1. 单条负载最大1024字节，与原协议包最大空间一致；

2. JSON序列化必须紧凑（无缩进、无空格）；

3. 字符串统一使用UTF\-8编码，与原协议保持兼容。

## 五、协议包分解

所有协议包均遵循上述MQTT规范，按业务类型划分，各类包的发送方、主题、QoS、负载格式及说明如下，所有协议包QoS均为1。

### 5\.1 登录包\(login\)

#### 核心说明

- 设备MQTT连接成功后5秒内发布，为首个业务包，用于上报设备基础信息；

- 5秒内未收到服务器回复则超时，超时3次后停止发送，进入定时重连\+重发流程；

- 设备断网重连后，自动重发登录包。

#### 设备→服务器（上报）

- 主题：GT/MXS/UP/\{deviceId\}

- 负载JSON格式：

```JSON
{
"type": "login",
"id": "G{蓝牙MAC地址}",
"mb": "主板版本号",
"sf1": "MCU软件版本号",
"sf2": "ESP软件版本号",
"wf": "WIFI名称",
"ip": "设备IP地址",
"lang": 0/1,
"ts": "时间戳"
}
```

lang说明：语言标识符 0\-表示中文版，1表示非中文版。

- 示例：

```JSON
{
"type": "login",
"id": "G1afe3598d37b",
"mb": "GT_FM_Mozi_V1.0",
"sf1": "MXS_FM_V1.07",
"sf2": "ESP32_V4.1.0.0",
"wf": "GEEETECH-GUEST",
"ip": "192.168.1.188",
"lang":0,
"ts": 23456789
}
```

#### 服务器→设备（回复）

- 主题：GT/MXS/DOWN/\{deviceId\}

- 负载JSON格式：

```JSON
{
"type": "login",
"id": "G{蓝牙MAC地址}",
"bindState": 整数,
"account":账号,
"ts": "时间戳"
}
```

bindState说明：0\-表示已绑定；1\-表示该序列号未绑定；2\-表示该序列号未录入
account说明：中文固件\-手机号码；非中文固件\-邮箱地址

- 示例1（传入lang标识为0，中文版本固件）：

```JSON
{
"type": "login",
"id": "G1afe3598d37b",
"bindState": 1,
"account":"13812345678",
"ts": 234567890
}
```

- 示例2（传入lang标识为1，非中文版本固件）：

```JSON
{
"type": "login",
"id": "G1afe3598d37b",
"bindState": 0,
"account":"test@gmail.com",
"ts": 234567890
}
```

### 5\.2 错误报警包\(alarm\)

#### 核心说明

- 设备出现异常时主动发布，服务器无需回复；

- 错误类型分已定义类型（1\-20，对应具体故障）和未定义类型（0，需附带具体错误信息）；

- 已定义类型的errMsg字段填\&\#34;NA\&\#34;，未定义类型需填写详细错误描述。

#### 设备→服务器（发送）

- 主题：GT/MXS/UP/\{deviceId\}

- 负载JSON格式：

```JSON
{
"type": "alarm",
"id": "G{蓝牙MAC地址}",
"errType": 整数,
"errMsg": "错误信息字符串",
"ts": "时间戳"
}
```

errType说明：0\-未定义；1\-喷嘴加热失败；2\-热床加热失败\.\.\.

- 示例1（已定义类型）：

```JSON
{
"type": "alarm",
"id": "G1afe3598d37b",
"errType": 1,
"errMsg": "NA",
"ts": 12467890765
}
```

- 示例2（未定义类型）：

```JSON
{
"type": "alarm",
"id": "G1afe3598d37b",
"errType": 0,
"errMsg": "Unknown error: UART communication failure",
"ts": 12467890765
}
```

#### 错误类型定义表（十进制）

|错误类型|错误信息内容|
|---|---|
|1|喷嘴加热失败（Nozzle heating failed）|
|2|热床加热失败（Bed heating failed）|
|3|断料检测报警，打印已自动暂停|
|4|打印过程中异常断电|
|5\-7|X/Y/Z轴归位失败|
|8|最低温度（MINITEMP）|
|9|最高温度（MAXTEMP）|
|10|耗材用完（Filament run out）|
|11|SD卡不存在（No SD Card）|
|12|预热索引范围超出（1\-PLA 2\-TPU）|
|13|打印忙（printer is busy）|
|14|数据包数据太长|
|15|数据包无尾|
|16|探针探测失败（Probing Failed）|
|17|加热失败（Heating Failed）|
|18|温度失控（Thermal Runaway）|
|19|电力中断（Power Outage）|
|20|门打开|

### 5\.3 设备状态信息包\(status\_hardware/status\_temp\_fan/status\_level/status\_print\)

#### 核心说明

- 设备上传间隔区分三种状态：打印/预热/调平/文件下载等操作时10秒/次、空闲时5分钟/次、事件产生时实时上传；

- 服务器仅查询指令需设备响应，主动发布的状态包无需回复；

- 负载包含硬件、加热、打印、调平等全量设备信息，替代原心跳包实现链路检测。

#### 设备→服务器（主动发布）

##### 硬件状态包

- 主题：GT/MXS/UP/\{deviceId\}

- 负载JSON格式：

```JSON
{
"type": "status_hardware",
"id": "G{蓝牙MAC地址}",
"runout": 0/1,
"beep": 0/1,
"light": 0/1,
"Breathing": 0/1,
"sd": 0/1,
"level": 0/1,
"door": 0/1,
"ts": "时间戳"
}
```

字段说明：
a\. runout\-断料检测\(0关/1开\)；
b\. beep\-蜂鸣器\(0关/1开\)；
c\. light\-照明LED\(0关/1开\)；
d\. Breathing\-呼吸LED\(0关/1开\)；
e\. sd\-SD卡\(0拔出/1插入\)；
f\. level\-调平\(0未调平/1已调平\)；
g\. door\-门磁\(0开/1关\)

- 示例：

```JSON
{
"type": "status_hardware",
"id": "G1afe3598d37b",
"runout":0,
"beep":0,
"light":0,
"Breathing":0,
"sd":0,
"level":0,
"door":0,
"ts": 12467890765
}
```

##### 温度风扇状态包

- 主题：GT/MXS/UP/\{deviceId\}

- 负载JSON格式：

```JSON
{
"type": "status_temp_fan",
"id": "G{蓝牙MAC地址}",
"preheatType": 0-3,
"preheatState": 0/1,
"heatState": 0/1,
"nozzleTargetTemp": 整数,
"nozzleActualTemp": 整数,
"bedTargetTemp": 整数,
"bedActualTemp": 整数,
"mainFanSpeed": 整数,
"boardFanSpeed": 整数,
"auxFanSpeed": 整数,
"ts": "时间戳"
}
```

字段说明：
1、加热状态：
a\. preheatType\-0无/1PLA/2TPU/3ABS；
b\. preheatState\-0停止/1启动
c\. heatState\-加热标志\(0停止/1加热\)；
d\. nozzleTargetTemp\-喷嘴目标\(16位\)
e\. nozzleActualTemp\-实际温度\(16位\)；
f\. bedTargetTemp\-热床目标\(8位\) ；
g\. bedActualTemp\-实际温度\(8位\)；
2、风扇转速：0\-100%（8位）
a\. mainFanSpeed\-主风扇；
b\. boardFanSpeed\-主板风扇；
c\. auxFanSpeed\-辅助风扇；

- 示例：

```JSON
{
"type": "status_temp_fan",
"id": "G1afe3598d37b",
"preheatType": 1,
"preheatState": 0,
"heatState": 1,
"nozzleTargetTemp": 200,
"nozzleActualTemp": 198,
"bedTargetTemp": 60,
"bedActualTemp": 58,
"mainFanSpeed": 50,
"auxFanSpeed": 30,
"ts": 12467890765
}
```

##### 调平状态包

- 主题：GT/MXS/UP/\{deviceId\}

- 负载JSON格式：

```JSON
{
"type": "status_level",
"id": "G{蓝牙MAC地址}",
"levelingStatus": 0-3,
"levelingPoint": 整数,
"levelingTotalPoints": 整数,
"ts": "时间戳"
}
```

字段说明：
a\. levelingStatus\-0未调平/1调平中/2完成/3失败；
b\. levelingPoint\-调平总点数；
c\. levelingTotalPoints\-当前调平点；

- 示例：

```JSON
{
"type": "status_level",
"id": "G1afe3598d37b",
"levelingStatus": 2,
"levelingPoint": 9,
"levelingTotalPoints": 16,
"ts": 12467890765
}
```

##### 打印状态包

- 主题：GT/MXS/UP/\{deviceId\}

- 负载JSON格式：

```JSON
{
"type": "status_print",
"id": "G{蓝牙MAC地址}",
"printState": 0-7,
"zOffset": 带符合整数（Zoffset*100）,
"printProgress": 0-100,
"printElapsedTime": {
    "hour": 0-255,
    "min": 0-59,
    "sec": 0-59
},
"printRemainTime": {
    "hour": 0-255,
    "min": 0-59,
    "sec": 0-59
},
"currentPrintFile": "文件名",
"ts": "时间戳"
}
```

字段说明：
1、打印状态：
a\. zOffset\-Z轴偏移量，带符合整数（Zoffset\*100）
b\. printState\-0空闲/1打印/2完成/3暂停/4失败/5取消/6断电续打/7换料
2、打印信息：
a\. printProgress\-进度\(%\)；
b\. currentPrintFile\-打印文件名，无则为空字符串;

- 示例：

```JSON
{
"type": "status_print",
"id": "G1afe3598d37b",
"printState": 1,
"zOffset": -125,
"printProgress": 45,
"printElapsedTime": {
    "hour": 0,
    "min": 23,
    "sec": 15
},
"printRemainTime": {
    "hour": 0,
    "min": 27,
    "sec": 45
},
"currentPrintFile": "cube.gco",
"ts": 12467890765
}
```

#### 服务器→设备（查询指令）

- 主题：GT/MXS/DOWN/\{deviceId\}

- 负载JSON格式：

```JSON
{
"type": "status_query",
"id": "G{蓝牙MAC地址}",
"ts": "时间戳"
}
```

- 实例：

```JSON
{
"type": "status_query",
"id": "G1afe3598d37b",
"ts": 12467890765
}
```

#### 设备→服务器（查询回复）

与设备主动发布的格式完全一致（主题、负载、QoS不变），接收到查询指令后立即发送。

### 5\.4 GCODE指令包\(gcode\)

#### 核心说明

- GCODE指令由服务器下发，设备执行后必须回复执行结果；

- 指令格式遵循Marlin Gcode标准：Gnnn/Mnnn/Tnnn \+参数\+ ；

- 执行结果无数据回传则返回OK，有数据则返回具体内容（如文件删除结果）。

#### 服务器→设备（下发指令）

- 主题：GT/MXS/DOWN/\{deviceId\}

- 负载JSON格式：

```JSON
{
"type": "gcode",
"id": "G{蓝牙MAC地址}",
"gcodeCmd": "GCODE指令字符串",
"ts": "时间戳"
}
```

- 示例：

```JSON
{
"type": "gcode",
"id": "G1afe3598d37b",
"gcodeCmd": "G29N",
"ts":1234567890
}
```

#### 设备→服务器（执行回复）

- 主题：GT/MXS/UP/\{deviceId\}

- 负载JSON格式：

```JSON
{
"type": "gcode",
"id": "G{蓝牙MAC地址}",
"cmdType": "gcode命令类型",
"execResult": "执行结果字符串",
"ts": "时间戳"
}
```

- 示例1（执行成功）：

```JSON
{
"type": "gcode",
"id": "G1afe3598d37b",
"cmdType": "G29N",
"execResult": "OK",
"ts":1234567890
}
```

- 示例2（执行失败）：

```JSON
{
"type": "gcode",
"id": "G1afe3598d37b",
"cmdType": "G29N",
"execResult": "ERR",
"ts":1234567890
}
```

#### 常用APP相关GCODE指令映射

|指令功能|APP下发格式|设备回复内容|
|---|---|---|
|自动调平|G29N|OK\+状态包/错误包|
|删除SD卡文件|M30 filename|File deleted/Deletion failed|
|选定SD卡文件并开始打印|M32 filename|File selected OK/File open failed|
|开始/恢复打印|M24|OK|
|暂停打印|M25|OK|
|终止打印|M524|OK|
|材料预热|M145 S（0\-停止，1\-PLA，2\-TPU，3\-ABS）|OK|
|喷嘴温度设置|M104 S（0为停止加热）|OK|
|热床温度设置|M140 S（0为停止加热）|OK|
|风扇设置|M106 P S（P：1散热/2主板/3辅助）|OK|
|呼吸灯开关|M150 S（0关/1开）|OK|
|LED照明灯开关|M355 S（0关/1开）|OK|
|蜂鸣器开关|M230 S（0关/1开）|OK|
|设置Z\-Offset|M851 Z|OK|
|急停|M112|OK|
|固件升级|M997|OK|
|上电续打|M2000 S（0取消/1开启）|OK|
|APP在线/离线|M2001 S（0离线/1在线）|OK|

### 5\.5 文件下载起始包\(download/begin\)

#### 核心说明

- 文件下载由服务器发起，分3阶段：起始（MQTT）、数据传输（HTTP）、结束（MQTT）；

- MQTT仅负责起始指令交互，设备校验通过则启动HTTP下载，失败则终止流程；

- 支持文件类型：0\-GCODE文件、1\-MCU固件、2\-ESP固件。

#### 服务器→设备（下发起始指令）

- 主题：GT/MXS/DOWN/\{deviceId\}

- 负载JSON格式：

```JSON
{
"type": "download_begin",
"id": "G{蓝牙MAC地址}",
"fileType": 0-2,
"serverIp": "服务器IP地址",
"fileName": "文件名",
"url": "文件完整下载URL（V2.2.0新增，可选）",
"ts": "时间戳"
}
```

字段说明：
- fileType：0-GCODE / 1-MCU固件 / 2-ESP固件
- serverIp：切片服务器IP:端口，url 存在时可为空
- fileName：GCODE文件名
- **url（V2.2.0新增，可选）**：云端文件完整下载URL（如 `http://getech-app-cn.oss-cn-shenzhen.aliyuncs.com/uploads/models/gcode/abc123.gcode`）。设备优先使用此字段直接从 COS 下载；字段不存在或为空时回退到旧方式（`http://{serverIp}/admin-api/remote/file/{fileName}`）

- 示例（云端下载）：

```JSON
{
"type": "download_begin",
"id": "G1afe3598d37b",
"fileType": 0,
"serverIp": "",
"fileName": "myprint.gco",
"url": "http://getech-app-cn.oss-cn-shenzhen.aliyuncs.com/uploads/models/gcode/abc123.gcode",
"ts":1234567890
}
```

- 示例（旧版兼容，无 url 字段）：

```JSON
{
"type": "download_begin",
"id": "G1afe3598d37b",
"fileType": 0,
"serverIp": "192.168.2.103",
"fileName": "myprint.gco",
"ts":1234567890
}
```

#### 设备→服务器（校验回复）

- 主题：GT/MXS/UP/\{deviceId\}

- 负载JSON格式：

```JSON
{
"type": "download_begin",
"id": "G{蓝牙MAC地址}",
"fileName": "文件名",
"fileType": "文件类型",
"transState": "状态字符串",
"errCode": 0/1/2,
"ts": "时间戳"
}
```

字段说明：
transState\(OK /ERROR\)；errCode（0\-成功/1\-无SD卡/2\-打印机忙）
fileType（0\-GCODE/1\-MCU固件/2\-ESP固件）

- 示例：

```JSON
{
"type": "download_begin",
"id": "G1afe3598d37b",
"fileName": "myprint.gco",
"fileType": 0,
"transState": "OK",
"errCode": 0,
"ts":1234567890
}
```

### 5\.6 文件下载结束回复包\(download/end\)

#### 核心说明

- HTTP数据传输完成后，设备发布结束包反馈状态，服务器接收后确认；

- 固件升级/文件打印需在下载完成后，通过MQTT下发GCODE指令（M997/ M32\+M24）执行。

#### 设备→服务器（反馈结束状态）

- 主题：GT/MXS/UP/\{deviceId\}

- 负载JSON格式：

```JSON
{
"type": "download_end",
"id": "G{蓝牙MAC地址}",
"fileName": "文件名",
"fileType": "文件类型",
"transState": "状态字符串",
"errCode": 0/1/2,
"ts": "时间戳"
}
```

字段说明：
transState\(OK /ERROR\)；errCode（0\-成功/1\-传输超时/2\-SD卡弹出）
fileType（0\-GCODE/1\-MCU固件/2\-ESP固件）

- 示例：

```JSON
{
"type": "download_end",
"id": "G1afe3598d37b",
"fileName": "myprint.gco",
"fileType": 0,
"transState": "ERROR",
"errCode": 1,
"ts":1234567890
}
```

#### 服务器→设备（确认接收）

- 主题：GT/MXS/DOWN/\{deviceId\}

- 负载JSON格式：

```JSON
{
"type": "download_end",
"id": "G{蓝牙MAC地址}",
"ackState": "确认字符串",
"ts": "时间戳"
}
```

ackState说明：OK /ERROR

- 示例：

```JSON
{
"type": "download_end",
"id": "G1afe3598d37b",
"ackState": "OK",
"ts":1234567890
}
```

### 5\.7 固件查询/升级指令包\(upgrade/query\)

#### 核心说明

- 固件升级两种方式：服务器主动发起/设备主动查询，均为HTTP下载\+MQTT指令升级；

- 服务器根据设备登录包的sf1（MCU版本）、sf2（ESP版本）判断是否有新版本；

- 固件命名规则：MCU固件MXS\_VX\.XX\_MCU103\.bin、ESP固件esp\-at\_vx\.x\.x\.x\.bin。

#### 设备→服务器（发起查询）

- 主题：GT/MXS/UP/\{deviceId\}

- 负载JSON格式：

```JSON
{
"type": "upgrade_query",
"id": "G{蓝牙MAC地址}",
"ts": "时间戳"
}
```

- 示例：

```JSON
{
"type": "upgrade_query",
"id": "G1afe3598d37b",
"ts":1234567890
}
```

#### 服务器→设备（查询回复）

- 主题：GT/MXS/DOWN/\{deviceId\}

- 负载JSON格式：

```JSON
{
"type": "upgrade_query",
"id": "G{蓝牙MAC地址}",
"serverIp": "服务器IP地址",
"mcuFile": "MCU固件名",
"espFile": "ESP固件名",
"ts": "时间戳"
}
```

字段说明：serverIp\(无则为空\)；mcuFile/espFile\(无则为NA\)

- 示例（仅更新MCU）：

```JSON
{
"type": "upgrade_query",
"id": "G1afe3598d37b",
"serverIp": "192.168.2.103",
"mcuFile": "MXS_V1.08_MCU103.bin",
"espFile": "NA",
"ts":1234567890
}
```

### 5\.8 SD卡文件列表查询\(file/list\)

#### 服务器→设备（发起查询）

- 主题：GT/MXS/DOWN/\{deviceId\}

- 负载JSON格式：

```JSON
{
"type": "file_list",
"id": "G{蓝牙MAC地址}",
"ts": "时间戳"
}
```

- 示例:

```JSON
{
"type": "file_list",
"id": "G1afe3598d37b",
"ts":1234567890
}
```

#### 设备→服务器（查询回复）

- 主题：GT/MXS/UP/\{deviceId\}

- 负载JSON格式：

```JSON
{
"type": "file_list",
"id": "G{蓝牙MAC地址}",
"fileTotal": 整数,
"fileIndex": 整数,
"fileList": [
    {
        "fileName": "文件名1.gco",
        "fileSize": 整数
    },
    {
        "fileName": "文件名2.gco",
        "fileSize": 整数
    }
],
"ts": "时间戳"
}
```

字段说明：fileTotal\(JSON总包数\)；fileIndex\(JSON包索引\)；fileSize\(文件大小，字节\)；无文件则返回空数组

- 实例1（SD卡有文件）：

```JSON
{
"type": "file_list",
"id": "G1afe3598d37b",
"fileTotal": 10,
"fileIndex": 3,
"fileList": [
    {
        "fileName": "cube.gco",
        "fileSize": 125800
    },
    {
        "fileName": "cat.gco",
        "fileSize": 286540
    },
    {
        "fileName": "test_model.gco",
        "fileSize": 98760
    }
],
"ts":1234567890
}
```

- 实例2（SD卡无文件）：

```JSON
{
"type": "file_list",
"id": "G1afe3598d37b",
"fileTotal": 0,
"fileIndex": 0,
"fileList": [],
"ts":1234567890
}
```

#### 核心说明

- 设备接收到查询指令后，立即扫描SD卡内所有后缀为\*\*\.gco\*\*的GCODE文件，忽略其他格式文件；

- 文件大小按字节统计，保留整数位，不进行单位换算；

- 若SD卡未插入（对应设备状态包中sd=0），回复时fileTotal=0、fileIndex=0、fileList为空数组，同时可在后续状态包中上报SD卡不存在错误（errType=11）。

### 5\.9 服务器解绑设备包\(server/unbind\)

#### 核心说明

- 解绑由服务器发起，用于手机APP与设备的关联管理。

#### 服务器→设备（解绑指令）

- 主题：GT/MXS/DOWN/\{deviceId\}

- 负载JSON格式：

```JSON
{
"type": "server_unbind",
"id": "G{蓝牙MAC地址}",
"ts": "时间戳"
}
```

- 实例：

```JSON
{
"type": "server_unbind",
"id": "G1afe3598d37b",
"ts":1234567890
}
```

### 5\.10 设备端发起解绑包（device/unbind）

#### 设备→服务器（设备发起解绑）

- 主题：GT/MXS/UP/\{deviceId\}

- 负载JSON格式：

```JSON
{
"type": "device_unbind",
"id": "G{蓝牙MAC地址}",
"ts": "时间戳"
}
```

- 实例：

```JSON
{
"type": "device_unbind",
"id": "G1afe3598d37b",
"ts":1234567890
}
```

#### 服务器→设备（服务器回复解绑）

- 主题：GT/MXS/DOWN/\{deviceId\}

- 负载JSON格式：

```JSON
{
"type": "device_unbind",
"id": "G{蓝牙MAC地址}",
"ts": "时间戳"
}
```

- 实例：

```JSON
{
"type": "device_unbind",
"id": "G1afe3598d37b",
"ts":1234567890
}
```

## 六、附件

（暂无内容，预留扩展）

我可以帮你把这份协议整理成**按功能分类的速查表格**，方便开发和调试时快速查阅，需要吗？

> （注：文档部分内容可能由 AI 生成）
