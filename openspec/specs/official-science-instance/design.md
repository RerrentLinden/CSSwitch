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
```

`LaunchOverrides` 与镜像配置的签名见 [science-launch-overrides](../science-launch-overrides/design.md)。

## 理由与教训

- 错误：为"隔离干净"传 `--data-dir`——用户打开后发现对话全没了。
- 退出服务要先停实例：网关一走，指向它的实例只剩断掉的推理链路。
- `--config` 只换配置文件、不换数据目录，是 2026-10-10 用 Science 0.1.60 隔离实测的结论。

## 必需测试

- `science::tests::model_env_clear_list_covers_auth_and_role_overrides`：断言清除列表
  覆盖模型与凭证变量，且 **`ANTHROPIC_BASE_URL` 不在其中**（它是要注入的）。
- `science::tests::official_serve_args_pin_ports_and_never_isolate`：无镜像时参数向量与字面量
  逐项相等；有镜像时 `--config <镜像>` 恰好一次、去掉这一对后与无镜像时相同；`--data-dir`、
  `--no-auto-update` 两种状态下都不出现。这条测试是官方模式的冻结线，改启动方式时不改它。
- loopback 层断言配置文件权限为 `0600`、且未写出 `~/.csswitch/config.json`。
- loopback 层用假 `CLAUDE_SCIENCE_BIN` 与命令行同形的假沙箱进程，断言退出服务停掉两个实例且进程退出；
  loopback 全程不得解析到本机真实 `claude-science`。
- 假 `claude-science` 的 `status` 输出形状必须与真实一致
  （`{"running":true,"daemon":{"active_conversations":N}}`），否则测的是假设。

## Wrong vs Correct

### Wrong

```rust
// 为了"隔离"与"稳定"，把用户的实例改造成另一个实例
&["serve", "--port", "0", "--detached", "--no-browser",
  "--data-dir", &managed_dir, "--no-auto-update"]
```

### Correct

```rust
// 只在启动时注入，其余一律沿用用户自己的实例
&["serve", "--port", "8686", "--sandbox-port", "8687", "--detached", "--no-browser"]
// env: ANTHROPIC_BASE_URL=<gateway>
// 联网搜索关闭、或实例处于第三方模式时,在 serve 之后多一对:"--config", <CSSwitch 生成的镜像>
// 跳过审批总开关开着时,末尾多一个:"--dangerously-skip-approvals"
```
