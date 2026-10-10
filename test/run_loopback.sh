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

# 默认测刚构建出来的 debug 二进制。CSSWITCH_LOOPBACK_BIN 可以显式指向别的产物
# (例如 dist/CSSwitch.app/Contents/MacOS/CSSwitch),让应用包里的那个二进制过同一套断言;
# 这时不需要 cargo。
if [ -n "${CSSWITCH_LOOPBACK_BIN:-}" ]; then
  BIN="$CSSWITCH_LOOPBACK_BIN"
  [ -x "$BIN" ] || { echo "CSSWITCH_LOOPBACK_BIN 指向的文件不可执行:$BIN" >&2; exit 1; }
else
  ensure_rust_toolchain_on_path || { echo "env-blocked: 找不到 cargo 工具链" >&2; exit 3; }
  MANIFEST="$ROOT/desktop/gateway/Cargo.toml"
  cargo build --manifest-path "$MANIFEST" --quiet || exit 1
  BIN="$ROOT/desktop/gateway/target/debug/csswitch-gateway"
fi

SANDBOX="$(mktemp -d)"
PORT=$(( 18800 + RANDOM % 400 ))
# 启动器拉起的服务脱离了启动器,也就不是本脚本的子进程,kill $! 够不着它。
# 先请它自己退出;再按命令行里的日志路径兜底(路径在本次运行的临时目录里,唯一)。
stop_launched() { # stop_launched <端口> <service.log 路径>
  pgrep -f -- "--log-file $2" >/dev/null 2>&1 || return 0
  curl -s --max-time 30 -X POST "http://127.0.0.1:$1/control/quit" >/dev/null 2>&1
  for _ in $(seq 1 25); do
    pgrep -f -- "--log-file $2" >/dev/null 2>&1 || return 0
    sleep 0.2
  done
  pkill -f -- "--log-file $2" 2>/dev/null
}
cleanup() {
  [ -n "${PID:-}" ] && kill "$PID" 2>/dev/null
  [ -n "${LEGACY_PID:-}" ] && kill "$LEGACY_PID" 2>/dev/null
  [ -n "${SB_PID:-}" ] && kill "$SB_PID" 2>/dev/null
  # 后台那个启动器要等它自己跑完(有超时,几秒):先放掉它占着的端口,它反而会真的起一个服务。
  [ -n "${L_BUSY_JOB:-}" ] && wait "$L_BUSY_JOB" 2>/dev/null
  [ -n "${L_BUSY_NC:-}" ] && kill "$L_BUSY_NC" 2>/dev/null
  [ -n "${L_OLD_NC:-}" ] && kill "$L_OLD_NC" 2>/dev/null
  if [ -n "${L_PORT:-}" ]; then
    stop_launched "$L_PORT" "$L_LOG"
    # 另外几个场景本不该留下服务;万一留下了(被测逻辑出错),同样不能漏。
    pkill -f -- "--log-file $SANDBOX/" 2>/dev/null
    pkill -f -- "$SANDBOX/hung-shell" 2>/dev/null
  fi
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
echo "被测二进制:$BIN"

check "WebUI 可达"        200 "$(code "http://127.0.0.1:$PORT/")"
check "控制面状态可达"     200 "$(code "http://127.0.0.1:$PORT/control/status")"
check "未知路径显式 404"   404 "$(code "http://127.0.0.1:$PORT/v1/unknown")"
check "未知控制操作报错"   400 "$(code -X POST "http://127.0.0.1:$PORT/control/nope")"

# 版本号只有一个来源:状态接口报告的,就是这个二进制 --version 打印的。
# 顶层 version 是状态对象的第一个键(沙箱状态里另有一个嵌套的 version,那是 Science 的)。
bin_version="$("$BIN" --version)"
case "$bin_version" in
  [0-9]*.[0-9]*.[0-9]*) echo "  ok   --version 打印版本号($bin_version)" ;;
  *) echo "  FAIL --version 没有打印版本号:「$bin_version」"; fail=1 ;;
