#!/usr/bin/env bash
# run_all.sh 聚合契约测试：在隔离沙盒中用桩层脚本验证判定与退出码。
# 不递归实跑真实五层——真实层由 run_all.sh 本身调度，这里只测汇总逻辑。
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SANDBOX="$(mktemp -d -t csswitch-aggregator-test)" || exit 1
trap 'rm -rf "$SANDBOX"' EXIT
mkdir -p "$SANDBOX/test"
cp "$ROOT/test/run_all.sh" "$SANDBOX/test/run_all.sh"

write_stub() {  # $1=layer $2=状态（none = 不输出标记行）
  if [ "$2" = "none" ]; then
    printf '#!/usr/bin/env bash\necho "no marker here"\nexit 0\n' > "$SANDBOX/test/run-$1.sh"
  else
    printf '#!/usr/bin/env bash\necho "S0_LAYER %s %s"\nexit 0\n' "$1" "$2" > "$SANDBOX/test/run-$1.sh"
  fi
}
set_layers() {  # 按 offline loopback scripts rust frontend 顺序给 5 个状态
  write_stub offline "$1"; write_stub loopback "$2"; write_stub scripts "$3"
  write_stub rust "$4"; write_stub frontend "$5"
}

fails=0
ok() { echo "ok - $1"; }
no() { echo "NOT ok - $1"; fails=1; }

# 1) 全 pass：默认 rc=0，两种判定均 YES；release 模式 rc=0
set_layers pass pass pass pass pass
out="$(bash "$SANDBOX/test/run_all.sh" 2>/dev/null)"; rc=$?
[ "$rc" -eq 0 ] && ok "全 pass 默认 rc=0" || no "全 pass 默认 rc=$rc"
echo "$out" | grep -q "current-env clean: YES" && ok "全 pass clean=YES" || no "全 pass 缺 clean=YES"
echo "$out" | grep -q "release-ready green: YES" && ok "全 pass release=YES" || no "全 pass 缺 release=YES"
bash "$SANDBOX/test/run_all.sh" --require-release-ready >/dev/null 2>&1; rc=$?
[ "$rc" -eq 0 ] && ok "全 pass release 模式 rc=0" || no "全 pass release 模式 rc=$rc"

# 2) 一层 env-blocked：默认 rc=0 且 clean=YES release=NO；release 模式 rc=2
set_layers pass env-blocked pass pass pass
out="$(bash "$SANDBOX/test/run_all.sh" 2>/dev/null)"; rc=$?
[ "$rc" -eq 0 ] && ok "env-blocked 默认 rc=0" || no "env-blocked 默认 rc=$rc"
echo "$out" | grep -q "current-env clean: YES" && ok "env-blocked clean=YES" || no "env-blocked 缺 clean=YES"
echo "$out" | grep -q "release-ready green: NO" && ok "env-blocked release=NO" || no "env-blocked 缺 release=NO"
bash "$SANDBOX/test/run_all.sh" --require-release-ready >/dev/null 2>&1; rc=$?
[ "$rc" -eq 2 ] && ok "env-blocked release 模式 rc=2" || no "env-blocked release 模式 rc=$rc"

# 3) 一层 fail：rc=1，clean=NO
set_layers pass pass pass fail pass
out="$(bash "$SANDBOX/test/run_all.sh" 2>/dev/null)"; rc=$?
[ "$rc" -eq 1 ] && ok "fail 层 rc=1" || no "fail 层 rc=$rc"
echo "$out" | grep -q "current-env clean: NO" && ok "fail 层 clean=NO" || no "fail 层缺 clean=NO"

# 4) 缺标记行 = 按 fail 处理（不静默）：rc=1 且汇总标注
set_layers pass none pass pass pass
out="$(bash "$SANDBOX/test/run_all.sh" 2>/dev/null)"; rc=$?
[ "$rc" -eq 1 ] && ok "缺标记行 rc=1" || no "缺标记行 rc=$rc"
echo "$out" | grep -q "current-env clean: NO" && ok "缺标记行 clean=NO" || no "缺标记行缺 clean=NO"

# 5) S0 行尾注记（如 ignored=N）不影响状态解析并出现在汇总里
set_layers pass pass pass "pass ignored=7" pass
out="$(bash "$SANDBOX/test/run_all.sh" 2>/dev/null)"; rc=$?
[ "$rc" -eq 0 ] && ok "行尾注记 rc=0" || no "行尾注记 rc=$rc"
echo "$out" | grep -q "release-ready green: YES" && ok "行尾注记 release=YES" || no "行尾注记破坏 release 判定"
echo "$out" | grep -q "ignored=7" && ok "行尾注记进入汇总" || no "行尾注记未进入汇总"

# 6) 未知参数：rc=64（旧 kernel 的 --output-root 调用方式必须明确报错）
bash "$SANDBOX/test/run_all.sh" --output-root /tmp/x >/dev/null 2>&1; rc=$?
[ "$rc" -eq 64 ] && ok "未知参数 rc=64" || no "未知参数 rc=$rc"

[ "$fails" -eq 0 ] && echo "ALL PASS" || { echo "$fails FAILED"; exit 1; }
