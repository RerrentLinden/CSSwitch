# installed-app-launcher 设计注记

## 签名

```text
main.rs::entry(args, app_launch) -> Launcher | Version | Serve | OfficialPassthrough | Gateway
launcher::run() -> ExitCode                       # 0 = 已请求打开控制台;1 = 起不来(已提示)
serve [--port N] [--log-file PATH]
service_log::open(path) -> io::Result<File>       # 追加、0600、路径是符号链接时拒绝
service_log::attach(path) -> Result<(), String>   # fd 1 / 2 接到文件 + 轮转线程
GET /control/status -> {"version": <CARGO_PKG_VERSION>, "mode": …}    # version 是第一个键
bash packaging/build_app.sh [--allow-dirty]
  -> dist/CSSwitch.app、dist/CSSwitch-<版本>-<架构>.dmg

env  CSSWITCH_APP_LAUNCH=1           只由 Info.plist 的 LSEnvironment 设置
     CSSWITCH_APP_HELPER_DIR=<目录>   只供隔离测试替换 open / osascript,生效时写日志
     CSSWITCH_LOOPBACK_BIN=<路径>     让 loopback 层去测指定的二进制(例如应用包里的那个)
```

## 理由与教训

- 正常:双击 → 后台服务起来、浏览器打开控制台、启动器退出;再双击只再开一次控制台。
- 基线:`./csswitch`、不带参数直接运行二进制,行为与引入启动器之前相同;`bash test/run_all.sh`
  不依赖 `packaging/`。
- 错误:让常驻服务做应用主进程(第二次双击没反应);把登录 shell 的全部变量导进服务(旧的
  `ANTHROPIC_*` 跟着进来);为了验证去 `open dist/CSSwitch.app`(用真实 HOME 起真实服务,
  还会打开用户的浏览器)。

## 必需测试

- 单测 `launcher::tests`:标记之前的横幅不被当成变量;白名单之外(凭证、`HOME`、启动器自己的
  变量)不导入;置空的值作为「有」保留;没有标记、或标记后面不是 NUL 时是失败而不是空环境;
  版本比对三态,嵌套的 `sandbox.version` 不算;别的程序的 200 JSON 不算服务在跑。
- 单测 `main::tests`:无参数默认是 `Serve`;显式子命令压过 `CSSWITCH_APP_LAUNCH`;
  `--log-file` 缺路径是错误。
- 单测 `service_log::tests`:用一个代替 fd 1 / 2 的描述符做真实的 `dup2` —— 轮转后**沿用同一个
  描述符号**写出的内容落在新文件、旧内容在 `.1` 且覆盖更早的历史、新文件 0600;文件被外部删掉后
  仍会轮转;已有文件权限被收紧、内容不截断;符号链接被拒绝且目标不变。
- loopback「安装版启动器」一段:以应用包同样的方式执行二进制(无参数 + `CSSWITCH_APP_LAUNCH=1`),
  环境按 Finder 启动的样子给(PATH 只有系统目录),`open` / `osascript` / 登录 shell /
  `claude-science` 全是假的。断言:首次启动拉起且服务脱离启动器(父进程是 1);再次启动 pid 不变、
  日志里只有一次启动横幅且没有抢端口的报错;`service.log` 0600;假 shell 导出的
  `CLAUDE_SCIENCE_BIN` 与 `PATH` 到了服务进程、白名单外的变量没到;登录 shell 卡住时被结束、
  服务照常启动、日志有记录;**真实的 `/bin/zsh`** 读隔离 HOME 里的 `.zshrc` 同样导入成功(假 shell
  只证明管道接得上,这一条证明调用方式在真实 shell 里成立);预先撑过 5 MiB 的日志在服务启动时被
  挪到 `.1`,之后服务写的行落在新文件里;后台是别的版本时提示并只开控制台;端口被占时非零退出、
  提示里有端口、日志路径与系统原始报错。
- 启动器拉起的服务不是测试脚本的子进程:正常路径与 `trap` 都要显式收口(先 `POST /control/quit`,
  再按命令行里唯一的日志路径兜底),跑完用 `pgrep` 确认没有残留。
- 发布侧:`packaging/build_app.sh` 的自检;需要确认包内二进制的运行行为时用
  `CSSWITCH_LOOPBACK_BIN=dist/CSSwitch.app/Contents/MacOS/CSSwitch bash test/run_loopback.sh`。
- 经 Finder 的真实双击、提示框是否看得见、系统放行流程属于真机验收
  (`test/LIVE_ACCEPTANCE.md` §F),离线结论不能替代。

## Wrong vs Correct

### Wrong

```rust
// 用路径猜角色:从终端直接运行包内二进制会意外变成启动器;
// 而且 serve 子进程同样位于 .app 里,少一道保险就会无限拉起自己
if std::env::current_exe()?.to_string_lossy().contains(".app/Contents/MacOS") {
    return launcher::run();
}
```

### Correct

```rust
// 显式变量只在没有参数时生效;显式子命令永远优先
match args.get(1).map(String::as_str) {
    None if app_launch => Entry::Launcher,
    None | Some("serve") => Entry::Serve,
    …
}
// 启动器拉起服务:显式 serve + 移除变量
command.args(["serve", "--port", &port, "--log-file"]).arg(log).env_remove(APP_LAUNCH_ENV);
```

## 应用包与入口

`bash packaging/build_app.sh` 产出的应用包只有三样东西:

```text
CSSwitch.app/Contents/
├── Info.plist             LSUIElement=true(不进 Dock)
│                          LSEnvironment: CSSWITCH_APP_LAUNCH=1
├── MacOS/CSSwitch         csswitch-gateway 二进制本身,包内唯一的可执行文件
└── Resources/AppIcon.icns
```