esac
check "状态接口的版本与 --version 一致" "$bin_version" \
  "$(body "http://127.0.0.1:$PORT/control/status" | sed -n 's/^{"version":"\([^"]*\)".*/\1/p')"

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

# ── 安装版启动器 ────────────────────────────────────────────────────────────
# 应用包的主可执行文件经 Finder 打开时以启动器角色运行:无参数 + CSSWITCH_APP_LAUNCH=1
# (Info.plist 的 LSEnvironment)。这里用同样的方式直接执行二进制,不经 LaunchServices。
# 环境按 Finder 启动的样子给:PATH 只有系统目录,没有 CLAUDE_SCIENCE_BIN —— 这些只能从
# 登录 shell 来(这里是假的),所以它们出现在服务进程里就证明环境真的传到了。
# open / osascript 换成只记录参数的假命令,不会打开浏览器,也不会弹任何窗口。
L_HELPERS="$SANDBOX/launcher-helpers"
L_SCI="$SANDBOX/launcher-science"
L_CALLS="$L_HELPERS/calls.log"
mkdir -p "$L_HELPERS" "$L_SCI"
for helper in open osascript; do
  cat >"$L_HELPERS/$helper" <<'EOF'
#!/bin/bash
# 一次调用记一行:参数里的换行写成字面量 \n。
{ printf '%s' "$(basename "$0")"; for arg in "$@"; do printf ' [%s]' "${arg//$'\n'/\\n}"; done; printf '\n'; } \
  >>"$(dirname "$0")/calls.log"
EOF
  chmod +x "$L_HELPERS/$helper"
done
# 假 claude-science:每次被调用都记下自己看到的环境(服务每次读状态都会调它一次 status)。
cat >"$L_SCI/claude-science" <<'EOF'
#!/bin/bash
echo "canary=${CSSWITCH_LOOPBACK_CANARY:-absent} path=$PATH" >>"$(dirname "$0")/env.log"
[ "$1" = status ] && echo '{"running":false}'
exit 0
EOF
chmod +x "$L_SCI/claude-science"
# 假登录 shell:像 rc 文件那样先打印横幅、导出变量,再执行启动器交给它的命令。
# 不是以 -l -i -c 调用就直接失败:真实 shell 少了这两个开关就不会读用户的配置。
cat >"$SANDBOX/login-shell" <<EOF
#!/bin/bash
echo "Last login: rc 文件打印的横幅"
echo "PATH=/from/the/banner"
[ "\$1 \$2 \$3" = "-l -i -c" ] || exit 64
export CLAUDE_SCIENCE_BIN="$L_SCI/claude-science"
export PATH="$L_SCI/only-in-login-shell:/usr/bin:/bin:/usr/sbin:/sbin"
export CSSWITCH_LOOPBACK_CANARY="must-not-reach-the-service"
exec /bin/sh -c "\$4"
EOF
# 卡住的登录 shell(rc 文件在等什么东西):启动器必须超时放弃、把它结束掉,服务照常启动。
cat >"$SANDBOX/hung-shell" <<'EOF'
#!/bin/bash
exec -a "$0.hung" /bin/sleep 600
EOF
chmod +x "$SANDBOX/login-shell" "$SANDBOX/hung-shell"

# launcher_home <目录> <端口>:建一个隔离 HOME,配置文件只为了指定端口。
launcher_home() {
  mkdir -p "$1/.csswitch"
  cat >"$1/.csswitch/service.v1.json" <<EOF
{"mode":"official","port":$2,
 "kimi":{"base_url":"https://api.example.invalid","api_key":"",
         "default_model":{"model_id":"m1","display_name":"M1"}},
 "deepseek":{"base_url":"https://api.example.invalid","api_key":"",
             "default_model":{"model_id":"d1","display_name":""}}}
EOF
}
# launch <HOME> <登录 shell>:返回启动器的退出码。
launch() {
  env -i HOME="$1" SHELL="$2" PATH=/usr/bin:/bin:/usr/sbin:/sbin \
    CSSWITCH_APP_HELPER_DIR="$L_HELPERS" CSSWITCH_APP_LAUNCH=1 "$BIN" 2>/dev/null
}
launched_pids() { pgrep -f -- "--log-file $1" | tr '\n' ' ' | sed 's/ $//'; }
wait_for_listener() {
  for _ in $(seq 1 25); do
    lsof -nP -iTCP:"$1" -sTCP:LISTEN >/dev/null 2>&1 && return 0
    sleep 0.2
  done
}

