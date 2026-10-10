#!/bin/bash
# 由 AppIcon.svg / AppIcon-small.svg 生成 AppIcon.icns(入库;应用构建只拷贝它,不依赖 Chrome)。
#
#   bash packaging/icon/build_icon.sh
#
# 流程:无头 Chrome 把两张 SVG 各渲染成 1024 px 透明底 PNG → sips 缩出 iconset 的十张图
# → iconutil 打成 .icns → 反解一遍核对十张齐全。
# 16 / 32 px 三张取自简化稿(主稿缩到这么小,开关就看不出来了),64 px 及以上取自主稿。
#
# 改图形:改 gen.py 的参数 → python3 packaging/icon/gen.py packaging/icon → 再跑本脚本。
# 需要 Google Chrome(本机没有 rsvg-convert / ImageMagick);路径不同可用 CHROME=<路径> 指定。
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHROME="${CHROME:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"
[ -x "$CHROME" ] || { echo "env-blocked: 找不到 Google Chrome($CHROME)" >&2; exit 3; }

WORK="$(mktemp -d)"
PROFILE="$WORK/chrome-profile"
# 无头 Chrome 写完截图后不会自己退出:每次渲染后按这个临时 profile 的路径把它收掉,
# 退出时再兜一次。profile 是本次运行独有的,不会碰到用户正在用的 Chrome。
stop_chrome() { pkill -f -- "--user-data-dir=$PROFILE" 2>/dev/null || true; }
cleanup() { stop_chrome; rm -rf "$WORK"; }
trap cleanup EXIT

size_of() { stat -f '%z' "$1" 2>/dev/null || echo 0; }

render() { # render <svg> <png>:1024×1024,透明底
  local svg="$1" png="$2" pid last=-1 now
  "$CHROME" --headless=new --disable-gpu --no-first-run --no-default-browser-check \
    --user-data-dir="$PROFILE" --hide-scrollbars --force-device-scale-factor=1 \
    --window-size=1024,1024 --default-background-color=00000000 \
    --screenshot="$png" "file://$svg" >/dev/null 2>&1 &
  pid=$!
  # 等文件出现并且大小不再变化(连续两次相同),再结束 Chrome。
  for _ in $(seq 1 150); do
    now="$(size_of "$png")"
    [ "$now" -gt 0 ] && [ "$now" = "$last" ] && break
    last="$now"
    sleep 0.2
  done
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  stop_chrome
  local width height
  width="$(sips -g pixelWidth "$png" 2>/dev/null | awk '/pixelWidth/{print $2}')"
  height="$(sips -g pixelHeight "$png" 2>/dev/null | awk '/pixelHeight/{print $2}')"
  if [ "$width" != 1024 ] || [ "$height" != 1024 ]; then
    echo "渲染失败:$svg → ${width:-?}×${height:-?}(应为 1024×1024)" >&2
    return 1
  fi
}

render "$DIR/AppIcon.svg" "$WORK/main.png"
render "$DIR/AppIcon-small.svg" "$WORK/small.png"

ICONSET="$WORK/AppIcon.iconset"
mkdir "$ICONSET"
scale() { # scale <源 png> <边长> <iconset 内文件名>
  sips -z "$2" "$2" "$1" --out "$ICONSET/$3" >/dev/null
}
scale "$WORK/small.png"   16 icon_16x16.png
scale "$WORK/small.png"   32 icon_16x16@2x.png
scale "$WORK/small.png"   32 icon_32x32.png
scale "$WORK/main.png"    64 icon_32x32@2x.png
scale "$WORK/main.png"   128 icon_128x128.png
scale "$WORK/main.png"   256 icon_128x128@2x.png
scale "$WORK/main.png"   256 icon_256x256.png
scale "$WORK/main.png"   512 icon_256x256@2x.png
scale "$WORK/main.png"   512 icon_512x512.png
scale "$WORK/main.png"  1024 icon_512x512@2x.png

iconutil -c icns "$ICONSET" -o "$DIR/AppIcon.icns"

# 自检:反解出来必须还是十张。
iconutil -c iconset "$DIR/AppIcon.icns" -o "$WORK/check.iconset"
count="$(find "$WORK/check.iconset" -name 'icon_*.png' | wc -l | tr -d ' ')"
if [ "$count" != 10 ]; then
  echo "自检失败:.icns 里有 $count 张图,应为 10 张" >&2
  exit 1
fi
echo "已生成 $DIR/AppIcon.icns($(size_of "$DIR/AppIcon.icns") 字节,10 张)"
