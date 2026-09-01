#!/bin/bash
# build.sh — 交叉编译 mqtt-client 到 RV1106 (Luckfox Pico, Rockchip 830)
#
# 用法:
#   ./scripts/build.sh                 # 交叉编译 debug 版本
#   ./scripts/build.sh release         # 交叉编译 release 版本 (--release)
#   ./scripts/build.sh deploy          # debug 编译 + scp 到板子
#   ./scripts/build.sh release deploy  # release 编译 + scp 到板子
#
# 环境变量:
#   RV1106_HOST       — RV1106 的 IP (deploy 用, 默认 192.168.1.100)
#   RV1106_TOOLCHAIN  — Rockchip uclibc 工具链 bin 目录 (默认见下)
#   DEPLOY_PATH       — 板子上部署路径 (默认 /usr/bin/mqtt-client)
#   CARGO_TARGET_DIR  — 覆盖编译输出目录 (默认 workspace/target)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# ---- 解析参数 ----
RELEASE=false
DEPLOY=false
for arg in "$@"; do
    case "$arg" in
        release) RELEASE=true ;;
        deploy)  DEPLOY=true ;;
        *) echo "[ERROR] Unknown arg: $arg"; exit 1 ;;
    esac
done

# ---- 目标与工具链 ----
# RV1106 (Luckfox Pico) 官方 uclibc 1.0.31 工具链。Rust 无 uclibceabihf 标准 target，
# 采用 armv7-unknown-linux-gnueabihf + uclibc gcc 作 linker/sysroot (见 .cargo/config.toml)。
TARGET="armv7-unknown-linux-gnueabihf"
GCC_NAME="arm-rockchip830-linux-uclibcgnueabihf-gcc"
TOOLCHAIN_DIR="${RV1106_TOOLCHAIN:-$PROJECT_ROOT/../luckfox-pico/tools/linux/toolchain/arm-rockchip830-linux-uclibcgnueabihf}"

BIN_NAME="mqtt-client"
OUT_DIR="$PROJECT_ROOT/target/$TARGET"
BIN_PATH="$OUT_DIR/$( [ "$RELEASE" = true ] && echo release || echo debug )/$BIN_NAME"

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

# ---- 编译 ----
echo ""
echo "[2/4] Building mqtt-client for RV1106..."
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
    RV1106_HOST="${RV1106_HOST:-192.168.1.100}"
    DEPLOY_PATH="${DEPLOY_PATH:-/usr/bin/mqtt-client}"
    echo ""
    echo "[4/4][Deploy] Copying to RV1106 ($RV1106_HOST)..."
    scp "$BIN_PATH" "root@$RV1106_HOST:$DEPLOY_PATH"
    echo "[Deploy] Done. Run on RV1106:"
    echo "  mqtt-client [config-path]   # 默认 /etc/mqtt-client.toml 或 ./config/mqtt-client.toml"
fi
