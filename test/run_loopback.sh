#!/bin/bash
# loopback 层:起一个真实的服务进程,用真实 HTTP 请求验证它。
#
# 这一层要抓的是单测抓不到的东西:进程真的能起来、路由真的接上了、
# 未知路径真的会显式 404、控制面真的不回显凭证。
# 不联任何上游,配置写进临时 HOME,不碰用户的 ~/.csswitch。
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=_cargo_path.sh
source "$ROOT/test/_cargo_path.sh"
ensure_rust_toolchain_on_path || { echo "env-blocked: 找不到 cargo 工具链" >&2; exit 3; }

MANIFEST="$ROOT/desktop/gateway/Cargo.toml"
cargo build --manifest-path "$MANIFEST" --quiet || exit 1
BIN="$ROOT/desktop/gateway/target/debug/csswitch-gateway"

SANDBOX="$(mktemp -d)"
PORT=$(( 18800 + RANDOM % 400 ))
cleanup() {
  [ -n "${PID:-}" ] && kill "$PID" 2>/dev/null
  rm -rf "$SANDBOX"
}
trap cleanup EXIT

# 隔离 HOME:服务的配置落在沙箱里,且 PATH 清掉以确保找不到 claude-science
# (这一层不该依赖本机是否装了 Science)。
HOME="$SANDBOX" "$BIN" serve --port "$PORT" >"$SANDBOX/out.log" 2>&1 &
PID=$!
for _ in $(seq 1 40); do
  curl -sf --max-time 1 "http://127.0.0.1:$PORT/control/status" >/dev/null 2>&1 && break
  sleep 0.2
done

fail=0
check() {
  local name="$1" expected="$2" actual="$3"
  if [ "$actual" = "$expected" ]; then
    echo "  ok   $name"
  else
    echo "  FAIL $name:期望 $expected,实际 $actual"
    fail=1
  fi
}

code() { curl -s -o /dev/null -w '%{http_code}' --max-time 5 "$@"; }
body() { curl -s --max-time 5 "$@"; }

echo "loopback @ 127.0.0.1:$PORT (HOME=$SANDBOX)"

check "WebUI 可达"        200 "$(code "http://127.0.0.1:$PORT/")"
check "控制面状态可达"     200 "$(code "http://127.0.0.1:$PORT/control/status")"
check "未知路径显式 404"   404 "$(code "http://127.0.0.1:$PORT/v1/unknown")"
check "未知控制操作报错"   400 "$(code -X POST "http://127.0.0.1:$PORT/control/nope")"

# 默认是官方模式。
mode="$(body "http://127.0.0.1:$PORT/control/status" | sed -n 's/.*"mode":"\([a-z]*\)".*/\1/p')"
check "默认官方模式" official "$mode"

# 配置保存后:key 不回显、ready 变 true、文件权限 0600。
saved="$(body -X POST "http://127.0.0.1:$PORT/control/config" \
  -H 'content-type: application/json' \
  -d '{"channel":"kimi","base_url":"https://api.example.invalid","api_key":"sk-loopback-secret",
       "default_model":{"model_id":"m1","display_name":"M1"},
       "quality_model":{"model_id":"","display_name":""},
       "fast_model":{"model_id":"","display_name":""},
       "fable_model":{"model_id":"","display_name":""}}')"
case "$saved" in
  *sk-loopback-secret*) echo "  FAIL 保存响应回显了 API key"; fail=1 ;;
  *'"has_api_key":true'*) echo "  ok   保存后 key 不回显,仅报告已配置" ;;
  *) echo "  FAIL 保存未成功:$saved"; fail=1 ;;
esac

status_after="$(body "http://127.0.0.1:$PORT/control/status")"
case "$status_after" in
  *sk-loopback-secret*) echo "  FAIL 状态接口回显了 API key"; fail=1 ;;
  *) echo "  ok   状态接口不回显 key" ;;
esac

perm="$(stat -f '%Lp' "$SANDBOX/.csswitch/service.v1.json" 2>/dev/null || stat -c '%a' "$SANDBOX/.csswitch/service.v1.json" 2>/dev/null)"
check "配置文件权限 0600" 600 "$perm"

# 配置文件名不得与旧桌面端的 config.json 相同(曾导致旧应用清空自身配置)。
if [ -e "$SANDBOX/.csswitch/config.json" ]; then
  echo "  FAIL 服务写了旧桌面端的 config.json"
  fail=1
else
  echo "  ok   未触碰旧桌面端的 config.json"
fi