同一个二进制既是启动器也是服务,由入口区分(`main.rs`):

| 调用方式 | 角色 |
| --- | --- |
| 无参数,且环境里 `CSSWITCH_APP_LAUNCH=1` | 启动器 |
| `launch` | 启动器(同上,供终端与测试显式调用) |
| `--version` | 打印版本号后退出 |
| 无参数 / `serve [--port N] [--log-file PATH]` | 前台服务 |
| `official-passthrough …` 及其余 | 不变 |

`CSSWITCH_APP_LAUNCH` 只由 Info.plist 设置,而且只在没有参数时起作用;启动器拉起服务时显式带
`serve` 并把这个变量从子进程环境里去掉。不用「可执行文件是否在 `.app` 里」来判断:那是隐式约定,
从终端直接运行包内二进制时行为会与预期相反。

## 启动流程与细节

**启动器做完就退出**(`launcher.rs`)。LaunchServices 以「应用主进程是否还活着」判断应用是否在
运行;主进程若就是常驻服务,第二次双击不会再执行任何代码,控制台也就打不开。流程:

```text
1. 端口取自 service.v1.json(默认 8788)
2. GET /control/status(回环,不走代理;连接超时 1 秒,等回话最长 5 秒)
     是 CSSwitch 的状态 ─┬ version 与自己相同 → 打开控制台 → 退出 0
                         └ 不同或没有        → 提示先退出旧服务 → 打开控制台 → 退出 0
     连不上 / 不是 CSSwitch → 继续
3. 读登录 shell 的环境,按白名单并进子进程环境
4. 拉起 <自己> serve --port N --log-file ~/.csswitch/service.log
     stdin=/dev/null;stdout / stderr=日志文件(追加);setsid 脱离启动器
5. 每 200 ms 查一次,最长 10 秒;子进程提前退出即失败
     就绪 → 打开控制台 → 退出 0
     失败 → 提示框(原因、日志路径、日志最后一行)→ 退出 1
```

- **登录 shell 环境**:从 Finder 启动的进程只有 launchd 的环境(PATH 只有系统目录,没有 shell 里
  配置的代理)。网关的上游请求只认环境变量代理,免登录沙箱按 PATH 找本机 `gh` —— 不处理的话,
  安装版会比 `./csswitch` 悄悄少功能。取法是 `$SHELL -l -i -c 'printf "%s\0" <标记>; /usr/bin/env -0'`
  (`SHELL` 缺失时用 `/bin/zsh`,独立会话,5 秒超时即结束),只解析标记之后的内容,只导入
  `PATH`、`HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY` 及其小写形式、`CLAUDE_SCIENCE_BIN`;
  shell 里有就覆盖继承值(置空也算有),没有就保留继承值。`ANTHROPIC_*` 等模型与凭证类变量不导入。
  超时、非零退出、找不到标记都不终止启动:沿用继承环境,并在日志里写明原因。日志只记导入了
  哪些变量名,不记值(代理地址可能带口令)。
- **什么算「服务已在跑」**:状态接口回 200、是 JSON 且有字符串 `mode`。别的程序占着端口时
  不算,启动器照常去拉起服务,由它撞上端口并把系统的原始报错带进提示框。
  没有监听时回环连接立刻被拒,不会等;连上之后给 5 秒,是因为状态接口要先执行一次
  `claude-science status` 才回话,冷启动时可能超过一秒 —— 等得太短会把正在运行的服务判成
  没在跑,随后的拉起必然因端口被占而失败。
- **版本不一致**:服务常驻后台,换了新版应用再双击,连上的仍是旧服务。状态接口带 `version`
  (`CARGO_PKG_VERSION`),启动器比对;不同、或更早的服务根本不报告版本号,都提示用户到控制台
  「退出服务」后重新打开。不自动重启:退出会停掉 Science 与沙箱、打断进行中的会话。
- **打开控制台与提示**用 `/usr/bin/open` 与 `/usr/bin/osascript`(`display alert`,标题与正文经
  argv 传入,不拼进脚本)。提示只出现在两种情况:服务起不来、版本不一致;提示框 120 秒无人
  理会自动消失,启动器不会一直挂着。`CSSWITCH_APP_HELPER_DIR` 可把这两个命令换成别的目录里的
  同名文件,只供隔离测试使用,生效时写日志。
- **日志落盘**(`service_log.rs`):带 `--log-file` 的服务把自己的 stdout / stderr 接到该文件
  (追加,0600,路径是符号链接时拒绝),并起一个后台线程每 60 秒检查一次正在写的那个文件,超过
  5 MiB 就改名为 `service.log.1`(覆盖上一份)、0600 新建同名文件、`dup2` 到 fd 1 / 2。
  所以上限是「5 MiB + 一分钟的写入量」再加一份历史。轮转必须在服务进程内做:fd 重定向之后,
  外部改名不会改变进程正在写的文件。启动器在 exec 之前也把这两个 fd 接到同一个文件,
  参数解析失败、panic 这类最早期的输出不会丢。不带 `--log-file` 时这些都不发生。
- **版本号**只有一个来源:`desktop/gateway/Cargo.toml`。它流向 `--version`、状态接口的
  `version`、控制台页眉,以及构建脚本写进 Info.plist 的两个版本键。

应用是 ad-hoc 签名(`codesign --sign -`),bundle id `io.github.rerrentlinden.csswitch`,架构跟随
构建机。没有 Developer ID 签名与公证,经下载分发时首次打开需要用户在系统设置里放行。
