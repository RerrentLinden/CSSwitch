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
  [ -n "${LEGACY_PID:-}" ] && kill "$LEGACY_PID" 2>/dev/null
  [ -n "${SB_PID:-}" ] && kill "$SB_PID" 2>/dev/null
  rm -rf "$SANDBOX"
}
trap cleanup EXIT

# 假 claude-science:把每次调用的完整参数记进 calls.log,不监听任何端口。
# - `running` 标记文件决定 status 的回答:官方形态的 serve(不带 --data-dir)建它,
#   官方形态的 stop(不带 --data-dir)清它。沙箱形态的 stop 什么也不做,留给服务的
#   兜底 SIGTERM 去收口。
# - `active` 标记文件让 status 报告有会话正在进行。输出形状与真实 `status` 一致
#   (会话数在 daemon.active_conversations),否则「有会话先确认」测的就是假设。
# - 官方形态的 serve 另记一行注入的 ANTHROPIC_BASE_URL。
FAKE_SCI="$SANDBOX/fake-science"
mkdir -p "$FAKE_SCI"
cat >"$FAKE_SCI/claude-science" <<'EOF'
#!/bin/bash
dir="$(dirname "$0")"
echo "$*" >>"$dir/calls.log"
case "$1" in
  status)
    if [ ! -e "$dir/running" ]; then echo '{"running":false}'
    elif [ -e "$dir/active" ]; then echo '{"running":true,"daemon":{"active_conversations":2}}'
    else echo '{"running":true,"daemon":{"active_conversations":0}}'
    fi ;;
  stop) [ "$#" -eq 1 ] && rm -f "$dir/running" ;;
  serve)
    case " $* " in
      *" --data-dir "*) ;;
      *) touch "$dir/running"; echo "${ANTHROPIC_BASE_URL:-}" >>"$dir/serve-base-url.log" ;;
    esac ;;
esac
exit 0
EOF
chmod +x "$FAKE_SCI/claude-science"

# 隔离 HOME:服务的配置落在沙箱里。Science 二进制显式指向假脚本
# (CLAUDE_SCIENCE_BIN 无效时 fail closed,不回落):这一层既不依赖本机是否装了
# Science,也绝不触碰本机的真实实例。
HOME="$SANDBOX" CLAUDE_SCIENCE_BIN="$FAKE_SCI/claude-science" \
  "$BIN" serve --port "$PORT" >"$SANDBOX/out.log" 2>&1 &
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
# post <控制面动作> <JSON 请求体>
post() {
  body -X POST "http://127.0.0.1:$PORT/control/$1" -H 'content-type: application/json' -d "$2"
}
# has / lacks <名称> <子串> <实际文本>
has() {
  case "$3" in
    *"$2"*) echo "  ok   $1" ;;
    *) echo "  FAIL $1:应包含「$2」,实际 $3"; fail=1 ;;
  esac
}
lacks() {
  case "$3" in
    *"$2"*) echo "  FAIL $1:不应包含「$2」,实际 $3"; fail=1 ;;
    *) echo "  ok   $1" ;;
  esac
}
# 状态接口顶层的联网搜索总开关。
web_search_state() {
  body "http://127.0.0.1:$PORT/control/status" | sed -nE 's/.*"web_search":(true|false).*/\1/p'
}
# 假 claude-science 收到的最后一次 serve,以及 stop / serve 的累计次数(判断有没有重启过)。
last_serve() { grep '^serve' "$FAKE_SCI/calls.log" 2>/dev/null | tail -1; }
restart_calls() { grep -cE '^(stop|serve)' "$FAKE_SCI/calls.log" 2>/dev/null; }
# 向当前渠道发一条声明了 typed web_search 的推理请求,返回它留下的 relay 日志行。
# 上游是不存在的域名,请求必然失败 —— 但规则行在联网之前就已写进 stderr。
relay_line_for_search_request() {
  local before
  before="$(grep -c 'POST /v1/messages relay' "$SANDBOX/out.log")"
  body -X POST "http://127.0.0.1:$PORT/v1/messages" \
    -H 'content-type: application/json' \
    -d '{"model":"claude-opus-5","max_tokens":64,"stream":false,
         "messages":[{"role":"user","content":"查一下今天的新闻"}],
         "tools":[{"type":"web_search_20250305","name":"web_search"},
                  {"name":"Bash","input_schema":{"type":"object"}}]}' >/dev/null
  # 只取这次请求新写出的行:没有新行就返回空,不拿上一次请求的结果充数。
  grep 'POST /v1/messages relay' "$SANDBOX/out.log" | tail -n +"$((before + 1))" | tail -1
}

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