L_HOME="$SANDBOX/launcher-home"
L_PORT=$(( PORT + 2 ))
L_LOG="$L_HOME/.csswitch/service.log"
L_STATUS="http://127.0.0.1:$L_PORT/control/status"
launcher_home "$L_HOME" "$L_PORT"

# 端口被占的场景要等一次响应超时(占着端口的 nc 不回话),放到后台与下面的主动线并行。
L_BUSY_HOME="$SANDBOX/launcher-busy-home"
L_BUSY_PORT=$(( PORT + 3 ))
launcher_home "$L_BUSY_HOME" "$L_BUSY_PORT"
nc -lk 127.0.0.1 "$L_BUSY_PORT" </dev/null >/dev/null 2>&1 &
L_BUSY_NC=$!
wait_for_listener "$L_BUSY_PORT"
( launch "$L_BUSY_HOME" "$SANDBOX/login-shell"; echo $? >"$SANDBOX/busy.exit" ) &
L_BUSY_JOB=$!

# 首次启动:拉起后台服务、请求打开控制台,启动器自己退出。
launch "$L_HOME" "$SANDBOX/login-shell"
check "首次启动:启动器退出码 0" 0 $?
check "首次启动:请求打开的是控制台地址" "open [http://127.0.0.1:$L_PORT/]" \
  "$(grep ":$L_PORT/" "$L_CALLS" 2>/dev/null | tail -1)"
first_pid="$(launched_pids "$L_LOG")"
case "$first_pid" in
  ''|*' '*) echo "  FAIL 首次启动后应当恰好有一个后台服务,实际 pid:「$first_pid」"; fail=1 ;;
  *) echo "  ok   首次启动拉起了一个后台服务" ;;
esac
check "启动器退出后服务仍在,且已脱离启动器" 1 \
  "$(ps -o ppid= -p "${first_pid:-0}" 2>/dev/null | tr -d ' ')"
launched_status="$(body "$L_STATUS")"
has "服务以配置里的端口提供控制面" "\"port\":$L_PORT" "$launched_status"
check "service.log 权限 0600" 600 \
  "$(stat -f '%Lp' "$L_LOG" 2>/dev/null || stat -c '%a' "$L_LOG" 2>/dev/null)"

# 登录 shell 的环境真的到了服务进程:白名单内的到,白名单外的不到。
has "登录 shell 导出的 CLAUDE_SCIENCE_BIN 到达服务进程" \
  "\"binary\":\"$L_SCI/claude-science\"" "$launched_status"
has "登录 shell 的 PATH 到达服务进程" "path=$L_SCI/only-in-login-shell:" \
  "$(tail -1 "$L_SCI/env.log" 2>/dev/null)"
lacks "白名单之外的变量没有进入服务进程" "must-not-reach-the-service" \
  "$(cat "$L_SCI/env.log" 2>/dev/null)"
has "启动器在日志里记下导入了哪些变量" "已从登录 shell 导入" "$(cat "$L_LOG")"
has "命令目录 override 在日志里留痕" "CSSWITCH_APP_HELPER_DIR 生效" "$(cat "$L_LOG")"

