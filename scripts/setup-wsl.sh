#!/usr/bin/env bash
# Bailongma WSL 开发环境初始化脚本（幂等，可重复执行，重复运行只做校验/跳过）。
#
# 用法：
#   bash scripts/setup-wsl.sh            # 普通用户运行（全部装到 $HOME，无需 root）
#   sudo bash scripts/setup-wsl.sh       # 顺带修复 apt 源为 HTTPS（可选）
#
# 解决的问题（实测踩坑）：
#   * 本机透明代理按 TLS 指纹拦截 apt(gnutls) 的请求，apt 装不了任何包；
#     curl / python 正常 → 改用 conda-forge 预编译 gcc + make（HTTPS 直连）。
#   * Rust 官方 rustup 走 HTTPS，正常。
#   * reqwest 已改纯 rustls、rusqlite bundled，编译仅需 C 编译器，无需系统库。
#
# 安装内容（全部用户级，不污染系统）：
#   1. Rust 工具链          -> ~/.cargo + ~/.rustup（rustup + stable minimal）
#   2. C 工具链             -> ~/miniconda3（gcc/make/openssl/pkg-config）
#   3. 环境变量             -> 追加到 ~/.bashrc（带标记块，可安全重复执行）
#   4.（可选，需 sudo）apt 源 http:// -> https://
#
# 之后构建：bash scripts/start-serve.sh（环境变量已在 .bashrc，新开终端生效）
set -euo pipefail

say()  { printf '\033[1;34m[setup]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[warn]\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31m[error]\033[0m %s\n' "$*"; exit 1; }

# ── 0) 平台与前置检查 ──────────────────────────────────────────────
[ "$(uname -s)" = "Linux" ] || die "仅支持 Linux（WSL / 原生均可）"
command -v curl >/dev/null 2>&1 || die "缺少 curl（apt-get install -y curl 或手动安装）"

# ── 1) Rust 工具链（rustup + stable） ─────────────────────────────
if [ -x "$HOME/.cargo/bin/cargo" ]; then
  say "Rust 已存在: $("$HOME/.cargo/bin/cargo" --version 2>/dev/null)"
else
  say "安装 Rust（rustup + stable minimal）..."
  curl -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
  say "Rust 安装完成"
fi
[ -x "$HOME/.cargo/bin/cargo" ] || die "Rust 安装失败"

# ── 2) C 工具链（conda-forge gcc，绕开 apt） ──────────────────────
CONDA_PREFIX="$HOME/miniconda3"
CC_BIN="$CONDA_PREFIX/bin/x86_64-conda-linux-gnu-cc"

if [ -x "$CC_BIN" ]; then
  say "conda gcc 已存在: $("$CC_BIN" --version | head -1)"
else
  say "安装 miniconda（$CONDA_PREFIX）..."
  curl -sL -o /tmp/miniconda-setup.sh https://repo.anaconda.com/miniconda/Miniconda3-latest-Linux-x86_64.sh
  bash /tmp/miniconda-setup.sh -b -p "$CONDA_PREFIX" || die "miniconda 安装失败"
  rm -f /tmp/miniconda-setup.sh

  # 新版 conda 需要接受默认 channel 的 ToS 才能安装（幂等，失败可忽略）
  "$CONDA_PREFIX/bin/conda" tos accept --override-channels --channel https://repo.anaconda.com/pkgs/main >/dev/null 2>&1 || true
  "$CONDA_PREFIX/bin/conda" tos accept --override-channels --channel https://repo.anaconda.com/pkgs/r    >/dev/null 2>&1 || true

  say "安装 gcc / make / openssl / pkg-config（conda-forge，预编译二进制）..."
  "$CONDA_PREFIX/bin/conda" install -y -q -c conda-forge gcc make openssl pkg-config \
    || die "conda 安装工具链失败（检查网络后重试）"
  say "conda 工具链安装完成"
fi
[ -x "$CC_BIN" ] || die "conda gcc 不可用"

# ── 3)（可选）apt 源 http -> https，修复 80 端口被代理拦截 ────────
if command -v sudo >/dev/null 2>&1; then
  if ! curl -sI --max-time 6 http://archive.ubuntu.com/ubuntu/dists/noble/InRelease >/dev/null 2>&1; then
    if sudo -n true 2>/dev/null; then
      say "检测到 http 源不可达，切换为 https 并刷新 apt 索引（幂等）..."
      # Ubuntu 24.04: deb822 格式；旧版: sources.list。两者都处理，缺文件时静默。
      sudo sed -i 's|http://|https://|g' /etc/apt/sources.list.d/ubuntu.sources 2>/dev/null || true
      sudo sed -i 's|http://|https://|g' /etc/apt/sources.list                      2>/dev/null || true
      sudo apt-get update -qq 2>&1 | tail -1 || warn "apt update 失败（编译不依赖 apt，可忽略）"
    else
      warn "sudo 需要密码，跳过 apt 源修复（编译不受影响）。如需 apt 请手动执行："
      warn "  sudo sed -i 's|http://|https://|g' /etc/apt/sources.list.d/ubuntu.sources"
      warn "  sudo apt-get update"
    fi
  fi
fi

# ── 4) 环境变量写入 ~/.bashrc（标记块，重复执行安全） ──────────────
BASH_RC="$HOME/.bashrc"
MARK="bailongma-wsl env"
if [ -f "$BASH_RC" ] && grep -q "$MARK" "$BASH_RC"; then
  say "~/.bashrc 已包含环境变量块（跳过写入）"
else
  cat >> "$BASH_RC" <<'EOF'

# >>> bailongma-wsl env (managed by scripts/setup-wsl.sh) >>>
export RUSTUP_HOME="$HOME/.rustup"
export CARGO_HOME="$HOME/.cargo"
export PATH="$HOME/.cargo/bin:$HOME/miniconda3/bin:$PATH"
export CC="$HOME/miniconda3/bin/x86_64-conda-linux-gnu-cc"
export CXX="$HOME/miniconda3/bin/x86_64-conda-linux-gnu-c++"
export RUSTFLAGS="-C linker=$HOME/miniconda3/bin/x86_64-conda-linux-gnu-cc"
export CARGO_TARGET_DIR="${BAILONGMA_TARGET_DIR:-$HOME/bailongma-target}"
# <<< bailongma-wsl env <<<
EOF
  say "环境变量已写入 ~/.bashrc（新开终端生效；或先执行: source ~/.bashrc）"
fi

# ── 5) 验证 ────────────────────────────────────────────────────────
export RUSTUP_HOME="$HOME/.rustup"
export CARGO_HOME="$HOME/.cargo"
export PATH="$HOME/.cargo/bin:$CONDA_PREFIX/bin:$PATH"
export CC="$CC_BIN"
export CXX="$CONDA_PREFIX/bin/x86_64-conda-linux-gnu-c++"
export RUSTFLAGS="-C linker=$CC_BIN"
export CARGO_TARGET_DIR="${BAILONGMA_TARGET_DIR:-$HOME/bailongma-target}"

echo ""
say "环境就绪，版本信息："
printf '  %-22s %s\n' "cargo"  "$(cargo  --version)"
printf '  %-22s %s\n' "rustc"  "$(rustc  --version)"
printf '  %-22s %s\n' "gcc"    "$("$CC_BIN" --version | head -1)"
printf '  %-22s %s\n' "make"   "$("$CONDA_PREFIX/bin/make" --version | head -1)"
echo ""
say "下一步："
echo "    source ~/.bashrc"
echo "    bash scripts/start-serve.sh    # 构建并启动 API 服务器"
echo "    bash scripts/stop-serve.sh     # 停止"
