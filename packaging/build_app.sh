#!/bin/bash
# 从当前 commit 构建安装版:dist/CSSwitch.app 与 dist/CSSwitch-<版本>-<架构>.dmg。
#
#   bash packaging/build_app.sh                 发布用:工作区必须干净
#   bash packaging/build_app.sh --allow-dirty   开发自测:允许带着未提交的改动构建
#
# 只写 dist/(已在 .gitignore)。不安装到 /Applications,不打开应用,不 commit / tag。
# 签名是 ad-hoc(codesign --sign -):不是 Developer ID 签名,更不是公证;经下载分发的
# 应用首次打开需要用户在「系统设置 → 隐私与安全性」里放行。
#
# 版本号的唯一来源是 desktop/gateway/Cargo.toml;架构跟随构建机。
# 任何一步自检不过都以非零退出,不留下「看起来构建成功」的产物。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MANIFEST="$ROOT/desktop/gateway/Cargo.toml"
DIST="$ROOT/dist"
APP="$DIST/CSSwitch.app"
EXECUTABLE="$APP/Contents/MacOS/CSSwitch"
PLIST="$APP/Contents/Info.plist"
ICON="$ROOT/packaging/icon/AppIcon.icns"
BUNDLE_ID="io.github.rerrentlinden.csswitch"

ALLOW_DIRTY=0
for arg in "$@"; do
  case "$arg" in
    --allow-dirty) ALLOW_DIRTY=1 ;;
    *) echo "用法:$0 [--allow-dirty]" >&2; exit 2 ;;
  esac
done

step() { echo; echo "── $* ──"; }
die() { echo "失败:$*" >&2; exit 1; }
# verify <名称> <期望> <实际>
verify() {
  if [ "$3" = "$2" ]; then
    echo "  ok   $1"
  else
    die "$1:期望「$2」,实际「$3」"
  fi
}
plist_value() { /usr/libexec/PlistBuddy -c "Print :$1" "$PLIST" 2>/dev/null; }
# hdiutil 在新系统上每次调用都往 stderr 打一行「已弃用」提示(功能照常)。
# 成功时不显示它的输出,失败时原样打出来。
quiet_hdiutil() {
  local output
  if ! output="$(hdiutil "$@" 2>&1)"; then
    echo "$output" >&2
    return 1
  fi
}

STAGE=""
MOUNT=""
DMG=""
ASSEMBLING=0
cleanup() {
  [ -n "$MOUNT" ] && hdiutil detach "$MOUNT" -quiet 2>/dev/null
  [ -n "$STAGE" ] && rm -rf "$STAGE"
  # 中途失败:把这次的半成品删掉。留着一个没过自检的 .app 或上一次的 .dmg,迟早被当成成品拿走。
  if [ "$ASSEMBLING" = 1 ]; then
    rm -rf "$APP"
    [ -n "$DMG" ] && rm -f "$DMG"
    echo "已删除未通过自检的产物。" >&2
  fi
  return 0
}
trap cleanup EXIT

# ── 1 工具链、commit、工作区 ────────────────────────────────────────────────
step "1 工具链与工作区"
# shellcheck source=../test/_cargo_path.sh
source "$ROOT/test/_cargo_path.sh"
ensure_rust_toolchain_on_path || { echo "env-blocked: 找不到 cargo 工具链" >&2; exit 3; }
for tool in git codesign plutil lipo otool hdiutil shasum; do
  command -v "$tool" >/dev/null 2>&1 || { echo "env-blocked: 找不到 $tool" >&2; exit 3; }
done
[ -f "$ICON" ] || die "缺少 $ICON(由 packaging/icon/build_icon.sh 生成并入库)"

COMMIT="$(git -C "$ROOT" rev-parse --short HEAD)"
# 未跟踪的文件也算:它们同样会被编进产物(例如还没 git add 的新模块),而任何 commit 里都没有。
CHANGES="$(git -C "$ROOT" status --porcelain)"
if [ -z "$CHANGES" ]; then
  echo "commit $COMMIT,工作区干净"
