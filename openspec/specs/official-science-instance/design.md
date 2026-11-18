# official-science-instance 设计注记

## 签名

```text
science::start(base_url, &LaunchOverrides)
claude-science serve [--config <镜像>] --port 8686 --sandbox-port 8687 --detached --no-browser
                                                  # 全部启动参数,不多不少
  env: ANTHROPIC_BASE_URL=http://127.0.0.1:<port> # 唯一注入的变量
  env_remove: ANTHROPIC_MODEL / *_MODEL / *_MODEL_NAME
              ANTHROPIC_API_KEY / ANTHROPIC_AUTH_TOKEN
claude-science stop | status | url
配置: ~/.csswitch/service.v1.json (0600)

science::status() -> {"installed", "running", "detail": <status 输出原文>, "binary"}
science::unmanaged_reason(&Value) -> Option<&'static str>   # "app" | "port";输入是 status() 的结果
control::science_activity(&Value) -> (running, active)       # active = 对话数,为 0 时取 frame 数
control::active_sessions_refusal(active)   # 「有 N 个会话正在进行,…」;控制台靠「会话正在进行」转确认框
GET /control/status → science.unmanaged = {"reason": "app" | "port"}   # 未命中时没有这一项
```

`status` 输出里与本文有关的字段(0.1.72):顶层 `running`、`port`;`daemon.launch_context`、
`daemon.active_frames`、`daemon.active_conversations`。

`LaunchOverrides` 与镜像配置的签名见 [science-launch-overrides](../science-launch-overrides/design.md)。

## 理由与教训

- 错误：为"隔离干净"传 `--data-dir`——用户打开后发现对话全没了。
- 退出服务要先停实例：网关一走，指向它的实例只剩断掉的推理链路。
- `--config` 只换配置文件、不换数据目录，是 2026-10-10 用 Science 0.1.60 隔离实测的结论。
- 错误：切换模式先存盘、再判断有没有会话。被拒绝时新模式已经写进配置（规格写的是「配置不变」，代码没做到）。
  2026-11-18 起渠道校验与忙闲判定都在存盘之前，loopback 用配置文件逐字节比较钉住。
- 错误：忙闲只看 `active_conversations`。子 agent、Reviewer 这类只有 frame 在跑的时候会被判成空闲，
  切换不经确认就重启 Science、打断它们。依据是对 Science 0.1.72 的静态分析：Science 自己判断能否替换旧 daemon
  看的是 frame 数。报给用户的数字仍取对话数（一个对话可以有好几个 frame），为 0 时才取 frame 数，不说「有 0 个会话」。

## 归属识别的由来与证据

问题：正在运行的 Science 不是经 CSSwitch 启动时，它的环境里没有网关地址，流量不经过 CSSwitch；
它自己重启（看门狗、自动更新）也沿用原来的环境与参数。此前控制台照样显示「Science 运行中」与所选模式，没有任何提示。

两个信号的来历不同，都保留：

- **端口**（`port` 不是 8686）：经 CSSwitch 启动的固定在 8686。2026-11-18 晚间的真机观察：机器重启并登录后，
  Claude Science 的桌面 App 自己拉起了 daemon，进程命令行是 `claude-science serve --app`，`status` 报告 `port: 8765`、
  `daemon.launch_context: "launchd"`（不是 `app-child`）、`daemon.skip_approvals: false`；这个 daemon 没有带
  CSSwitch 的网关地址。当时在跑的旧安装版 0.11.0 的控制台仍显示「Science 运行中」和 Kimi 模式，没有任何提示。
  把这份 status 输出原样喂给 0.12.0 的构建（除版本号外代码与 `main@0513908` 相同；隔离的临时 HOME，status 由包装脚本转述），
  `/control/status` 给出 `science.unmanaged = {"reason": "port"}`。这个真实场景是靠端口认出来的。
  手动 `claude-science serve` 不带 `--port` 时落在 Science 默认的 8000，同样靠端口认出。
- **`launch_context == "app-child"`**：依据是对 Science 0.1.72 的静态分析——以 `--stdio` 启动时报这个值。
  真机上还没见过；上面那次桌面 App 拉起的 daemon 报的是 `launchd`，`app-child` 没有出现。
  所以不能把「桌面 App 拉起」等同于 `app-child`。原因值叫 `app` 只是沿用代码里的命名（`science.rs` 的注释仍写作
  「桌面 App 拉起的」），不表示桌面 App 拉起的 daemon 都会报它。

`launch_context` 单看分不出归属：安装版 CSSwitch 启动的实测报 `launchd`，从终端跑 `./csswitch` 报 `terminal`，
而上面那次桌面 App 拉起的同样报 `launchd`。判定里它只用来认 `app-child`，其余靠端口。