# 切到未配置完整的渠道必须失败,而不是切过去再在推理时炸。
switch_bad="$(post switch '{"mode":"deepseek"}')"
case "$switch_bad" in
  *'"error"'*) echo "  ok   缺 key 的渠道拒绝切换" ;;
  *) echo "  FAIL 缺 key 的渠道竟然切换成功:$switch_bad"; fail=1 ;;
esac

# ── 联网搜索总开关 ──────────────────────────────────────────────────────────
# 一个全局开关驱动两层:Science 启动时带不带 `--config <镜像>`,以及网关桥不桥。
# Science 自己的配置放在临时 HOME 里(服务只读它),镜像由服务写进临时 HOME 的 .csswitch。
SCI_HOME="$SANDBOX/.claude-science"
SCI_CONFIG="$SCI_HOME/config.toml"
MIRROR="$SANDBOX/.csswitch/science-config/official.toml"
SERVICE_JSON="$SANDBOX/.csswitch/service.v1.json"
SERVE_ON="serve --port 8686 --sandbox-port 8687 --detached --no-browser"
SERVE_OFF="serve --config $MIRROR --port 8686 --sandbox-port 8687 --detached --no-browser"
mkdir -p "$SCI_HOME"
cat >"$SANDBOX/config.good.toml" <<'EOF'
# 管理员下发
update_channel = "stable"

[network]
proxy = "http://proxy.corp.example:8080"
EOF
cp "$SANDBOX/config.good.toml" "$SCI_CONFIG"

# 默认开启;字段只在顶层出现一次(渠道投影里不再有第二份)。
check "联网搜索默认开启" true "$(web_search_state)"
check "状态接口只有一个 web_search 字段" 1 \
  "$(printf '%s' "$status_after" | grep -o '"web_search"' | wc -l | tr -d ' ')"

# 开启态:启动调用与引入开关之前逐项相同,只注入 base_url,不生成任何镜像。
post science/start '{}' >/dev/null
check "开启态启动参数与主线逐项相同" "$SERVE_ON" "$(last_serve)"
check "启动时注入网关 base_url" "http://127.0.0.1:$PORT" "$(tail -1 "$FAKE_SCI/serve-base-url.log")"
if [ -e "$SANDBOX/.csswitch/science-config" ]; then
  echo "  FAIL 开启态生成了镜像目录"; fail=1
else
  echo "  ok   开启态不生成镜像"
fi

# 旧端点带 web_search:显式报错并指向新端点,不静默忽略。
legacy_post="$(post config '{"channel":"kimi","web_search":false}')"
has "旧端点带 web_search 显式报错并指向新端点" "/control/web-search" "$legacy_post"
check "旧端点没有改动总开关" true "$(web_search_state)"

# 非布尔值必须显式报错,不做类型降级。
has "非布尔 enabled 显式报错" '"error"' "$(post web-search '{"enabled":"off"}')"
check "非布尔 enabled 没有改动总开关" true "$(web_search_state)"

