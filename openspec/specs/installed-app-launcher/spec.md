# installed-app-launcher Specification

## Purpose

安装版是在 `./csswitch` 之外新增的一种启动方式：双击应用 → 后台服务起来 → 浏览器打开控制台。服务本身（`serve`）两者共用，
脚本形态的行为不因安装版而改变。本规格约束应用包的构成、启动器的入口判定与行为、登录 shell 环境导入、日志落盘与轮转、构建脚本与图标。
改动 `launcher.rs`、`service_log.rs`、`main.rs` 的入口判定、状态接口的 `version`，或 `packaging/` 下的构建脚本、Info.plist 模板、图标时适用。

## Requirements

### Requirement: 应用包里只有一个可执行文件

应用包里 MUST 只有一个可执行文件，它就是网关二进制；启动器与服务是同一个文件的两个角色。
MUST NOT 加 shell 脚本主可执行文件（签名存在扩展属性里，拷贝易丢）、Swift 壳、托盘 / Dock 图标 / 原生窗口。
界面只有浏览器里的控制台，退出只有控制台的「退出服务」。

#### Scenario: 检查应用包

- **WHEN** 检查构建出的 `CSSwitch.app`
- **THEN** 包内只有网关二进制一个可执行文件，没有 Dock 图标（`LSUIElement`）

### Requirement: 启动器干完就退

启动器 MUST 干完就退：LaunchServices 以「应用主进程是否还活着」判断应用是否在运行，主进程若常驻，第二次双击不会再执行任何代码。
提示框 MUST NOT 无限期地挂住启动器（`giving up after`）。`launcher::run()` 的退出码：0 = 已请求打开控制台；1 = 起不来（已提示）。

#### Scenario: 首次双击

- **WHEN** 用户首次双击应用
- **THEN** 后台服务起来并脱离启动器（父进程是 1），浏览器打开控制台，启动器退出

#### Scenario: 再次双击

- **WHEN** 服务已在跑时再次双击
- **THEN** 只再开一次控制台；服务 pid 不变，日志里只有一次启动横幅，没有抢端口的报错

### Requirement: 入口靠显式环境变量

入口 MUST 靠显式的环境变量，MUST NOT 靠路径嗅探。`CSSWITCH_APP_LAUNCH=1`（只由 Info.plist 的 `LSEnvironment` 设置）只在**没有参数**时
把二进制变成启动器；显式子命令永远优先。启动器拉起服务时 MUST 显式带 `serve` 并移除这个变量——两道保险少了任何一道，
服务都可能再次判成启动器、无限拉起自己。无参数且没有这个变量时 MUST 仍是前台 `serve`（`./csswitch` 与 loopback 都依赖它）。

入口判定：`main.rs::entry(args, app_launch) -> Launcher | Version | Serve | OfficialPassthrough | Gateway`。

#### Scenario: 无参数且带启动变量

- **WHEN** 二进制无参数运行且 `CSSWITCH_APP_LAUNCH=1`
- **THEN** 它是启动器

#### Scenario: 无参数且没有启动变量

- **WHEN** 二进制无参数运行且没有该变量
- **THEN** 它是前台 `serve`，行为与引入启动器之前相同

#### Scenario: 显式子命令

- **WHEN** 带显式子命令运行，同时 `CSSWITCH_APP_LAUNCH=1`
- **THEN** 子命令优先

#### Scenario: 启动器拉起服务

- **WHEN** 启动器拉起服务进程
- **THEN** 命令行显式带 `serve`，环境里没有 `CSSWITCH_APP_LAUNCH`

### Requirement: 服务已在跑的判据

「服务已在跑」的判据 MUST 是状态接口回 200、JSON、带字符串 `mode`，不是「端口连得上」。
别的程序占着端口时 SHALL 照常去拉起服务，由它撞上端口，把系统的原始报错带进提示框。
探测 MUST 走回环且显式不走代理（用户环境里的 `HTTP_PROXY` 会把 127.0.0.1 的请求带到代理上）。
连接超时 1 秒；连上之后等回话 5 秒：状态接口要先执行 `claude-science status`，不是纯回环往返。

