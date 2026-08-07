# 供层脚本 source：把本机 Rust 工具链加入 PATH（若缺失）。
# 优先级：已在 PATH > rustup shim (~/.cargo/bin) > rustup toolchain 目录。
# 本机可能没有 rustup shim（历史上 kernel 因此硬编码 toolchain 绝对路径）；
# 这里改为按目录探测，找不到由调用方决定 env-blocked / fail，不静默。
ensure_rust_toolchain_on_path() {
  command -v cargo >/dev/null 2>&1 && return 0
  local d
  for d in "$HOME/.cargo/bin" "$HOME"/.rustup/toolchains/*/bin; do
    if [ -x "$d/cargo" ]; then export PATH="$d:$PATH"; return 0; fi
  done
  return 1
}
