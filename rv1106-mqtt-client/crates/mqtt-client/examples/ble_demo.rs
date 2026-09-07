//! 最小 BLE GATT 服务端 demo（基于 `bluer`），用于**验证板子 ↔ 手机 APP 的双向蓝牙通信**。
//!
//! 等价于 zbus 手写 `GattApplication1` / `ObjectManager` / `GattManager1.RegisterApplication`
//! / `LEAdvertisingManager1.RegisterAdvertisement` 的那一套，只是这些样板全部由 `bluer` 代劳，
//! 我们只写回调：
//! - 服务 `0xFFB0`，特征 `0xFFB1`（write / write-without-response，可 read 回显）；
//! - 特征 `0xFFB2`（notify，每秒推一包计数，用于验证设备 → APP 方向）；
//! - **默认不注册广播**（板子系统侧已在广播，手机 APP 能直接发现并连接），
//!   只注册 GATT 应用；需要 demo 自己广播时加 `--adv` 或 `BLE_ADV=1`，
//!   广播名默认 `ble-demo`，可用命令行第一个参数覆盖。
//!
//! 手机端用任意 BLE 调试 APP（nRF Connect / LightBlue 等）即可：
//!   1. 扫描到板子（默认由系统广播发现），连接；
//!   2. 往 `0xFFB1` 写几个字节 → 板子串口打印 hex；再读 `0xFFB1` 应回显同样内容；
//!   3. 订阅 `0xFFB2` 的 Notify → 每秒收到一包递增数据。
//!
//! 运行（板子上）：
//! ```sh
//! RUST_LOG=info ./ble_demo [设备名]
//! ```
//!
//! 交叉编译（在 rv1106-mqtt-client 目录下）：
//! ```sh
//! export PATH=/opt/toolchain/arm-rockchip830-linux-uclibcgnueabihf/bin:$PATH
//! cargo build --example ble_demo --target armv7-unknown-linux-gnueabihf
//! scp target/armv7-unknown-linux-gnueabihf/debug/examples/ble_demo root@<板子IP>:/tmp/
//! ```

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use bluer::adv::{Advertisement, Type as AdvType};
use bluer::gatt::local::{
    Application, Characteristic, CharacteristicNotify, CharacteristicNotifyMethod, CharacteristicRead,
    CharacteristicWrite, CharacteristicWriteMethod, Service,
};
use bluer::{Session, Uuid};
use futures::FutureExt;
use tokio::sync::Mutex;

// -------------------------- UUID --------------------------
/// 默认（通用调试）UUID：服务 `0xFFB0`、写特征 `0xFFB1`、通知特征 `0xFFB2`。
const SERVICE_UUID: Uuid = Uuid::from_u128(0x0000ffb0_0000_1000_8000_00805f9b34fb);
const CHAR_WRITE_UUID: Uuid = Uuid::from_u128(0x0000ffb1_0000_1000_8000_00805f9b34fb);
const CHAR_NOTIFY_UUID: Uuid = Uuid::from_u128(0x0000ffb2_0000_1000_8000_00805f9b34fb);

/// BluFi（M2 APP 实际使用的）UUID：服务 `0xFFFF`、写特征 `0xFF01`、通知特征 `0xFF02`。
/// 用 `--blufi` 切换：M2 APP 只会往 `0xFFFF/0xFF01` 写数据，用默认 UUID 时
/// 手机连上后做完服务发现发现没有目标服务，就不会下发任何数据（表现为「收不到」）。
const BLUFI_SERVICE_UUID: Uuid = Uuid::from_u128(0x0000ffff_0000_1000_8000_00805f9b34fb);
const BLUFI_CHAR_WRITE_UUID: Uuid = Uuid::from_u128(0x0000ff01_0000_1000_8000_00805f9b34fb);
const BLUFI_CHAR_NOTIFY_UUID: Uuid = Uuid::from_u128(0x0000ff02_0000_1000_8000_00805f9b34fb);

