#!/usr/bin/env bash
# S0 rust 层：四个 Cargo 工程统一 fmt+clippy+test。无 cargo → env-blocked。
# 无 loopback → src-tauri 跳过端口 bind 测试并把本层标为 env-blocked。
# ignored 测试计数汇总在 S0_LAYER 行尾（ignored=N）；已知 manual-ignored 集见 docs/operations/testing.md。
set -u -o pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

. "$ROOT/test/_cargo_path.sh"
if ! ensure_rust_toolchain_on_path; then
  echo "S0_LAYER rust env-blocked (no cargo)"; exit 0
fi

fail=0
blocked=0
ignored_total=0
TEST_LOG="$(mktemp -t csswitch-rust-layer)" || exit 1
trap 'rm -f "$TEST_LOG"' EXIT

lint_crate() {  # $1: Cargo 工程目录（相对仓库根）
  (cd "$ROOT/$1" && cargo fmt --check) || fail=1
  (cd "$ROOT/$1" && cargo clippy --all-targets -- -D warnings) || fail=1
}

count_ignored() {  # 从最近一次 cargo test 输出累加 ignored 数
  n="$(grep -Eo '[0-9]+ ignored' "$TEST_LOG" | awk '{s+=$1} END {print s+0}')"
  ignored_total=$((ignored_total + n))
}

# desktop/src-tauri：端口 bind 测试名单（无 loopback 时 skip 并标 env-blocked）
lint_crate desktop/src-tauri
PORT_TESTS="pick_scratch_port_returns_usable_nonreserved_port two_picks_are_bindable loopback_port_occupancy_probe_detects_listener_without_http"
if [ "$(python3 "$ROOT/test/_capability.py")" = "1" ]; then
  (cd "$ROOT/desktop/src-tauri" && cargo test 2>&1 | tee "$TEST_LOG") || fail=1
else
  blocked=1
  echo "loopback 禁 → 跳过端口 bind 测试，本 rust 层标记 env-blocked：$PORT_TESTS"
  skip_args=""; for t in $PORT_TESTS; do skip_args="$skip_args --skip $t"; done
  (cd "$ROOT/desktop/src-tauri" && cargo test -- $skip_args 2>&1 | tee "$TEST_LOG") || fail=1
fi
count_ignored

for crate in desktop/gateway desktop/codex-network; do
  lint_crate "$crate"
  (cd "$ROOT/$crate" && cargo test 2>&1 | tee "$TEST_LOG") || fail=1
  count_ignored
done

# desktop/skill-package：测试须单线程执行
lint_crate desktop/skill-package
(cd "$ROOT/desktop/skill-package" && cargo test -- --test-threads=1 2>&1 | tee "$TEST_LOG") || fail=1
count_ignored

if [ "$fail" -ne 0 ]; then echo "S0_LAYER rust fail ignored=$ignored_total"; exit 1; fi
if [ "$blocked" -ne 0 ]; then echo "S0_LAYER rust env-blocked (loopback bind tests skipped) ignored=$ignored_total"; exit 0; fi
echo "S0_LAYER rust pass ignored=$ignored_total"; exit 0
