# 架构

## 一句话

一个本地 Rust 服务进程,把用户自己的 Claude Science 实例的推理流量转到
官方 Anthropic、Kimi 或 DeepSeek,并在必要处做窄补偿。

## 进程与端口

```
./csswitch(启动脚本)
  └── csswitch-gateway serve --port 8788           ← 唯一的进程
        监听 127.0.0.1:8788
                /                 WebUI(内嵌 HTML,单文件)
                /control/*        控制面 API(含 /control/quit 退出服务)
                /v1/messages      推理
                /v1/models        模型清单
                其余              显式 404
```

Science daemon 由服务用 CLI 拉起,注入 `ANTHROPIC_BASE_URL=http://127.0.0.1:8788`。
两者都绑 loopback。

## 三种模式

| 模式 | `/v1/messages` | `/v1/models` |
| --- | --- | --- |
| 官方 | 零改写转发 `api.anthropic.com`,鉴权头原样透传 | 转发 |
| Kimi | relay 契约 + Kimi 补偿链 | 由用户四槽配置本地合成 |
| DeepSeek | deepseek 契约 + DeepSeek 补偿链 | 同上 |

模式存在配置里,切换需重启 Science daemon(它只在启动时读 base_url)。

## 端点合同的由来

2026-08 实测:Science 经 base_url 只调这两个端点,登录 / 会话存储 / 内核 /
Reviewer 都走 daemon 自身。所以没有第三类端点需要处理,其余一律显式 404 ——
新版本 Science 若开始调别的端点,会立刻暴露而不是被静默兜底。
`csswitch-gateway official-passthrough --port N --log F` 是诊断子命令,
catch-all 转发并记录脱敏端点清单,用于回归这个结论。

## 模型解析

用户在 WebUI 里为每个渠道填四个槽(默认 / 高质量 / 快速 / Fable),
每槽是「上游模型 ID + 显示名」,空槽继承默认槽。服务据此生成一份静态模型目录:

- 每个不同的模型 ID 产出一条 route,selector 形如 `claude-csswitch-<model-id>`;
- 四个角色(sonnet / opus / haiku / fable)绑到对应 selector;
- Science 发来的官方模型名(如 `claude-opus-5`)按角色映射到上游模型。

目录带指纹校验,生成与解析共用同一套算法。

## 补偿链

补偿都是窄规则,统一形态:端点/flavor 门控 + 规则 ID + 单测。
命中的规则会打进请求日志,便于判断"哪条在起效"。

- Kimi:`anthropic_compat.rs`(含 `kimi_coding_search.rs` 的 web_search 桥)
- DeepSeek:`deepseek_compat.rs`
- 策略(如 thinking 政策)来自 provider contract,不靠环境变量传递。
  环境变量只作显式 override 并留日志。

## 失败处理

- 上游错误保留 status,向客户端返回可诊断的错误体;不伪造成功。
- `CSSWITCH_DEBUG_UPSTREAM_ERROR=1` 时额外打印上游错误正文(仅当上游真的
  返回了 HTTP 状态;传输层失败的 detail 可能带 URL 凭证,那条路径保持静默)。
- 凭证从不出现在日志或控制面响应里:配置接口只报告"是否已配置"。

## 配置

`~/.csswitch/service.v1.json`(0600):模式、端口、两个渠道的 base_url / key / 四槽。

文件名刻意避开 `config.json` —— 那是旧桌面端的文件名,它会把不认识的 schema
当作自己的旧版本迁移,结果清空自身 provider 列表(实测踩过一次)。
