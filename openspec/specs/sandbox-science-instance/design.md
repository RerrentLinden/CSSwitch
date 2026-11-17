# sandbox-science-instance 设计注记

## 签名

```text
sandbox::start(base_url, &LaunchOverrides) → forge(幂等三态) → SSH/gh 桥接 → 沙箱钥匙串 → 端口检查
  → 镜像配置 → daemon → 健康 → 钥匙串校验
claude-science serve --data-dir <沙箱> --config <镜像> --host 127.0.0.1 --port 8790 --sandbox-port 8791
  --no-browser --no-auto-update --detached
  镜像: source = <沙箱 data-dir>/config.toml(只读)  mirror = ~/.csswitch/science-config/sandbox.toml
  env: HOME=<沙箱 home>  ANTHROPIC_BASE_URL=<网关>/sandbox  https_proxy=<网关>(不带前缀;CONNECT 到 Anthropic / Claude 主机 404,其余直连隧道)
       DO_NOT_TRACK=1(关 Science 的使用统计与错误上报)
       PATH=<沙箱>/bin:<继承>  GIT_CONFIG_COUNT/KEY_n/VALUE_n(本机装了 gh 时)
sandbox_forge::ensure_virtual_login(auth_dir, email, sandbox_root)  # 三道护栏先行

配置: service.v1.json 顶层 sandbox_mode = "kimi" | "deepseek"      # 与正式实例的 mode 互相独立
路由: /sandbox/v1/messages | /sandbox/v1/models → Instance::Sandbox  # 前缀在路由处剥掉
POST /control/sandbox/switch {"mode": "kimi"|"deepseek", "force"?: bool}
  → {"ok", "mode", "mode_label", "changed", "sandbox_restarted"}
GET  /control/status → 顶层 sandbox_mode / sandbox_mode_label
```

## 必需测试

- 护栏负例断言**零写入**（真实目录逐字节不变、符号链接目标不建目录）。
- 幂等三态：Reused 断言 `.enc` 与 `encryption.key` 逐字节不变；Repaired 断言 org 保留。
- 兜底停止的命令行匹配：正例命中、pid 复用/真实实例/前缀相似目录/瞬时 CLI 四组负例拒绝。
- 启动参数：不带镜像时与字面量逐项相等；带镜像时 `--config <镜像>` 恰好一次、去掉后与不带时相同，
  且带着它的命令行仍被兜底停止的匹配认出（沙箱实际启动时总是带着它，认不出就停不掉）。
- 入口链接主机名改写：localhost → 127.0.0.1、带路径前缀、已是 127.0.0.1 时幂等、
  非法 URL 原样返回，四例都钉住；并断言沙箱主机名不等于 `localhost`。
- gh 桥接：包装脚本行为级校验（假 gh 在沙箱 HOME 下被执行时看到的是真实 HOME、参数原样
  透传、临时文件已改名）；查找本机 gh 时跳过沙箱 bin 与不可执行文件；环境注入的 PATH 前置
  与 git 凭据「先清空再指向 gh」两对都钉住；已有 `GIT_CONFIG_COUNT` / PATH 缺失两个负例。
- 独立渠道：
  - 路由函数：四条推理路径（含 query，前缀被剥、query 保留）与近似路径
    （`/sandbox`、`/sandbox/`、`/sandbox/v1/unknown`、`/sandboxx/…`、叠两层前缀、`//sandbox/…`）。
  - `daemon_env`：base_url 带前缀，两个代理变量不带；带 `DO_NOT_TRACK`，且取值不是空串 /
    `0` / `false`（这三个 Science 当作没设）。
  - 切换判定的四条分支，拒绝文案含「沙箱正在运行」；请求解析拒绝 `official`、未知模式、
    非布尔 `force`。
  - 启动前置检查要抽成函数来测（`sandbox_start_precheck`）：正式实例为官方而沙箱渠道完整 →
    通过；沙箱渠道缺 Key → 报错点名渠道。检查内联在处理函数里时，把它改回看 `profile.mode`
    没有任何测试会红（变异检查实测）。
  - `Profile::parse` 的推导四例、显式值优先、手改成 `official` 被归一、存盘读回。
  - `for_channel(profile, mode)` 取传入的模式，不读 `profile.mode`。
  - loopback：两个入口的请求日志与本地合成的模型清单分别来自各自渠道；正式实例切到官方后
    沙箱入口仍走渠道（这时不得向正式入口发推理请求，那会连到真实官方接口）；切换被拒后配置
    逐字节不变；切换沙箱渠道不触发正式实例的 stop / serve；正式实例收到的 base_url 始终不带
    路径。带 `force` 的重启路径不进 loopback：真实启动要调 `security`、占固定端口。

## Wrong vs Correct

### Wrong

```rust
// 沙箱跟着全局模式走,与正式实例共用一个入口
// env: ANTHROPIC_BASE_URL=http://127.0.0.1:8788
let cfg = GatewayConfig::for_channel(&profile)?; // 内部读 profile.mode

// 切换时先存盘,再判断要不要确认:被拒绝时路由已经改了
profile.sandbox_mode = mode;
profile.save()?;
if running && !force {
    return Err(sandbox_running_refusal());
}
```

### Correct

```rust
// env: ANTHROPIC_BASE_URL=http://127.0.0.1:8788/sandbox(代理变量仍是不带路径的网关地址)
let mode = profile.mode_for(instance); // 沙箱 → sandbox_mode
let cfg = GatewayConfig::for_channel(&profile, mode)?; // 官方直通只给「正式实例 + 官方模式」

// 判定是纯函数,拒绝发生在存盘之前
match plan_sandbox_switch(&profile.sandbox_mode, &mode, running, force) {
    SandboxSwitch::NeedsConfirmation => return Err(sandbox_running_refusal()),
    // ...
}
profile.sandbox_mode = mode;
profile.save()?;
```
