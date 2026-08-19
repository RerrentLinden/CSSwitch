#!/bin/bash
# 结束 C1 官方实例直通冒烟:停 Science daemon 与试验网关,恢复常态。
# 之后按平常方式启动 Science(不带 env)即为官方直连。
set -euo pipefail

RUN_DIR="$HOME/.csswitch/official-smoke"

SCI=""
for c in "$(command -v claude-science || true)" "$HOME/.claude-science/bin/claude-science" "$HOME/.local/bin/claude-science"; do
  if [ -n "$c" ] && [ -x "$c" ]; then SCI="$c"; break; fi
done
if [ -n "$SCI" ]; then
  "$SCI" stop || true
fi

if [ -f "$RUN_DIR/gateway.pid" ]; then
  kill "$(cat "$RUN_DIR/gateway.pid")" 2>/dev/null || true
  rm -f "$RUN_DIR/gateway.pid"
fi

echo "已停止:试验网关与 Science daemon。之后按平常方式启动 Science 即为官方直连。"
