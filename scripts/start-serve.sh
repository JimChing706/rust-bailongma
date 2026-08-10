#!/usr/bin/env bash
# 启动 bailongma API 服务器（后台运行），等待 /status 就绪后返回。
#
# 用法：
#   ./scripts/start-serve.sh              # debug 构建
#   ./scripts/start-serve.sh --release    # release 构建
#   BAILONGMA_API_TOKEN=secret ./scripts/start-serve.sh   # 启用 token 校验
#
# 停止：./scripts/stop-serve.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MODE="debug"
if [ "${1:-}" = "--release" ]; then MODE="release"; fi
PORT=3721

mkdir -p "$ROOT/logs"
LOG="$ROOT/logs/serve.log"
ERR="$ROOT/logs/serve.log.err"
PIDF="$ROOT/logs/serve.pid"

# 1) 构建（确保最新代码）
echo "[1/3] cargo build ${MODE/debug/} -p bailongma-app --bin serve"
( cd "$ROOT" && cargo build ${MODE/debug/} -p bailongma-app --bin serve )

# 2) 启动（后台 + 重定向 + 记录 PID）
# 支持 CARGO_TARGET_DIR 指向独立 target（避免与 Windows 的 target/ 互相重编译）
TGT="${CARGO_TARGET_DIR:-$ROOT/target}"
EXE="$TGT/$MODE/serve"
[ -x "$EXE" ] || { echo "未找到 $EXE" >&2; exit 1; }

if [ -f "$PIDF" ]; then
  OLD="$(tr -d '[:space:]' < "$PIDF")"
  if [ -n "$OLD" ] && kill -0 "$OLD" 2>/dev/null; then
    kill "$OLD" 2>/dev/null || true
    echo "[*] 已停止旧实例 PID=$OLD"
  fi
  rm -f "$PIDF"
fi

echo "[2/3] 启动 $EXE"
nohup "$EXE" >"$LOG" 2>"$ERR" &
echo $! > "$PIDF"
PID="$!"

# 3) 轮询就绪（agent 扫描最长 15s，给足余量）
echo "[3/3] 等待 http://127.0.0.1:$PORT/status 就绪（agent 扫描最长 15s）..."
for _ in $(seq 1 90); do
  if ! kill -0 "$PID" 2>/dev/null; then
    echo "serve 已退出，详见 $LOG / $ERR" >&2; exit 1
  fi
  if curl -fsS "http://127.0.0.1:$PORT/status" >/dev/null 2>&1; then
    echo ""
    echo "✔ serve 已就绪：PID=$PID  端口=$PORT"
    echo "  日志：$LOG"
    echo "  停止：./scripts/stop-serve.sh"
    echo "  验证：curl http://127.0.0.1:$PORT/status"
    exit 0
  fi
  sleep 0.5
done

echo "等待 /status 就绪超时，详见 $LOG / $ERR" >&2
exit 1