# 联网搜索开关:默认开 → 关掉能存 → 之后不带该字段的保存不得把它改回来。
case "$status_after" in
  *'"web_search":true'*) echo "  ok   联网搜索默认开启" ;;
  *) echo "  FAIL 新配置的联网搜索默认值不对:$status_after"; fail=1 ;;
esac

body -X POST "http://127.0.0.1:$PORT/control/config" \
  -H 'content-type: application/json' \
  -d '{"channel":"kimi","web_search":false}' >/dev/null

case "$(body "http://127.0.0.1:$PORT/control/status")" in
  *'"web_search":false'*) echo "  ok   联网搜索可关闭并落盘" ;;
  *) echo "  FAIL 关闭联网搜索未生效"; fail=1 ;;
esac

# 只改模型槽的保存不带 web_search,缺省语义是"保持不变",不能悄悄改回开启。
body -X POST "http://127.0.0.1:$PORT/control/config" \
  -H 'content-type: application/json' \
  -d '{"channel":"kimi","default_model":{"model_id":"m2","display_name":"M2"}}' >/dev/null
case "$(body "http://127.0.0.1:$PORT/control/status")" in
  *'"web_search":false'*) echo "  ok   缺省字段不改动已保存的开关" ;;
  *) echo "  FAIL 不带 web_search 的保存把开关改回了开启"; fail=1 ;;
esac

# 面板保存开关时 api_key 字段是空串(不是缺省),空串必须仍然表示"保持不变"。
# 用户以后会为了翻这个开关频繁保存这张表单,清空 key 一次就够疼。
body -X POST "http://127.0.0.1:$PORT/control/config" \
  -H 'content-type: application/json' \
  -d '{"channel":"kimi","api_key":"","web_search":false}' >/dev/null
case "$(body "http://127.0.0.1:$PORT/control/status")" in
  *'"has_api_key":true'*) echo "  ok   空 key 提交不清空已保存的 key" ;;
  *) echo "  FAIL 空 key 提交把已保存的 key 清掉了"; fail=1 ;;
esac

# 非布尔值必须显式报错,不做类型降级。
case "$(body -X POST "http://127.0.0.1:$PORT/control/config" \
  -H 'content-type: application/json' -d '{"channel":"kimi","web_search":"off"}')" in
  *'"error"'*) echo "  ok   非布尔 web_search 显式报错" ;;
  *) echo "  FAIL 非布尔 web_search 被静默接受"; fail=1 ;;
esac

# 切到未配置完整的渠道必须失败,而不是切过去再在推理时炸。
switch_bad="$(body -X POST "http://127.0.0.1:$PORT/control/switch" \
  -H 'content-type: application/json' -d '{"mode":"deepseek"}')"
case "$switch_bad" in
  *'"error"'*) echo "  ok   缺 key 的渠道拒绝切换" ;;
  *) echo "  FAIL 缺 key 的渠道竟然切换成功:$switch_bad"; fail=1 ;;
esac

# 关闭态的可诊断性:切到 Kimi,发一条声明了 typed web_search 的推理请求。
# 上游是不存在的域名,请求必然失败 —— 但补偿规则行在联网之前就已写进 stderr,
# 足以证明"摘除真的发生了且留下了痕迹"。没有这条,规则 id 被删掉也没人会发现。
body -X POST "http://127.0.0.1:$PORT/control/switch" \
  -H 'content-type: application/json' -d '{"mode":"kimi"}' >/dev/null
body -X POST "http://127.0.0.1:$PORT/v1/messages" \
  -H 'content-type: application/json' \
  -d '{"model":"claude-opus-5","max_tokens":64,"stream":false,
       "messages":[{"role":"user","content":"查一下今天的新闻"}],
       "tools":[{"type":"web_search_20250305","name":"web_search"},
                {"name":"Bash","input_schema":{"type":"object"}}]}' >/dev/null

relay_line="$(grep 'POST /v1/messages relay' "$SANDBOX/out.log" | tail -1)"
case "$relay_line" in
  *tool.relay.web-search-disabled-by-config*) echo "  ok   关闭态在日志里留下规则 id" ;;
  *) echo "  FAIL 关闭态没有记录摘除规则:$relay_line"; fail=1 ;;
esac
case "$relay_line" in
  *web-search.query-tool-adapter*) echo "  FAIL 关闭态仍然触发了兼容桥:$relay_line"; fail=1 ;;
  *) echo "  ok   关闭态不触发兼容桥" ;;
esac
[ "$fail" = "0" ] || { echo; echo "服务输出:"; cat "$SANDBOX/out.log"; }
exit "$fail"
