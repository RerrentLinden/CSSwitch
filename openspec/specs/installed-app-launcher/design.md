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
