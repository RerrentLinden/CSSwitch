#!/usr/bin/env bash
# S0 离线纯单元层：无 loopback / 无网络 / 无上游。
# 模块清单 = 原 catalog SUITE-PY-OFFLINE ∪ 旧脚本清单，另纳入曾无入口调度的孤儿测试
# skill_runtime_boundary（见 docs/audits/v083-test-system-audit.md）；
# 另一孤儿 external_skill_install_bridge 需拉起 gateway 二进制 + loopback，归入 loopback 层。
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"
if ! command -v python3 >/dev/null 2>&1; then
  echo "S0_LAYER offline env-blocked (no python3)"; exit 0
fi
# test_build_sidecar_identity 需要 rustc（本地编译桩 rlib，不联网）；
# 找不到工具链时该测试会显式断言失败，不静默跳过。
. "$ROOT/test/_cargo_path.sh"
ensure_rust_toolchain_on_path || true
if python3 -m unittest \
    test.test_capability \
    test.test_capability_catalog \
    test.test_process_ownership_policy \
    test.test_codex_browser_auth_contract \
    test.test_build_sidecar_identity \
    test.test_profile_pin_contract \
    test.test_skill_runtime_boundary \
    -v; then
  echo "S0_LAYER offline pass"; exit 0
else
  echo "S0_LAYER offline fail"; exit 1
fi
