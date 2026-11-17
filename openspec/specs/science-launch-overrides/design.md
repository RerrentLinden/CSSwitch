# science-launch-overrides 设计注记

## 签名

```text
science_config::LaunchOverrides { web_search: bool, default_model: Option<String>, skip_approvals: bool }
science_config::launch_config(&overrides, source, mirror) -> Result<Option<PathBuf>, String>
  source(只读): ~/.claude-science/config.toml
  mirror(0600): ~/.csswitch/science-config/official.toml
control::launch_overrides(profile, instance) -> LaunchOverrides   # 五个启动点唯一的取值处
POST /control/web-search {"enabled": bool, "force"?: bool}
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
- `control::tests`：切换判定纯函数的四条分支；请求体的 `enabled` / `force` 必须是布尔；
  `launch_overrides` 对两个实例各取各的模式（正式实例官方、沙箱走渠道 → 正式实例 `None`、沙箱
  `claude-sonnet-19`；把沙箱写成取 `profile.mode`，或把正式实例写成取 `sandbox_mode`，这条会红）。
- loopback 层的启动覆盖项：官方模式 + 开启态 `serve` 整行等于主线；官方模式 + 关闭态整行带
  `--config <镜像>`，镜像在根表含 `enable_web_search` 并保留临时 HOME 里源配置的其它键，源目录不变；
  官方模式下重新开启后整行回到主线、镜像被删。第三方模式下整行总是带 `--config`，镜像含
  `default_model`，开关联网搜索只增减 `enable_web_search`；切回官方模式后镜像被删。
  活跃会话未确认、源配置无法解析两种情况下配置文件逐字节不变且没有 stop / serve。