已知漏报：不经 CSSwitch、但手动用 `serve --port 8686` 起的 daemon；以及落在 8686 又不报 `app-child` 的其它拉起方式。
字段缺失时不判，宁可漏报，也不把经 CSSwitch 启动的误报成别人的。

只从已有的 `status` 输出推导，不新增状态文件，也不自动重启 Science：提示用户先「停止」再「启动 Science」。

2026-11-18 的隔离验收（真实控制台，`claude-science` 的 status 输出用替身）：报 `app-child` 或其它端口时状态接口带
`unmanaged`、控制台出现提示；报 8686 + `launchd` 时不出现；把一台经 CSSwitch 启动的真实 0.1.72 实例的 status 输出
原样喂进来，不误报；只有 frame 在跑时，切换模式与联网搜索开关都被拒并弹出确认，配置文件哈希不变。
逐项记录在 [test/LIVE_ACCEPTANCE.md](../../../test/LIVE_ACCEPTANCE.md) §K。

## 必需测试

- `science::tests::model_env_clear_list_covers_auth_and_role_overrides`：断言清除列表
  覆盖模型与凭证变量，且 **`ANTHROPIC_BASE_URL` 不在其中**（它是要注入的）。
- `science::tests::official_serve_args_pin_ports_and_never_isolate`：无镜像时参数向量与字面量
  逐项相等；有镜像时 `--config <镜像>` 恰好一次、去掉这一对后与无镜像时相同；`--data-dir`、
  `--no-auto-update` 两种状态下都不出现。这条测试是官方模式的冻结线，改启动方式时不改它。
- `science::tests::a_daemon_not_launched_by_csswitch_is_told_apart_from_status_alone`：8686 + `launchd` /
  `terminal` / `unknown` 不判；8000 → `port`；`app-child`（端口对得上、端口缺失、端口也不符）→ `app`；
  字段缺失、端口是字符串、`detail` 为 null、未运行时不判。
- `control::tests::science_activity_reads_the_real_status_shape`：计数从 `detail.daemon` 下读；读错路径时会话数恒为 0。
- `control::tests::running_frames_alone_count_as_sessions_in_progress`：只有 frame（3 个 frame、0 个对话）→ 需要确认，
  数字是 3；两项都有 → 报对话数；两项都为 0 → 直接重启。
- loopback 层断言配置文件权限为 `0600`、且未写出 `~/.csswitch/config.json`。
- loopback 层用假 `CLAUDE_SCIENCE_BIN` 与命令行同形的假沙箱进程，断言退出服务停掉两个实例且进程退出；
  loopback 全程不得解析到本机真实 `claude-science`。
- loopback「正在跑的 Science 是不是经 CSSwitch 启动的」：默认不带 `unmanaged`；端口 8000 → `port`，且状态里仍只有一个
  `mode` 键；再报 `app-child` → `app`；端口恢复 8686 而仍是 `app-child` → `app`；全部恢复后不再带。
- loopback「只有 frame 在跑也算有会话正在进行」：切换模式被拒（文案含「有 3 个会话正在进行」），模式不变、
  Science 没有被重启、配置文件逐字节不变、没有生成镜像；带 `force` 后放行并重启。
- 假 `claude-science` 的 `status` 输出形状必须与真实一致，否则测的是假设：
  `{"running":true,"pid":…,"version":"0.1.72","port":8686,"started_at":…,"daemon":{"boot":"serving","launch_context":"launchd","active_frames":N,"active_conversations":N}}`。

## Wrong vs Correct

### Wrong

```rust
// 为了"隔离"与"稳定"，把用户的实例改造成另一个实例
&["serve", "--port", "0", "--detached", "--no-browser",
  "--data-dir", &managed_dir, "--no-auto-update"]
```

```rust
// 先存盘再判忙,而且只看对话数
profile.mode = mode;
profile.save()?;
if running && active_conversations > 0 && !forced {
    return Err(active_sessions_refusal(active_conversations));
}
```

### Correct

```rust
// 只在启动时注入，其余一律沿用用户自己的实例
&["serve", "--port", "8686", "--sandbox-port", "8687", "--detached", "--no-browser"]
// env: ANTHROPIC_BASE_URL=<gateway>
// 联网搜索关闭、或实例处于第三方模式时,在 serve 之后多一对:"--config", <CSSwitch 生成的镜像>
// 跳过审批总开关开着时,末尾多一个:"--dangerously-skip-approvals"
```

```rust
// 判定在存盘之前;对话数或 frame 数任一大于 0 都算忙
let (running, active) = science_activity(&crate::science::status());
if running && active > 0 && !forced {
    return Err(active_sessions_refusal(active));
}
profile.mode = mode;
profile.save()?;
```
