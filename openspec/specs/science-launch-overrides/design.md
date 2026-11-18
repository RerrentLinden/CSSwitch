# science-launch-overrides 设计注记

## 签名

```text
science_config::LaunchOverrides { web_search: bool, default_model: Option<String>, skip_approvals: bool }
science_config::launch_config(&overrides, source, mirror) -> Result<Option<PathBuf>, String>
  source(只读): ~/.claude-science/config.toml
  mirror(0600): ~/.csswitch/science-config/official.toml
control::launch_overrides(profile, instance) -> LaunchOverrides   # 五个启动点唯一的取值处
POST /control/web-search {"enabled": bool, "force"?: bool}
POST /control/skip-approvals {"enabled": bool, "force"?: bool}   # 与上一条同一个 set_launch_switch
control::plan_launch_switch(current, target, running, active, force) -> LaunchSwitchPlan   # 纯判定
control::science_activity(&status) -> (running, active)   # 与切换模式共用的忙闲判定
```

沙箱一侧：source 是 `<沙箱 data-dir>/config.toml`（只读），mirror 是 `~/.csswitch/science-config/sandbox.toml`，
见 [sandbox-science-instance](../sandbox-science-instance/spec.md)。

## 理由与教训

- 错误：为了关闭联网搜索去改写 `~/.claude-science/config.toml`——坏文件会留在 Science 自己的
  目录里，不经 CSSwitch 也起不来；开启时还要删键，可能覆盖别人手写的 `false`。
- 重启失败不回滚：回滚再重启可能循环。

## 必需测试

- `science_config::tests`：断言一律**用 TOML 解析器把镜像读回来**比较，不拿实现里的字符串当预期。
  覆盖：四格表（无覆盖时不写文件并清残留）；无源文件；含根键 / 注释 / 表 / 行内表 / 多行字符串 /
  表数组的源（除覆盖的键外逐值相等、键在根表）；源里原本就有 `enable_web_search = true` 或自己的
  `default_model`（被覆盖，不得出现重复键）；源无法解析（报错、不写镜像、报错不含文件内容）；
  源读不了不等于空配置；源目录零写入；权限 `0600` / `0700`。
- `control::tests`：切换判定纯函数的四条分支（`launch_switch_never_interrupts_sessions_without_consent`）；
  只有 frame 在跑时同样需要确认、数字取 frame 数（`running_frames_alone_count_as_sessions_in_progress`）；
  请求体的 `enabled` / `force` 必须是布尔；
  `launch_overrides` 对两个实例各取各的模式（正式实例官方、沙箱走渠道 → 正式实例 `None`、沙箱
  `claude-sonnet-19`；把沙箱写成取 `profile.mode`，或把正式实例写成取 `sandbox_mode`，这条会红）。
- loopback 层的启动覆盖项：官方模式 + 开启态 `serve` 整行等于主线；官方模式 + 关闭态整行带
  `--config <镜像>`，镜像在根表含 `enable_web_search` 并保留临时 HOME 里源配置的其它键，源目录不变；
  官方模式下重新开启后整行回到主线、镜像被删。第三方模式下整行总是带 `--config`，镜像含
  `default_model`，开关联网搜索只增减 `enable_web_search`；切回官方模式后镜像被删。
  活跃会话未确认、源配置无法解析两种情况下配置文件逐字节不变且没有 stop / serve。

## 联网搜索总开关

`service.v1.json` 顶层的 `web_search`(默认 `true`)是唯一真相,全局生效、不分渠道。
它对应 Science 0.1.56 起的实例级选项 `enable_web_search`:该选项写在 Science 的
`config.toml` 根表,只在启动时读取,没有应用内设置项也没有环境变量。一个值驱动两层:

```text
service.v1.json: web_search
        │
        ├─ Science 一侧(启动时一次性,见「启动覆盖项」)
        │     true  → 不覆盖这个键
        │     false → 镜像配置里写 enable_web_search = false
        │
        └─ 网关一侧(每请求读快照,config.rs / server.rs)
              true  → Kimi 的 typed web_search 走 query-tool bridge
              false → relay 入口摘除 typed web_search,桥不触发
```