#### Scenario: 端口被别的程序占着

- **WHEN** 端口被别的程序占着（哪怕它回 200 JSON）
- **THEN** 不当成服务在跑；拉起的服务撞端口退出 → 提示（原因、日志路径、日志最后一行，含端口与系统原始报错）→ 退出 1

#### Scenario: 用户环境里有 HTTP_PROXY

- **WHEN** 环境里有 `HTTP_PROXY`
- **THEN** 探测仍直连回环，不经代理

### Requirement: 版本不同时提示而不重启

版本 MUST 只有一个来源（`Cargo.toml`）。`GET /control/status` 返回 `{"version": <CARGO_PKG_VERSION>, "mode": …}`，`version` 是第一个键。
启动器拿状态接口的 `version` 与自己比：不同、或没有这个字段（更早的服务），都 SHALL 先提示再打开控制台，MUST NOT 自动重启——
退出服务会停掉 Science 与沙箱、打断进行中的会话，必须由用户决定。状态里嵌套的 `sandbox.version` 是 Science 的版本，MUST NOT 拿来比。

#### Scenario: 服务已在跑且版本相同

- **WHEN** 服务已在跑，版本与启动器相同
- **THEN** 只打开控制台，退出 0；不拉起、不提示

#### Scenario: 版本不同或没有 version

- **WHEN** 服务已在跑，版本不同或状态里没有 `version`
- **THEN** 先提示，提示关掉后打开控制台，退出 0；不重启、不拉起

#### Scenario: 嵌套的 sandbox.version

- **WHEN** 状态里只有嵌套的 `sandbox.version` 与启动器版本不同
- **THEN** 它不参与版本比对

### Requirement: 登录 shell 环境按白名单导入

登录 shell 环境 MUST 按白名单导入：`PATH`、`HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY` 及其小写形式、`CLAUDE_SCIENCE_BIN`。
只解析标记之后的内容（rc 文件会打印横幅）；shell 里有就覆盖继承值（置空也算有），没有就保留继承值。
`ANTHROPIC_*` 等模型与凭证类变量 MUST 永不导入。取不到（超时、非零退出、没有标记）是**显式降级**：写日志、沿用继承环境、照常启动。
日志 MUST 只记变量名，不记值（代理地址可能带口令）。shell 起在独立会话里，超时按进程组结束。

#### Scenario: shell 导出了白名单内的变量

- **WHEN** 登录 shell 导出了 `CLAUDE_SCIENCE_BIN` 与 `PATH`
- **THEN** 它们到了服务进程

#### Scenario: 白名单之外的变量

- **WHEN** 登录 shell 里有凭证、`HOME` 或启动器自己的变量
- **THEN** 它们不被导入

#### Scenario: rc 文件打印横幅

- **WHEN** 标记之前有横幅输出
- **THEN** 横幅不被当成变量

#### Scenario: 置空的值

- **WHEN** shell 里某个白名单变量被置空
- **THEN** 置空的值作为「有」保留，覆盖继承值

#### Scenario: 登录 shell 取不到环境

- **WHEN** 登录 shell 超时、非零退出，或输出里没有标记（或标记后面不是 NUL）
- **THEN** 那个 shell 被结束，日志写原因，沿用继承环境照常启动，而不是当成空环境

### Requirement: 安装版日志落盘与轮转

`serve [--port N] [--log-file PATH]`。安装版日志 MUST 落在 `~/.csswitch/service.log`（`0600`）。
`service_log::open` 追加打开、`0600`、路径是符号链接时拒绝；`service_log::attach` 把 fd 1 / 2 接到文件并起轮转线程。
轮转 MUST 在服务进程内做：每 60 秒按**正在写的那个文件**量大小，超过 5 MiB → 改名 `.1` → `0600` 新建 → `dup2` 到 fd 1 / 2。
fd 重定向之后外部改名不改变进程在写的文件；按路径量大小，文件被外部删掉后就永远「没超限」。
`log_line!` / `request_log_line!` 不改；不带 `--log-file` 时服务行为 MUST 与没有这个参数时相同。

#### Scenario: 日志超过上限

