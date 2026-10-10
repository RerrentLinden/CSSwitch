# 发布规则

- 发布固定到干净 commit。`bash packaging/build_app.sh` 从同一 commit 产出
  `dist/CSSwitch.app` 与 `dist/CSSwitch-<版本>-<架构>.dmg`;工作区有任何未提交的改动
  (含未跟踪文件)时它会拒绝。`--allow-dirty` 只供开发自测,那样的产物不得发布。
- 应用包里只有三样东西:`Info.plist`、`MacOS/CSSwitch`(服务二进制本身,由
  `LSEnvironment` 的 `CSSWITCH_APP_LAUNCH=1` 进入启动器角色)、`Resources/AppIcon.icns`。
  DMG 根目录恰好是应用与指向 `/Applications` 的快捷方式。
- 版本号只改 `desktop/gateway/Cargo.toml`(`Cargo.lock` 随之更新);Info.plist、`--version`、
  状态接口与控制台页眉都从它来,不在别处手写。
- 构建脚本的自检(Info.plist 合法、三处版本一致、架构、图标、包内只有一个可执行文件、
  `codesign --verify --strict`、`hdiutil verify`、只读挂载核对 DMG 内容)全部通过才算构建成功;
  中途失败时脚本会删掉这次的半成品。报告里写明 commit、产物路径、大小与 SHA-256。
- 需要确认包内二进制的运行行为时,对它跑同一套 loopback,不要用 `open` 去启动应用:
  `CSSWITCH_LOOPBACK_BIN=dist/CSSwitch.app/Contents/MacOS/CSSwitch bash test/run_loopback.sh`。
  经 Finder / `open` 启动会用真实 HOME 起真实服务并打开用户的浏览器,那是用户执行的真机验收。
- 未经用户明确授权,不覆盖 `/Applications/CSSwitch.app`,不 commit / push / tag /
  发 release。构建或测试成功不构成这些授权。
- 分开记录:门禁三层结果、app 构建自检、真机验收(未执行就写「未执行」)。签名与公证是另一层
  结论:产物是 ad-hoc 签名,不是 Developer ID 签名,更不是公证;经下载分发时首次打开需要用户
  在系统设置里放行,README 必须写明这一点。
- 图标改动后重新生成并一起提交:`python3 packaging/icon/gen.py packaging/icon` →
  `bash packaging/icon/build_icon.sh`(需要 Google Chrome;应用构建本身只拷贝入库的 `.icns`)。
- README 与实际形态必须一致才算发布闭环(尤其是安装与使用方式、补偿清单)。
