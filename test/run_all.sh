#!/bin/bash
# CSSwitch 门禁:三层,全部离线可跑。
#
#   1 static   —— cargo fmt --check + clippy(零告警)
#   2 unit     —— cargo test(补偿链、配置、目录、控制面)
#   3 loopback —— 起真实服务进程,打真实 HTTP,校验路由与脱敏
#
# 真机验收(官方实例 + 真实 provider key)不在此处,见 test/LIVE_ACCEPTANCE.md。
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=_cargo_path.sh
source "$ROOT/test/_cargo_path.sh"
ensure_rust_toolchain_on_path || { echo "env-blocked: 找不到 cargo 工具链" >&2; exit 3; }

MANIFEST="$ROOT/desktop/gateway/Cargo.toml"
fail=0

layer() {
  local name="$1"; shift
  echo "── $name ──"
  if "$@"; then
    echo "[PASS] $name"
  else
    echo "[FAIL] $name"
    fail=1
  fi
  echo
}

static_layer() {
  cargo fmt --manifest-path "$MANIFEST" --check || return 1
  # clippy 的零告警门禁只看本仓库代码:传递依赖的 future-incompat 提示不计入。
  local out
  out="$(cargo clippy --manifest-path "$MANIFEST" --all-targets 2>&1)" || return 1
  local n
  n="$(printf '%s\n' "$out" | grep -cE '^(warning|error)(\[|:).*' | tr -d ' ')"
  local ours
  ours="$(printf '%s\n' "$out" | grep -E '^(warning|error)' | grep -v 'future version of Rust' | grep -cv '^note' | tr -d ' ')"
  if [ "$ours" != "0" ]; then
    printf '%s\n' "$out" | grep -E '^(warning|error)' | grep -v 'future version of Rust'
    return 1
  fi
  return 0
}

layer "1 static  (fmt + clippy)" static_layer
layer "2 unit    (cargo test)"   cargo test --manifest-path "$MANIFEST" --quiet
layer "3 loopback(真实服务进程)" bash "$ROOT/test/run_loopback.sh"

if [ "$fail" = "0" ]; then
  echo "全部通过"
else
  echo "存在失败层" >&2
fi
exit "$fail"
