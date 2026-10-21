#!/bin/bash
# build.sh — 交叉编译 mqttclient 到 RV1106 (Luckfox Pico, Rockchip 830)
#
# 用法:
#   ./scripts/build.sh                 # 交叉编译 debug 版本
#   ./scripts/build.sh release         # 交叉编译 release 版本 (--release)
#   ./scripts/build.sh deploy          # debug 编译 + scp 到板子
#   ./scripts/build.sh release deploy  # release 编译 + scp 到板子
#   ./scripts/build.sh release install # release 编译 + 装进 buildroot overlay
#
# 环境变量:
#   RV1106_HOST       — RV1106 的 IP (deploy 用, 默认 192.168.0.30)
#   RV1106_PASS       — 板端 root 密码 (deploy 用；设置后经 expect 自动应答，
#                       不设置则 scp/ssh 交互式提示输入。密码不写入本脚本)
#   RV1106_TOOLCHAIN  — Rockchip uclibc 工具链 bin 目录 (默认见下)
#   DEPLOY_PATH       — 板子上部署路径 (默认 /opt/mqttclient/mqttclient)
#   CARGO_TARGET_DIR  — 覆盖编译输出目录 (默认 workspace/target)
#   SDK_ROOT          — RV1106 SDK 根 (install 用, 默认见下)
#   INSTALL_DIR       — install 目标目录 (默认 SDK overlay/opt/mqttclient)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# ---- 解析参数 ----
RELEASE=false
DEPLOY=false
INSTALL=false
for arg in "$@"; do
    case "$arg" in
        release) RELEASE=true ;;
        deploy)  DEPLOY=true ;;
        install) INSTALL=true ;;
        *) echo "[ERROR] Unknown arg: $arg"; exit 1 ;;
    esac
done

# ---- 目标与工具链 ----
# RV1106 (Luckfox Pico) 官方 uclibc 1.0.31 工具链。Rust 无 uclibceabihf 标准 target，
# 采用 armv7-unknown-linux-gnueabihf + uclibc gcc 作 linker/sysroot。
# 必须用 uclibc 工具链：板子 rootfs 为 uclibc，gnu(glibc) 产物依赖 libc.so.6 /
# ld-linux-armhf.so.3 无法运行。uclibc 缺失的 getauxval 与 posix_spawn* 辅助函数
# 由 cross/ 下桩库补齐（libgetauxval.a / libposix_spawn.a，经 crates/mqttclient/build.rs 链接）。
# 注意：桩库必须用 rustc-link-arg（追加在链接命令末尾，libstd/-lc 之后）而非
# rustc-link-lib(static=)：静态库按命令行顺序解析，早于引用它的 libstd 出现则不会
# 拉入成员，getauxval/posix_spawn* 仍为 undefined reference。
#
# 蓝牙（bluer → dbus-rs）：uclibc sysroot 内没有 libdbus，因此对交叉目标启用
# dbus 的 `vendored` feature（见 crates/mqttclient/Cargo.toml 的
# [target.armv7-unknown-linux-gnueabihf.dependencies]），由 libdbus-sys 从自带
# 源码静态构建 libdbus。需要 CC_armv7_unknown_linux_gnueabihf（下面已导出）指向
# 交叉 gcc，产物不依赖板子上的 libdbus-1.so。
TARGET="armv7-unknown-linux-gnueabihf"
GCC_NAME="arm-rockchip830-linux-uclibcgnueabihf-gcc"
TOOLCHAIN_DIR="${RV1106_TOOLCHAIN:-/opt/toolchain/arm-rockchip830-linux-uclibcgnueabihf}"

BIN_NAME="mqttclient"
OUT_DIR="$PROJECT_ROOT/target/$TARGET"
BIN_PATH="$OUT_DIR/$( [ "$RELEASE" = true ] && echo release || echo debug )/$BIN_NAME"