**生效时机**:网关一侧存盘后下一次请求即生效,且不依赖 Science 是否已重启、是否认识这个选项。
Science 一侧要重启:`POST /control/web-search` 在官方实例运行时会重启它,有会话正在进行且未带
`force` 时拒绝并且不改任何状态(「有会话正在进行」是对话数或 frame 数任一大于 0,与切换模式、跳过审批同一个
`science_activity`;2026-11-18 起才把 frame 数算进来,依据见 [official-science-instance](../official-science-instance/design.md));镜像生成不出来同样在存盘之前报错。重启失败不回滚,原文报出
(此时开关已改、Science 未按新设置运行;拨回开启再启动就是原始启动方式)。免登录沙箱不自动重启,
响应里用 `sandbox_restart_required` 标出。

官方模式的零改写直通不变:该模式下的关闭效果完全来自 Science 一侧。DeepSeek 平时把搜索声明
留给上游原生执行,关闭态同样在入口被摘除。

**回滚注意**:旧版本把这个开关存在渠道里(`kimi.web_search`)。新版本读到「顶层没有该键且
`kimi.web_search == false`」时迁移为总开关关闭,保存后只写顶层键。因此代码回滚到旧版本后,
旧二进制读不到渠道级旧键,会把「关」读成「开」。

## 启动覆盖项(镜像配置)

Science 只在启动时读 `config.toml`,而且有两件事只能靠它告诉 Science:关掉联网搜索、新会话默认用
哪个模型。CSSwitch 不改 Science 自己的文件:需要覆盖时,把「源配置 + 覆盖的键」写成一份镜像,
启动时用 `--config <镜像>` 交给 Science(`science_config.rs`)。

| 覆盖的键(都在根表) | 什么时候写 | 值 |
| --- | --- | --- |
| `enable_web_search` | 联网搜索总开关关闭 | `false` |
| `default_model` | 这个实例处于第三方模式(Kimi / DeepSeek) | 默认槽所在的菜单条目 id,见 [model-menu](../model-menu/design.md)「模型解析」 |

两个键都不需要写时,不生成镜像、不带 `--config`,并删掉上次留下的镜像:

```text
正式实例 · 官方模式 · 联网搜索开   serve --port 8686 --sandbox-port 8687 --detached --no-browser
正式实例 · 官方模式 · 联网搜索关   serve --config <镜像> …      镜像 = 源 + enable_web_search
正式实例 · 第三方模式              serve --config <镜像> …      镜像 = 源 + default_model [+ enable_web_search]
免登录沙箱(总是第三方模式)        serve --data-dir <沙箱> --config <镜像> …
```

| | 官方实例 | 免登录沙箱 |
| --- | --- | --- |
| 源(只读) | `~/.claude-science/config.toml` | `<沙箱 data-dir>/config.toml` |
| 镜像(CSSwitch 写,0600) | `~/.csswitch/science-config/official.toml` | `~/.csswitch/science-config/sandbox.toml` |

- 镜像由 `toml_edit` 解析源文件后写出:只动上表两个根键(源里已有同名键就覆盖),其余键、表与注释
  原样保留。
- CSSwitch 对真实 `~/.claude-science/` 只读这一个文件,不写、不建任何东西。
- `--config` 只换配置文件,不换数据目录,登录与对话不受影响(2026-10-10 用 0.1.60 在隔离 HOME 实测)。
- 每次启动都重新生成镜像。源文件不存在时镜像只有覆盖的键;源文件读不了或解析不了时报错并拒绝启动,
  不猜着改 —— 这样的文件 Science 自己也起不来。
- 为什么 `default_model` 只能走这里:第三方模式下 Science 的默认模型只取配置里的这个键(默认
  `claude-opus-5`),界面里的「设置默认模型」依赖 claude.ai 的模型选择器,不可用。菜单里不列
  `claude-opus-5`,不指定它,新会话的默认模型就不在菜单里。
- `~/.csswitch/science-config/` 是派生文件,可直接删除。