# 有会话正在进行且未确认:拒绝,配置与 Science 都不变。
touch "$FAKE_SCI/active"
cp "$SERVICE_JSON" "$SANDBOX/service.before.json"
before_restarts="$(restart_calls)"
refused="$(post web-search '{"enabled":false}')"
has "有活跃会话且未确认时拒绝切换" "有 2 个会话正在进行" "$refused"
check "被拒绝后总开关不变" true "$(web_search_state)"
check "被拒绝后 Science 没有被重启" "$before_restarts" "$(restart_calls)"
if cmp -s "$SERVICE_JSON" "$SANDBOX/service.before.json" && [ ! -e "$MIRROR" ]; then
  echo "  ok   被拒绝后配置文件不变,也没有生成镜像"
else
  echo "  FAIL 被拒绝后配置文件变了或生成了镜像"; fail=1
fi
rm -f "$FAKE_SCI/active"

# Science 自己的 config.toml 无法解析:报出文件与原因,不存盘、不重启、不留镜像。
printf '[network]\nproxy = http://unquoted.example:8080\n' >"$SCI_CONFIG"
before_restarts="$(restart_calls)"
broken="$(post web-search '{"enabled":false}')"
has "源配置无法解析时报出是哪个文件" "$SCI_CONFIG" "$broken"
has "源配置无法解析时报出原因" "无法解析" "$broken"
check "源配置无法解析时总开关不变" true "$(web_search_state)"
check "源配置无法解析时 Science 没有被重启" "$before_restarts" "$(restart_calls)"
if cmp -s "$SERVICE_JSON" "$SANDBOX/service.before.json" && [ ! -e "$MIRROR" ]; then
  echo "  ok   源配置无法解析时配置文件不变,也没有生成镜像"
else
  echo "  FAIL 源配置无法解析时配置文件变了或生成了镜像"; fail=1
fi
cp "$SANDBOX/config.good.toml" "$SCI_CONFIG"

# 关闭(有会话,带确认):存盘、停掉再带着镜像启动。
touch "$FAKE_SCI/active"
turned_off="$(post web-search '{"enabled":false,"force":true}')"
rm -f "$FAKE_SCI/active"
has "确认后关闭成功并重启了 Science" '"science_restarted":true' "$turned_off"
check "关闭后总开关为关" false "$(web_search_state)"
check "关闭态启动带且只带镜像配置" "$SERVE_OFF" "$(last_serve)"
check "重启是先停后起" "stop" "$(grep -E '^(stop|serve)' "$FAKE_SCI/calls.log" | tail -2 | head -1)"
check "关闭态启动同样注入网关 base_url" "http://127.0.0.1:$PORT" "$(tail -1 "$FAKE_SCI/serve-base-url.log")"

# 镜像 = 用户原配置 + 根表的 enable_web_search = false(必须写在第一个 [table] 之前)。
key_line="$(grep -nE '^enable_web_search[[:space:]]*=[[:space:]]*false[[:space:]]*$' "$MIRROR" 2>/dev/null | head -1 | cut -d: -f1)"
table_line="$(grep -n '^\[network\]' "$MIRROR" 2>/dev/null | head -1 | cut -d: -f1)"
if [ -n "$key_line" ] && [ -n "$table_line" ] && [ "$key_line" -lt "$table_line" ]; then
  echo "  ok   镜像在根表里关闭了联网搜索"
else
  echo "  FAIL 镜像没有在根表里关闭联网搜索(键在第 ${key_line:-?} 行,[network] 在第 ${table_line:-?} 行)"; fail=1
fi
if grep -q '^proxy = "http://proxy.corp.example:8080"$' "$MIRROR" 2>/dev/null \
  && grep -q '^update_channel = "stable"$' "$MIRROR" 2>/dev/null; then
  echo "  ok   镜像保留了用户原配置的其它键"
else
  echo "  FAIL 镜像丢了用户原配置的键"; fail=1
fi
check "镜像文件权限 0600" 600 \
  "$(stat -f '%Lp' "$MIRROR" 2>/dev/null || stat -c '%a' "$MIRROR" 2>/dev/null)"
if cmp -s "$SCI_CONFIG" "$SANDBOX/config.good.toml" && [ "$(ls -A "$SCI_HOME")" = "config.toml" ]; then
  echo "  ok   Science 自己的目录只被读取:config.toml 不变,没有多出文件"