# SDK 根：默认 RV1106 IPC SDK（仅 install 用；可用 SDK_ROOT= 覆盖）
SDK_ROOT="${SDK_ROOT:-/home/song/samba/work/rv1106/rk-sdk/RV1106_IPC_SDK}"
# 安装目录 = buildroot overlay 源目录（随 rootfs 一起打包进镜像）
# 板端启动脚本 /opt/S25mqttclient 在 /opt/mqttclient 下运行 ./mqttclient。
OVERLAY_DIR="${INSTALL_DIR:-$SDK_ROOT/project/cfg/BoardConfig_IPC/overlay/overlay-gt-buildroot-app/opt/mqttclient}"

# ---- 板端访问（deploy 用）----
# 设置了 RV1106_PASS 时用 expect 自动应答密码（密码经环境变量交给 Tcl，
# 不写进脚本正文）；未设置则退回 scp/ssh 原生交互输入。
export RV1106_HOST="${RV1106_HOST:-192.168.0.30}"
export RV1106_PASS="${RV1106_PASS:-}"
RV_SSH_OPTS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null)

_need_expect() {
    command -v expect >/dev/null 2>&1 || {
        echo "[ERROR] 已设置 RV1106_PASS，但未安装 expect" >&2
        return 1
    }
}

# 上传文件：$1=本地路径 $2=板端路径
_rv_scp() {
    if [ -n "$RV1106_PASS" ]; then
        _need_expect || return 1
        RV_SRC="$1" RV_DST="$2" expect -c '
            set timeout 300
            spawn scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null $env(RV_SRC) root@$env(RV1106_HOST):$env(RV_DST)
            expect {
                -re {(?i)password:} { send "$env(RV1106_PASS)\r"; exp_continue }
                eof
            }
            catch wait result
            exit [lindex $result 3]
        '
    else
        scp "${RV_SSH_OPTS[@]}" "$1" "root@$RV1106_HOST:$2"
    fi
}

# 板端执行命令：$1=远端 shell 命令
_rv_ssh() {
    if [ -n "$RV1106_PASS" ]; then
        _need_expect || return 1
        RV_CMD="$1" expect -c '
            set timeout 120
            spawn ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null root@$env(RV1106_HOST) $env(RV_CMD)
            expect {
                -re {(?i)password:} { send "$env(RV1106_PASS)\r"; exp_continue }
                eof
            }
            catch wait result
            exit [lindex $result 3]
        '
    else
        ssh "${RV_SSH_OPTS[@]}" "root@$RV1106_HOST" "$1"
    fi
}

cd "$PROJECT_ROOT"

# ---- 检查 rustup target ----
echo "[1/4] Checking toolchain..."
if ! rustup target list --installed 2>/dev/null | grep -q "$TARGET"; then
    echo "[ERROR] Rust target '$TARGET' not installed."
    echo "  Run: rustup target add $TARGET"
    exit 1
fi

# 优先将工具链 bin 加入 PATH (cargo build 的 linker 需要)
if [ -n "${TOOLCHAIN_DIR:-}" ] && [ -d "$TOOLCHAIN_DIR/bin" ]; then
    export PATH="$TOOLCHAIN_DIR/bin:$PATH"
fi
# 传给 crates/mqttclient/build.rs：sysroot 链接参数由此变量派生（与本次构建用同一工具链）
export RV1106_TOOLCHAIN="$TOOLCHAIN_DIR"

GCC_PATH=$(which "$GCC_NAME" 2>/dev/null || echo "")
if [ -z "$GCC_PATH" ]; then
    echo "[ERROR] Cross compiler '$GCC_NAME' not found."
    echo "  Tried: $TOOLCHAIN_DIR/bin/$GCC_NAME"
    echo "  Set RV1106_TOOLCHAIN to the Rockchip toolchain bin dir"
    exit 1
fi

echo "      target:  $TARGET"
echo "      linker:  $GCC_PATH"
echo "      mode:    $( [ "$RELEASE" = true ] && echo release || echo debug )"

