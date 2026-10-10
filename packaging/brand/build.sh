#!/bin/bash
# 由 social-preview.html 渲染 docs/assets/social-preview.png(1280×640,入库)。
#
#   bash packaging/brand/build.sh
#
# 页面引用 ../icon/AppIcon.svg,所以图标改了之后宣传图也要重出。
# 文案只写产品现在真有的东西:改之前先对一遍 README。
# 与 ../icon/build_icon.sh 同样用无头 Chrome 渲染,渲染完主动结束它;
# Chrome 不在默认位置时用 CHROME=<可执行文件路径> 指定。
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT="$DIR/../../docs/assets/social-preview.png"
CHROME="${CHROME:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"
[ -x "$CHROME" ] || { echo "env-blocked: 找不到 Google Chrome($CHROME)" >&2; exit 3; }

WORK="$(mktemp -d)"
PROFILE="$WORK/chrome-profile"
stop_chrome() { pkill -f -- "--user-data-dir=$PROFILE" 2>/dev/null || true; }
cleanup() { stop_chrome; rm -rf "$WORK"; }
trap cleanup EXIT

size_of() { stat -f '%z' "$1" 2>/dev/null || echo 0; }

PNG="$WORK/social-preview.png"
"$CHROME" --headless=new --disable-gpu --no-first-run --no-default-browser-check \
  --user-data-dir="$PROFILE" --hide-scrollbars --force-device-scale-factor=1 \
  --window-size=1280,640 --screenshot="$PNG" "file://$DIR/social-preview.html" >/dev/null 2>&1 &
pid=$!
# 无头 Chrome 写完截图后不会自己退出:等文件大小稳定下来再结束它。
last=-1
for _ in $(seq 1 150); do
  now="$(size_of "$PNG")"
  [ "$now" -gt 0 ] && [ "$now" = "$last" ] && break
  last="$now"
  sleep 0.2
done
kill "$pid" 2>/dev/null || true
wait "$pid" 2>/dev/null || true
stop_chrome

width="$(sips -g pixelWidth "$PNG" 2>/dev/null | awk '/pixelWidth/{print $2}')"
height="$(sips -g pixelHeight "$PNG" 2>/dev/null | awk '/pixelHeight/{print $2}')"
if [ "$width" != 1280 ] || [ "$height" != 640 ]; then
  echo "渲染失败:${width:-?}×${height:-?}(应为 1280×640)" >&2
  exit 1
fi
mkdir -p "$(dirname "$OUT")"
cp "$PNG" "$OUT"
echo "已生成 docs/assets/social-preview.png(${width}×${height})"