#[tokio::main(flavor = "current_thread")]
async fn main() -> bluer::Result<()> {
    env_logger::init();

    // ---- 命令行参数解析 ----
    //   --blufi      切到 BluFi UUID 0xFFFF / 0xFF01 / 0xFF02（M2 APP 用）
    //   --adv        由 demo 自己注册广播；--skip-adv 关闭（默认走系统侧广播）
    //   --name <名>  广播名（默认 ble-demo）；不带 -- 的位置参数也当广播名
    //   未知 --xxx 参数直接报错退出，避免像 "--M1Sd1234" 被静默当成广播名
    let mut name = "ble-demo".to_string();
    let mut blufi = false;
    let mut adv_enabled = matches!(
        std::env::var("BLE_ADV").as_deref(),
        Ok(v) if !v.is_empty() && v != "0"
    );
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--blufi" => blufi = true,
            "--adv" => adv_enabled = true,
            "--skip-adv" => adv_enabled = false,
            "--name" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("错误: --name 需要一个参数");
                    std::process::exit(2);
                }
                name = argv[i].clone();
            }
            s if s.starts_with("--") => {
                eprintln!("错误: 未知参数 `{s}`（可用: --blufi --adv --skip-adv --name <名字>）");
                std::process::exit(2);
            }
            s => name = s.to_string(), // 位置参数当作广播名
        }
        i += 1;
    }
    let skip_adv = !adv_enabled;

    // `--blufi`：换成 BluFi 的 0xFFFF / 0xFF01 / 0xFF02，直接用 M2 APP 验证。
    let (service_uuid, write_uuid, notify_uuid) = if blufi {
        (BLUFI_SERVICE_UUID, BLUFI_CHAR_WRITE_UUID, BLUFI_CHAR_NOTIFY_UUID)
    } else {
        (SERVICE_UUID, CHAR_WRITE_UUID, CHAR_NOTIFY_UUID)
    };
    println!(
        "使用服务 0x{:04X} / 写特征 0x{:04X} / 通知特征 0x{:04X}{}",
        (service_uuid.as_u128() >> 96) as u16,
        (write_uuid.as_u128() >> 96) as u16,
        (notify_uuid.as_u128() >> 96) as u16,
        if blufi { " (BluFi)" } else { " (demo 默认)" }
    );

    // 连接 system bus 上的 org.bluez（等价于示例里的 Connection::system()）
    let session = Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;
    println!(
        "adapter {} addr {} — 广播名 {name}",
        adapter.name(),
        adapter.address().await?
    );

    // -------------------------- 注册广播（让手机能扫到） --------------------------
    // 句柄必须一直持有：drop 即注销广播
    let _adv_handle = if skip_adv {
        println!("不注册广播（默认），由系统侧负责广播；需要 demo 自己广播时加 --adv");
        None
    } else {
        let le_advertisement = Advertisement {
            advertisement_type: AdvType::Peripheral,
            service_uuids: BTreeSet::from([service_uuid]),
            discoverable: Some(true),
            local_name: Some(name),
            ..Default::default()
        };
        // 注册失败不致命：系统侧若已在广播，GATT 应用照样可以注册并正常通信
        match adapter.advertise(le_advertisement).await {
            Ok(h) => {
                println!("LE 广播已开启");
                Some(h)
            }
            Err(e) => {
                println!("注册广播失败（系统侧可能已在广播）：{e} — 继续注册 GATT 应用");
                None
            }
        }
    };

    // 业务共享 buffer：write_value 回调写入，业务侧可读取（这里只打印）
    let shared_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let read_buf = shared_buf.clone();

    // -------------------------- 注册 GATT Application --------------------------
    let app = Application {
        services: vec![Service {
            uuid: service_uuid,
            primary: true,
            characteristics: vec![
                // APP 下发：0xFFB1（write / write-without-response + read 回显）
                Characteristic {
                    uuid: write_uuid,
                    write: Some(CharacteristicWrite {
                        write: true,
                        write_without_response: true,
                        // bluetoothd 收到手机写请求时会调用这个回调
                        method: CharacteristicWriteMethod::Fun(Box::new(move |value, req| {
                            let buf = shared_buf.clone();
                            async move {
                                println!(
                                    "===== 收到APP下发数据 len={} from {} (mtu {}) =====",
                                    value.len(),
                                    req.device_address,
                                    req.mtu
                                );
                                println!("data hex: {:02x?}", value);
                                // 存入业务缓冲区（真实项目里这里换成业务线程的 channel）
                                *buf.lock().await = value;
                                Ok(())
                            }
                            .boxed()
                        })),
                        ..Default::default()
                    }),
                    read: Some(CharacteristicRead {
                        read: true,
                        fun: Box::new(move |_req| {
                            let buf = read_buf.clone();
                            // 读回最后一次写入的内容，方便手机端对照验证
                            async move { Ok(buf.lock().await.clone()) }.boxed()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                // 设备上报：0xFFB2（notify，每秒一包）
                Characteristic {
                    uuid: notify_uuid,
                    notify: Some(CharacteristicNotify {
                        notify: true,
                        // 手机使能 Notify 时进入这里，会话期间可以持续推送
                        method: CharacteristicNotifyMethod::Fun(Box::new(move |mut notifier| {
                            async move {
                                tokio::spawn(async move {
                                    println!(
                                        "APP 已订阅通知 (confirming={})，每秒推一包",
                                        notifier.confirming()
                                    );
                                    let mut i: u8 = 0;
                                    loop {
                                        i = i.wrapping_add(1);
                                        let data = vec![i; 4];
                                        match notifier.notify(data).await {
                                            Ok(()) => println!("notify -> {:02x?}", vec![i; 4]),
                                            Err(e) => {
                                                println!("notify error: {e}");
                                                break;
                                            }
                                        }
                                        tokio::time::sleep(Duration::from_secs(1)).await;
                                    }
                                    println!("通知会话结束");
                                });
                            }
                            .boxed()
                        })),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }],
        ..Default::default()
    };
    // 句柄必须一直持有：drop 即注销 GATT 服务
    let _app_handle = adapter.serve_gatt_application(app).await?;
    println!("GATT Application 注册成功，等待手机连接/写入...");

    // 阻塞运行 D-Bus 事件循环，等待 bluetoothd 回调（句柄存活即保持注册）
    std::future::pending::<()>().await;
    Ok(())
}