- **WHEN** 日志超过 5 MiB
- **THEN** 旧内容挪到 `.1`（覆盖更早的历史），之后服务写的行落在新文件里，新文件是 `0600`

#### Scenario: 日志文件被外部删掉

- **WHEN** 轮转时原路径已不存在
- **THEN** 不算错，仍会轮转，照样在原路径另起新文件

#### Scenario: 已有日志文件

- **WHEN** 日志文件已存在
- **THEN** 权限被收紧到 `0600`，内容不截断

#### Scenario: 日志路径是符号链接

- **WHEN** 日志路径是符号链接
- **THEN** 打开被拒绝，链接目标不变

#### Scenario: --log-file 后面没有路径

- **WHEN** `--log-file` 后面没有路径
- **THEN** 报错并以 2 退出，不悄悄退回 stderr

#### Scenario: 日志文件打不开

- **WHEN** 启动器要用的日志文件打不开
- **THEN** 提示，退出 1

### Requirement: 提示与打开控制台的方式

提示与打开控制台 MUST 只用 `/usr/bin/open` 与 `/usr/bin/osascript`。提示的标题与正文 MUST 经 argv 传给脚本，MUST NOT 拼进 AppleScript 源码
（正文带着日志的最后一行，内容不受控）。正常启动 MUST NOT 弹任何东西。
`CSSWITCH_APP_HELPER_DIR=<目录>` 只供隔离测试替换 `open` / `osascript`，生效时写日志。

#### Scenario: 服务 10 秒内没有就绪

- **WHEN** 拉起的服务 10 秒内没有就绪
- **THEN** 结束它，提示，退出 1

#### Scenario: open 失败

- **WHEN** 服务已在跑而 `open` 失败
- **THEN** 提示标题说明服务已启动、只是没打开控制台，正文给出地址，退出 1；不附日志最后一行（与此无关）

#### Scenario: 正常启动

- **WHEN** 启动一切正常
- **THEN** 不弹任何提示

### Requirement: 构建脚本只写 dist 且拒绝脏工作区

`bash packaging/build_app.sh [--allow-dirty]` 产出 `dist/CSSwitch.app` 与 `dist/CSSwitch-<版本>-<架构>.dmg`。它 MUST 只写 `dist/`，MUST NOT 碰 `/Applications`。
工作区有任何未提交改动（含未跟踪文件——它们同样会被编进产物）时 MUST 拒绝，除非显式 `--allow-dirty`。
自检任一项不过 MUST 非零退出并删掉这次的半成品。它 MUST NOT 并入 `test/run_all.sh`：三层门禁的合同是离线、快，release 构建与 DMG 是发布侧的独立结论。
签名是 ad-hoc，bundle id `io.github.rerrentlinden.csswitch`（不沿用上游的 `com.csswitch.menubar`：不是同一个应用，不共享系统里按 bundle id 记的状态）。

#### Scenario: 工作区不干净

- **WHEN** 工作区有未提交改动且没有 `--allow-dirty`
- **THEN** 构建被拒绝

#### Scenario: 自检失败

- **WHEN** Info.plist 版本、`LSEnvironment`、`LSUIElement`、架构、图标、签名任一不对
- **THEN** 自检失败，非零退出，半成品被删除

#### Scenario: 三层门禁与打包相互独立

- **WHEN** 运行 `bash test/run_all.sh`
- **THEN** 它不依赖 `packaging/`

### Requirement: 图标来源

图标 MUST 走 `packaging/icon/` 里 `gen.py` → 三张 SVG → `build_icon.sh` → `AppIcon.icns`，全部入库；应用构建只拷贝 `.icns`，不依赖 Chrome。
16 / 32 px 三档取自简化稿，其余取自主稿。控制台里的内联图形是 `console-glyph.svg` 的拷贝，改图后 MUST 同步。

#### Scenario: 构建应用

- **WHEN** 运行应用构建
- **THEN** 构建只拷贝入库的 `AppIcon.icns`，不调用 Chrome

#### Scenario: 改了图形

- **WHEN** `gen.py` 的图形被修改
- **THEN** 三张 SVG、`AppIcon.icns` 与控制台里的内联图形一并更新