# ---- 设置 linker 环境变量 ----
# 使用 CARGO_TARGET_*_LINKER 而非 .cargo/config.toml 的 [env] CC，
# 避免污染 host 构建 (ring/cc-rs 在 host 编译时会误用交叉 gcc)。
TARGET_UNDERSCORE=$(echo "$TARGET" | tr '-' '_')
export CARGO_TARGET_${TARGET_UNDERSCORE^^}_LINKER="$GCC_NAME"
# ring / cc-rs 等 build script 需要交叉 CC（与 rust-libp2p 的 build_device_cam.sh 一致）
export CC_${TARGET_UNDERSCORE}="$GCC_NAME"
export CFLAGS_${TARGET_UNDERSCORE}="-fPIC"

# ---- 编译 ----
echo ""
echo "[2/4] Building mqttclient for RV1106..."
BUILD_ARGS=(--target "$TARGET")
if [ "$RELEASE" = true ]; then
    BUILD_ARGS+=(--release)
fi
cargo build "${BUILD_ARGS[@]}"

# 同时编译 workspace 的库单元测试 (host 侧, 用于 sanity check, 可选)
# 注: 交叉测试需要在板子上运行, 这里仅确保 host 单测通过 (非交叉)
echo ""
echo "[3/4] Build complete!"

# ---- 验证产物 ----
echo ""
echo "============================================"
echo "  binary:  $BIN_PATH"
echo "  target:  $TARGET"
echo "  mode:    $( [ "$RELEASE" = true ] && echo release || echo debug )"
echo "============================================"
file "$BIN_PATH" 2>/dev/null || true
echo ""
ls -lh "$BIN_PATH"

# ---- deploy 模式 ----
if [ "$DEPLOY" = true ]; then
    DEPLOY_PATH="${DEPLOY_PATH:-/opt/mqttclient/mqttclient}"
    echo ""
    echo "[4/4][Deploy] Copying to RV1106 ($RV1106_HOST:$DEPLOY_PATH)..."
    # 运行中的二进制不可直接覆盖（ETXTBSY）：先传 .new，再原子 mv 落位
    _rv_scp "$BIN_PATH" "$DEPLOY_PATH.new"
    _rv_ssh "mv -f '$DEPLOY_PATH.new' '$DEPLOY_PATH' && chmod 755 '$DEPLOY_PATH' && md5sum '$DEPLOY_PATH'"
    echo "[Deploy] Done. 重启客户端使其生效:"
    echo "  ssh root@$RV1106_HOST 'kill \$(pidof mqttclient); cd /opt/mqttclient && setsid nohup ./mqttclient >> /root/printer_data/logs/mqttclient.log 2>&1 &'"
fi

# ---- install 模式：装进 buildroot overlay（随 rootfs 打包进镜像）----
if [ "$INSTALL" = true ]; then
    OVERLAY_PARENT="$(dirname "$OVERLAY_DIR")"
    if [ ! -d "$OVERLAY_PARENT" ]; then
        echo "[ERROR] overlay 父目录不存在: $OVERLAY_PARENT"
        echo "  请确认 SDK 结构（或设置 INSTALL_DIR）"
        exit 1
    fi
    echo ""
    echo "[4/4][Install] Copying to overlay ($OVERLAY_DIR)..."
    mkdir -p "$OVERLAY_DIR"
    cp "$BIN_PATH" "$OVERLAY_DIR/$BIN_NAME"
    chmod 755 "$OVERLAY_DIR/$BIN_NAME"
    echo "[Install] Done."
    ls -lh "$OVERLAY_DIR/$BIN_NAME"
    echo
    echo "  下一步: 重新打包 rootfs，烧录后板端即为 /opt/mqttclient/mqttclient"
    echo "  注: 设备专属配置 config/mqttclient.toml 不由本脚本覆盖（由出厂/配网流程生成）"
fi