elif [ "$ALLOW_DIRTY" = 1 ]; then
  echo "commit $COMMIT,工作区有未提交的改动(--allow-dirty:产物不对应任何 commit,只供开发自测)"
else
  echo "$CHANGES" | head -20 >&2
  die "工作区有未提交的改动,产物无法对应到 commit $COMMIT。先提交;开发自测可加 --allow-dirty。"
fi

VERSION="$(awk -F'"' '/^\[/{section=$0} section=="[package]" && /^version[[:space:]]*=/{print $2; exit}' "$MANIFEST")"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "从 $MANIFEST 读到的版本号不合法:「$VERSION」"
ARCH="$(uname -m)"
DMG="$DIST/CSSwitch-$VERSION-$ARCH.dmg"
echo "版本 $VERSION,架构 $ARCH"

# ── 2 构建 ──────────────────────────────────────────────────────────────────
step "2 cargo build --release --locked"
cargo build --release --locked --manifest-path "$MANIFEST"
BUILT="$ROOT/desktop/gateway/target/release/csswitch-gateway"
[ -x "$BUILT" ] || die "构建后找不到 $BUILT"

# ── 3 组装应用包 ────────────────────────────────────────────────────────────
step "3 组装 $APP"
ASSEMBLING=1
rm -rf "$APP"
rm -f "$DMG"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BUILT" "$EXECUTABLE"
cp "$ICON" "$APP/Contents/Resources/AppIcon.icns"
# 最低系统版本取自二进制自己的 LC_BUILD_VERSION,不另写一份会过期的常量。
MIN_MACOS="$(otool -l "$EXECUTABLE" | awk '/LC_BUILD_VERSION/{found=1} found && /minos/{print $2; exit}')"
[[ "$MIN_MACOS" =~ ^[0-9]+\.[0-9]+ ]] || die "读不到二进制的最低系统版本(otool minos):「$MIN_MACOS」"
sed -e "s/@VERSION@/$VERSION/g" -e "s/@MIN_MACOS@/$MIN_MACOS/g" \
  "$ROOT/packaging/Info.plist.in" >"$PLIST"
# 扩展属性(隔离标记、Finder 信息)会让签名失败或随包带走,签名前清掉。
xattr -cr "$APP"

# ── 4 签名 ──────────────────────────────────────────────────────────────────
step "4 ad-hoc 签名"
codesign --force --sign - --identifier "$BUNDLE_ID" "$APP"

# ── 5 自检 ──────────────────────────────────────────────────────────────────
step "5 应用包自检"
plutil -lint "$PLIST" >/dev/null || die "Info.plist 不合法"
echo "  ok   Info.plist 合法"
verify "Info.plist 短版本号 == Cargo 版本" "$VERSION" "$(plist_value CFBundleShortVersionString)"
verify "Info.plist 构建版本号 == Cargo 版本" "$VERSION" "$(plist_value CFBundleVersion)"
# 包内二进制报告的版本:抓的是「包里其实是另一次构建的旧二进制」。
verify "包内二进制 --version == Cargo 版本" "$VERSION" \
  "$(env -u CSSWITCH_APP_LAUNCH "$EXECUTABLE" --version)"
verify "bundle id" "$BUNDLE_ID" "$(plist_value CFBundleIdentifier)"
verify "主可执行文件名与包内文件一致" "CSSwitch" "$(plist_value CFBundleExecutable)"
# 这两项决定双击之后发生什么:少了前者会多出一个 Dock 图标,
# 少了后者双击就成了前台服务 —— 再双击不会有任何反应。
verify "LSUIElement(不进 Dock)" "true" "$(plist_value LSUIElement)"
verify "LSEnvironment 让主进程进入启动器角色" "1" "$(plist_value LSEnvironment:CSSWITCH_APP_LAUNCH)"
verify "最低系统版本与二进制一致" "$MIN_MACOS" "$(plist_value LSMinimumSystemVersion)"
case " $(lipo -archs "$EXECUTABLE") " in
  *" $ARCH "*) echo "  ok   可执行文件含本机架构($ARCH)" ;;
  *) die "可执行文件架构是「$(lipo -archs "$EXECUTABLE")」,不含 $ARCH" ;;
