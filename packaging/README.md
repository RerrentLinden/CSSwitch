# 打包

把 CSSwitch 打成可以拖进「应用程序」的 macOS 应用。设计与合同见
[docs/ARCHITECTURE.md](../docs/ARCHITECTURE.md)「安装版与启动器」,发布规则见
[.agents/rules/release.md](../.agents/rules/release.md)。

## 构建

```bash
bash packaging/build_app.sh                 # 发布用:工作区必须干净
bash packaging/build_app.sh --allow-dirty   # 开发自测:允许带着未提交的改动
```

产物在 `dist/`(不入库):

| 文件 | 内容 |
| --- | --- |
| `dist/CSSwitch.app` | `Info.plist` + `MacOS/CSSwitch`(网关二进制本身)+ `Resources/AppIcon.icns` |
| `dist/CSSwitch-<版本>-<架构>.dmg` | 应用 + 指向 `/Applications` 的快捷方式 |

脚本依次做:找工具链并检查工作区 → `cargo build --release --locked` → 组装应用包 →
ad-hoc 签名 → 自检 → 组装 DMG 并只读挂载核对 → 打印路径、大小与 SHA-256。
任一步自检不过就以非零退出,并删掉这次的半成品。

- 版本号只来自 `desktop/gateway/Cargo.toml`;最低系统版本取自二进制自己的 `LC_BUILD_VERSION`。
  两者由脚本填进 [Info.plist.in](Info.plist.in)。
- 架构跟随构建机。通用二进制、Intel 包不在这里做。
- 签名是 ad-hoc:不是 Developer ID 签名,也没有公证。
- 脚本只写 `dist/`,不安装到 `/Applications`,也不会去打开应用。

### 验证包内二进制

不要用 `open dist/CSSwitch.app` 来试:那会用真实的 HOME 起一个真实服务,并打开你的浏览器。
要确认包内二进制的运行行为(含启动器),让它过同一套 loopback 断言,全程隔离 HOME、假命令:

```bash
CSSWITCH_LOOPBACK_BIN="$PWD/dist/CSSwitch.app/Contents/MacOS/CSSwitch" bash test/run_loopback.sh
```

经 Finder 的真实双击属于真机验收,见 [test/LIVE_ACCEPTANCE.md](../test/LIVE_ACCEPTANCE.md) §F。

## 图标

[icon/](icon/) 里的文件全部入库;应用构建只拷贝 `AppIcon.icns`,不需要 Chrome。

| 文件 | 说明 |
| --- | --- |
| `gen.py` | 图形的唯一来源:按参数画出双螺旋与开关,输出下面三张 SVG |
| `AppIcon.svg` | 主稿,用于 64 px 及以上 |
| `AppIcon-small.svg` | 简化稿,用于 16 / 32 px(线更粗、开关更大、去掉普通碱基对) |
| `console-glyph.svg` | 控制台页眉与页签图标用的精简版,内联在 `desktop/gateway/ui/index.html` 里 |
| `build_icon.sh` | 两张 SVG → 十张 PNG → `AppIcon.icns` |
| `AppIcon.icns` | 生成结果 |

重新生成:

```bash
python3 packaging/icon/gen.py packaging/icon   # 改了 gen.py 的参数之后
bash packaging/icon/build_icon.sh              # 需要 Google Chrome(无头渲染 SVG)
```

`build_icon.sh` 用一个临时的 Chrome profile 渲染,渲染完会主动结束那个无头 Chrome
(它写完截图后不会自己退出),不影响正在使用的浏览器。Chrome 不在默认位置时用
`CHROME=<可执行文件路径>` 指定。

`.icns` 里的十张图:`icon_16x16`、`icon_16x16@2x`、`icon_32x32` 取自简化稿,
`icon_32x32@2x`(64 px)及以上取自主稿。

改了图形之后,把新的 `console-glyph.svg` 内容同步到 `index.html` 页眉里那份内联 SVG
(保留 `class="logo" id="logo"` 与宽高属性,去掉 `xmlns`)。

## 宣传图

[brand/](brand/) 里是 `docs/assets/social-preview.png`(1280×640,README 页首)的源文件:

```bash
bash packaging/brand/build.sh   # 需要 Google Chrome
```

页面直接引用 `icon/AppIcon.svg`,图标改了之后要重出。文案只写产品现在真有的东西。
GitHub 仓库设置里的 social preview 需要在网页上手动上传,脚本管不到。