else
  echo "  FAIL Science 自己的目录被改动了:$(ls -A "$SCI_HOME")"; fail=1
fi

# 只改模型槽的保存不得改动总开关;空 key 仍然表示"保持不变"。
post config '{"channel":"kimi","api_key":"","default_model":{"model_id":"m2","display_name":"M2"}}' >/dev/null
check "保存渠道配置不改动总开关" false "$(web_search_state)"
has "空 key 提交不清空已保存的 key" '"has_api_key":true' "$(body "http://127.0.0.1:$PORT/control/status")"

# 关闭态下「启动 Science」与「切换模式」都要把总开关的值带进启动参数。
post science/stop '{}' >/dev/null
post science/start '{}' >/dev/null
check "关闭态点启动同样带镜像配置" "$SERVE_OFF" "$(last_serve)"
before_restarts="$(restart_calls)"
post switch '{"mode":"kimi"}' >/dev/null
check "关闭态切换模式的重启同样带镜像配置" "$((before_restarts + 2)):$SERVE_OFF" "$(restart_calls):$(last_serve)"

# 关闭态的网关一侧:typed web_search 在请求入口被摘除并留下规则 id,兼容桥不触发。
# 没有这条,规则 id 被删掉、或开关没接到网关上,都没人会发现。
relay_line="$(relay_line_for_search_request)"
has "关闭态 Kimi 在日志里留下摘除规则" "tool.relay.web-search-disabled-by-config" "$relay_line"
lacks "关闭态 Kimi 不触发兼容桥" "web-search.query-tool-adapter" "$relay_line"

# 开关是全局的:DeepSeek 同样被摘除(它平时会把搜索声明原样留给上游)。
post config '{"channel":"deepseek","base_url":"https://api.example.invalid","api_key":"sk-loopback-ds"}' >/dev/null
post switch '{"mode":"deepseek"}' >/dev/null
has "关闭态 DeepSeek 在日志里留下摘除规则" "tool.relay.web-search-disabled-by-config" \
  "$(relay_line_for_search_request)"
post switch '{"mode":"kimi"}' >/dev/null

# 重新开启:Science 回到不带 --config 的原始启动方式,残留镜像被清掉;
# 网关一侧不等任何重启,同一条请求重新走桥。
turned_on="$(post web-search '{"enabled":true}')"
has "重新开启成功并重启了 Science" '"science_restarted":true' "$turned_on"
check "重新开启后总开关为开" true "$(web_search_state)"
check "重新开启后启动参数回到主线" "$SERVE_ON" "$(last_serve)"
if [ -e "$MIRROR" ]; then
  echo "  FAIL 重新开启后还留着镜像文件"; fail=1
else
  echo "  ok   重新开启后不留镜像文件"
fi
relay_line="$(relay_line_for_search_request)"
has "重新开启后 Kimi 重新走兼容桥" "web-search.query-tool-adapter" "$relay_line"
lacks "重新开启后不再摘除搜索声明" "tool.relay.web-search-disabled-by-config" "$relay_line"

# 存量配置的迁移(另起一个服务进程读一份旧格式的 service.v1.json):
# 在旧界面的 Kimi 页签关过搜索 → 总开关读成关闭;保存后只剩顶层新字段。
LEGACY_HOME="$SANDBOX/legacy-home"
LEGACY_JSON="$LEGACY_HOME/.csswitch/service.v1.json"
LEGACY_PORT=$(( PORT + 1 ))
mkdir -p "$LEGACY_HOME/.csswitch"
cat >"$LEGACY_JSON" <<'EOF'
{"mode":"official",
 "kimi":{"base_url":"https://api.example.invalid","api_key":"sk-legacy",
         "default_model":{"model_id":"m1","display_name":"M1"},"web_search":false},
 "deepseek":{"base_url":"https://api.example.invalid","api_key":"",
             "default_model":{"model_id":"d1","display_name":""},"web_search":true}}