esac
ICON_NAME="$(plist_value CFBundleIconFile)"
[ -s "$APP/Contents/Resources/$ICON_NAME.icns" ] || die "图标 $ICON_NAME.icns 不在包内"
echo "  ok   图标在包内($ICON_NAME.icns)"
verify "包内只有一个可执行文件" "Contents/MacOS/CSSwitch" \
  "$(cd "$APP" && find . -type f -perm +111 | sed 's|^\./||' | sort | tr '\n' ' ' | sed 's/ $//')"
codesign --verify --strict "$APP" || die "codesign --verify --strict 未通过"
echo "  ok   codesign --verify --strict"
case "$(codesign -dv "$APP" 2>&1)" in
  *"Signature=adhoc"*) echo "  ok   签名类型是 ad-hoc(不是 Developer ID,未公证)" ;;
  *) die "签名类型不是预期的 ad-hoc" ;;
esac

# ── 6 DMG ───────────────────────────────────────────────────────────────────
step "6 组装 $DMG"
STAGE="$(mktemp -d)"
ditto "$APP" "$STAGE/content/CSSwitch.app"
ln -s /Applications "$STAGE/content/Applications"
quiet_hdiutil create -volname "CSSwitch" -srcfolder "$STAGE/content" -fs HFS+ -format UDZO "$DMG" \
  || die "hdiutil create 未成功"
quiet_hdiutil verify "$DMG" || die "hdiutil verify 未通过"
echo "  ok   hdiutil verify"
# 只读挂到临时挂载点(不在 Finder 里出现),核对内容后卸载。
MOUNT="$STAGE/mount"
mkdir "$MOUNT"
quiet_hdiutil attach "$DMG" -readonly -nobrowse -noautoopen -mountpoint "$MOUNT" \
  || { MOUNT=""; die "DMG 挂载失败"; }
verify "DMG 根目录恰好是应用与「应用程序」快捷方式" "Applications CSSwitch.app" \
  "$(ls -A "$MOUNT" | sort | tr '\n' ' ' | sed 's/ $//')"
verify "快捷方式指向 /Applications" "/Applications" "$(readlink "$MOUNT/Applications")"
codesign --verify --strict "$MOUNT/CSSwitch.app" || die "DMG 里的应用签名校验未通过"
echo "  ok   DMG 里的应用签名完好"
verify "DMG 里的应用版本" "$VERSION" \
  "$(env -u CSSWITCH_APP_LAUNCH "$MOUNT/CSSwitch.app/Contents/MacOS/CSSwitch" --version)"
hdiutil detach "$MOUNT" -quiet
MOUNT=""
ASSEMBLING=0

# ── 7 产物 ──────────────────────────────────────────────────────────────────
step "7 产物"
size_of() { stat -f '%z' "$1"; }
echo "commit   $COMMIT$([ -n "$CHANGES" ] && echo "(工作区不干净,仅供自测)")"
echo "应用     $APP"
echo "         $(du -sk "$APP" | awk '{print $1}') KiB;可执行文件 $(size_of "$EXECUTABLE") 字节"
echo "         可执行文件 SHA-256 $(shasum -a 256 "$EXECUTABLE" | awk '{print $1}')"
echo "DMG      $DMG"
echo "         $(size_of "$DMG") 字节"
echo "         SHA-256 $(shasum -a 256 "$DMG" | awk '{print $1}')"
echo
echo "应用构建自检通过。未安装到 /Applications;签名为 ad-hoc,未公证。"