# 再次启动:服务已在跑,只再开一次控制台,不拉起第二个。
launch "$L_HOME" "$SANDBOX/login-shell"
check "再次启动:启动器退出码 0" 0 $?
check "再次启动:没有换进程,也没有第二个服务" "$first_pid" "$(launched_pids "$L_LOG")"
check "再次启动:又请求打开了一次控制台" 2 \
  "$(grep -c "^open \[http://127.0.0.1:$L_PORT/\]$" "$L_CALLS")"
# 多拉起的那个服务会因端口被占立刻退出,数进程看不出来,但它一定会在日志里留下一行。
check "再次启动:服务只启动过一次" 1 "$(grep -c 'CSSwitch 控制台:' "$L_LOG")"
lacks "再次启动:没有第二个服务去抢端口" "无法监听" "$(cat "$L_LOG")"
# 此刻唯一可能已经出现的提示,是后台那个「端口被占」场景的(正文里带着它自己的日志路径)。
check "正常启动不弹任何提示" 0 "$(grep '^osascript ' "$L_CALLS" | grep -vc "$L_BUSY_HOME/")"

# 退出服务后能重新启动;这一次登录 shell 卡住:留日志、结束掉它、服务照常起来。
stop_launched "$L_PORT" "$L_LOG"
check "退出服务后进程消失" "" "$(launched_pids "$L_LOG")"
launch "$L_HOME" "$SANDBOX/hung-shell"
check "登录 shell 卡住:启动器仍以 0 退出" 0 $?
has "登录 shell 卡住:日志里写明没有取到环境" "没有取到登录 shell 的环境" "$(cat "$L_LOG")"
check "登录 shell 卡住:被启动器结束,没有留下" "" \
  "$(pgrep -f -- "$SANDBOX/hung-shell" | tr '\n' ' ')"
hung_status="$(body "$L_STATUS")"
has "登录 shell 卡住:服务仍然启动" "\"port\":$L_PORT" "$hung_status"
# 没有登录 shell 的环境,服务就只有启动器自己的那一份:里面找不到任何 claude-science。
has "登录 shell 卡住:服务沿用启动器自身的环境" '"science":{"installed":false' "$hung_status"
stop_launched "$L_PORT" "$L_LOG"

# 真的 zsh(macOS 的默认 shell)读隔离 HOME 里的 .zshrc:上面的假 shell 只证明了管道接得上,
# 这一条证明我们调用登录 shell 的方式(-l -i -c、printf 的引号、env -0)在真实 shell 里成立。
cat >"$L_HOME/.zshrc" <<EOF
echo "来自 .zshrc 的横幅"
export CLAUDE_SCIENCE_BIN="$L_SCI/claude-science"
export PATH="$L_SCI/only-in-zshrc:\$PATH"
EOF
# 同一次启动顺带验证日志有上限:先把日志撑过 5 MiB(5242880 字节),服务一启动就该把它挪到
# service.log.1、另起新文件,并把自己正在写的 stdout / stderr 接过去。
yes '(填充行:把 service.log 撑过 5 MiB 的上限)' | head -c 5300000 >>"$L_LOG"
launch "$L_HOME" /bin/zsh
check "真实 zsh:启动器退出码 0" 0 $?
has "真实 zsh:.zshrc 里的 CLAUDE_SCIENCE_BIN 到达服务进程" \
  "\"binary\":\"$L_SCI/claude-science\"" "$(body "$L_STATUS")"
has "真实 zsh:.zshrc 里的 PATH 到达服务进程" "path=$L_SCI/only-in-zshrc:" \
  "$(tail -1 "$L_SCI/env.log" 2>/dev/null)"
stop_launched "$L_PORT" "$L_LOG"
file_size() { wc -c <"$1" 2>/dev/null | tr -d ' '; }
check "超限的日志整份挪到 service.log.1" yes \
  "$([ "$(file_size "$L_LOG.1")" -ge 5300000 ] 2>/dev/null && echo yes || echo "no($(file_size "$L_LOG.1") 字节)")"
check "轮转后的新日志从头开始,仍是 0600" "small 600" \
  "$([ "$(file_size "$L_LOG")" -lt 100000 ] && echo small || echo big) $(stat -f '%Lp' "$L_LOG" 2>/dev/null || stat -c '%a' "$L_LOG" 2>/dev/null)"
# 退出时服务写的那一行必须出现在新文件里:证明进程正在写的描述符真的被接到了新文件上,
# 而不是继续往改了名的旧文件里写。
has "轮转后服务的日志写进新文件" "退出服务" "$(cat "$L_LOG")"

# 后台跑着的是别的版本(这里是不报告版本号的更早服务):提示,然后只打开控制台,不拉起新服务。
L_OLD_HOME="$SANDBOX/launcher-old-home"
L_OLD_PORT=$(( PORT + 4 ))
launcher_home "$L_OLD_HOME" "$L_OLD_PORT"
old_status='{"mode":"official","mode_label":"official","port":0}'
printf 'HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: %s\r\nconnection: close\r\n\r\n%s' \
  "${#old_status}" "$old_status" | nc -l 127.0.0.1 "$L_OLD_PORT" >/dev/null 2>&1 &
L_OLD_NC=$!
wait_for_listener "$L_OLD_PORT"
launch "$L_OLD_HOME" "$SANDBOX/login-shell"
check "后台是别的版本:启动器退出码 0" 0 $?
old_alert="$(grep '^osascript ' "$L_CALLS" 2>/dev/null | grep '不是当前版本' | tail -1)"
has "后台是别的版本:提示先退出旧服务" "退出服务" "$old_alert"
has "后台是别的版本:提示里有当前应用的版本" "$bin_version" "$old_alert"
check "后台是别的版本:仍然打开控制台" 1 \
  "$(grep -c "^open \[http://127.0.0.1:$L_OLD_PORT/\]$" "$L_CALLS")"
check "后台是别的版本:不拉起新服务" "" "$(launched_pids "$L_OLD_HOME/.csswitch/service.log")"
kill "$L_OLD_NC" 2>/dev/null
wait "$L_OLD_NC" 2>/dev/null
L_OLD_NC=

# 端口被别的程序占着(开头放到后台的那一个):非零退出、弹提示、不开控制台、不留服务。
wait "$L_BUSY_JOB" 2>/dev/null
L_BUSY_JOB=
check "端口被占:启动器退出码非零" 1 "$(cat "$SANDBOX/busy.exit" 2>/dev/null)"
busy_alert="$(grep '^osascript ' "$L_CALLS" 2>/dev/null | grep '无法启动' | tail -1)"
has "端口被占:提示里指出是哪个端口" "端口 $L_BUSY_PORT" "$busy_alert"
has "端口被占:提示里带着日志最后一行(系统的原始报错)" \
  "无法监听 127.0.0.1:$L_BUSY_PORT" "$busy_alert"
has "端口被占:提示里给出日志路径" "$L_BUSY_HOME/.csswitch/service.log" "$busy_alert"
check "端口被占:没有去打开控制台" 0 \
  "$(grep -c "^open \[http://127.0.0.1:$L_BUSY_PORT/\]$" "$L_CALLS")"
check "端口被占:没有留下服务进程" "" "$(launched_pids "$L_BUSY_HOME/.csswitch/service.log")"
kill "$L_BUSY_NC" 2>/dev/null
wait "$L_BUSY_NC" 2>/dev/null
L_BUSY_NC=

if [ "$fail" != "0" ]; then
  echo; echo "服务输出:"; cat "$SANDBOX/out.log"
  # 只看末尾:轮转没发生时,日志里还留着上面灌进去的 5 MB 填充行。
  echo; echo "启动器段日志(末尾):"
  tail -n 40 "$L_LOG" "$L_BUSY_HOME/.csswitch/service.log" 2>/dev/null | grep -v '填充行'
  echo; echo "假 open / osascript 收到的调用:"; cat "$L_CALLS" 2>/dev/null
fi
exit "$fail"