EOF
HOME="$LEGACY_HOME" CLAUDE_SCIENCE_BIN="$FAKE_SCI/claude-science" \
  "$BIN" serve --port "$LEGACY_PORT" >"$SANDBOX/legacy.log" 2>&1 &
LEGACY_PID=$!
for _ in $(seq 1 40); do
  curl -sf --max-time 1 "http://127.0.0.1:$LEGACY_PORT/control/status" >/dev/null 2>&1 && break
  sleep 0.2
done
check "存量配置里 Kimi 关过搜索 → 总开关读成关闭" false \
  "$(body "http://127.0.0.1:$LEGACY_PORT/control/status" | sed -nE 's/.*"web_search":(true|false).*/\1/p')"
body -X POST "http://127.0.0.1:$LEGACY_PORT/control/config" -H 'content-type: application/json' \
  -d '{"channel":"kimi","default_model":{"model_id":"m2","display_name":"M2"}}' >/dev/null
check "保存后只写顶层新字段,渠道级旧字段消失" '"web_search": false' \
  "$(grep -o '"web_search": *[a-z]*' "$LEGACY_JSON" | tr '\n' ';' | sed 's/;$//')"
kill "$LEGACY_PID" 2>/dev/null
wait "$LEGACY_PID" 2>/dev/null
LEGACY_PID=

# 退出服务必须先停掉两个实例,再退出进程(放在最后:之后服务就不在了)。
# 官方实例:假脚本报告在跑。沙箱:一个命令行与真 daemon 同形的假进程
# (argv[0] 以 claude-science 结尾 + serve + 精确 --data-dir),pid 写进沙箱 lock。
# 双层子 shell 让它脱离本脚本,被 launchd 及时回收——留成僵尸会被当成还活着。
touch "$FAKE_SCI/running"
stops_before_quit="$(grep -cx 'stop' "$FAKE_SCI/calls.log")"
SB_DATA="$SANDBOX/.csswitch/science-sandbox/home/.claude-science"
mkdir -p "$SB_DATA"
SB_PID="$( (exec -a "$FAKE_SCI/claude-science" /bin/bash -c 'while :; do sleep 1; done' \
  serve --data-dir "$SB_DATA" >/dev/null 2>&1 &
  echo $!) )"
printf '{"pid":%s}' "$SB_PID" >"$SB_DATA/operon.lock"
case "$(body "http://127.0.0.1:$PORT/control/status")" in
  *'"sandbox":{'*'"running":true'*) echo "  ok   假沙箱 daemon 被识别为运行中" ;;
  *) echo "  FAIL 假沙箱 daemon 未被识别,退出场景不成立"; fail=1 ;;
esac

quit="$(curl -s --max-time 30 -X POST "http://127.0.0.1:$PORT/control/quit")"
case "$quit" in
  *'"stopped":["Science","免登录沙箱"]'*) echo "  ok   退出响应列出两个已停止的实例" ;;
  *) echo "  FAIL 退出响应不对:$quit"; fail=1 ;;
esac
if [ "$(grep -cx 'stop' "$FAKE_SCI/calls.log")" -gt "$stops_before_quit" ] && [ ! -e "$FAKE_SCI/running" ]; then
  echo "  ok   官方实例收到了 stop"
else
  echo "  FAIL 官方实例没有被停止"; fail=1
fi
if kill -0 "$SB_PID" 2>/dev/null; then
  echo "  FAIL 沙箱 daemon 仍在运行"; fail=1
else
  echo "  ok   沙箱 daemon 已停止"
fi
for _ in $(seq 1 25); do
  kill -0 "$PID" 2>/dev/null || break
  sleep 0.2
done
if kill -0 "$PID" 2>/dev/null; then
  echo "  FAIL 退出服务后进程仍在"; fail=1
else
  echo "  ok   退出服务后进程已退出"
fi
[ "$fail" = "0" ] || { echo; echo "服务输出:"; cat "$SANDBOX/out.log"; }
exit "$fail"
