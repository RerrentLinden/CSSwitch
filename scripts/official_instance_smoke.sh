#!/bin/bash
# C1 官方实例直通冒烟(任务 08-19-official-instance-smoke)。
# 形态:官方 Claude Science(默认 profile、真实登录)→ 本地直通网关 → api.anthropic.com。
# 网关仅记录脱敏端点日志(方法/路径/状态/耗时),不记录任何 header/body。
# Science 二进制用用户自己的安装(这不是 CSSwitch 托管沙箱启动,不走 snapshot 机制)。
# 不传 --no-auto-update:那是隔离沙箱时代的规则,官方实例必须保留它自己的
# 自动更新行为(该参数只影响单次 daemon 运行,不写入配置)。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT="${1:-8791}"
RUN_DIR="$HOME/.csswitch/official-smoke"
mkdir -p "$RUN_DIR"
LOG="$RUN_DIR/endpoints-$(date +%Y%m%d-%H%M%S).jsonl"

# shellcheck source=../test/_cargo_path.sh
source "$ROOT/test/_cargo_path.sh"
ensure_rust_toolchain_on_path || { echo "找不到 cargo 工具链" >&2; exit 1; }

echo "[1/5] 构建网关(release)…"
cargo build --release --manifest-path "$ROOT/desktop/gateway/Cargo.toml"
BIN="$ROOT/desktop/gateway/target/release/csswitch-gateway"

SCI=""
for c in "$(command -v claude-science || true)" "$HOME/.claude-science/bin/claude-science" "$HOME/.local/bin/claude-science"; do
  if [ -n "$c" ] && [ -x "$c" ]; then SCI="$c"; break; fi
done
[ -n "$SCI" ] || { echo "找不到 claude-science 可执行文件(PATH / ~/.claude-science/bin / ~/.local/bin)" >&2; exit 1; }
echo "Science CLI: $SCI"

echo "[2/5] 确保官方 daemon 已停止…"
"$SCI" stop >/dev/null 2>&1 || true

echo "[3/5] 启动直通网关 127.0.0.1:$PORT …"
if [ -f "$RUN_DIR/gateway.pid" ] && kill -0 "$(cat "$RUN_DIR/gateway.pid")" 2>/dev/null; then
  kill "$(cat "$RUN_DIR/gateway.pid")" 2>/dev/null || true
  sleep 0.3
fi
nohup "$BIN" official-passthrough --port "$PORT" --log "$LOG" >"$RUN_DIR/gateway.out" 2>&1 &
echo $! > "$RUN_DIR/gateway.pid"
sleep 1

echo "[4/5] 回环连通性探测(无鉴权,期望上游 401)…"
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 20 "http://127.0.0.1:$PORT/v1/models")"
echo "GET /v1/models -> HTTP $CODE"
if [ "$CODE" = "000" ]; then
  echo "网关或上游不可达,终止(诊断见 $RUN_DIR/gateway.out)" >&2
  exit 1
fi

echo "[5/5] 以官方默认 profile 启动 Claude Science(仅注入 ANTHROPIC_BASE_URL)…"
env -u ANTHROPIC_API_KEY -u ANTHROPIC_AUTH_TOKEN \
    -u ANTHROPIC_MODEL -u ANTHROPIC_REASONING_MODEL \
    -u ANTHROPIC_DEFAULT_HAIKU_MODEL -u ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME \
    -u ANTHROPIC_DEFAULT_SONNET_MODEL -u ANTHROPIC_DEFAULT_SONNET_MODEL_NAME \
    -u ANTHROPIC_DEFAULT_OPUS_MODEL -u ANTHROPIC_DEFAULT_OPUS_MODEL_NAME \
    -u ANTHROPIC_DEFAULT_FABLE_MODEL -u ANTHROPIC_DEFAULT_FABLE_MODEL_NAME \
    -u ANTHROPIC_SMALL_FAST_MODEL \
    ANTHROPIC_BASE_URL="http://127.0.0.1:$PORT" \
    "$SCI" serve --port 0 --detached --no-browser

URL="$("$SCI" url 2>/dev/null | grep -oE 'https?://[^[:space:]]+' | head -1 || true)"
echo
echo "Science UI:  ${URL:-"(url 子命令未返回,可执行 '$SCI status' 查看)"}"
echo "端点日志:    $LOG"
echo "网关输出:    $RUN_DIR/gateway.out"
echo "结束试验:    $ROOT/scripts/official_instance_smoke_stop.sh"
if [ -n "$URL" ]; then open "$URL" || true; fi
