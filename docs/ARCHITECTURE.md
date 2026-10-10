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
官方实例固定监听 `127.0.0.1:8686`(预览端口 8687),免登录沙箱监听 8790/8791。
手动 `claude-science serve` 仍用 Science 自己的默认 8000,与经 CSSwitch 启动的地址不同。
两者都绑 loopback。

启动参数(方括号里的一对只在联网搜索总开关关闭时出现,见下文「联网搜索总开关」):

```text
官方:serve [--config <镜像>] --port 8686 --sandbox-port 8687 --detached --no-browser
沙箱:serve --data-dir <沙箱> [--config <镜像>] --host 127.0.0.1 --port 8790 --sandbox-port 8791
       --no-browser --no-auto-update --detached
```

官方实例永不带 `--data-dir` 与 `--no-auto-update`:前者会造出隔离实例,后者会关掉它自己的自动更新。

退出服务(`/control/quit`,控制台按钮与 `./csswitch stop` 共用)先并行停掉正在运行的官方实例与
免登录沙箱,再退出进程:网关一走,指向它的实例只剩断掉的推理链路。停止失败不拦截退出,
失败原文随响应返回。终端里 Ctrl-C 只结束服务进程,不停实例。

日志只打到服务进程的 stderr(终端),不落文件。控制台「请求日志」卡片的「记录请求日志」开关
(`service.v1.json` 的 `request_log`,默认开启,切换立即生效)控制每条请求的明细:终端里的
`POST /v1/messages relay …` / `relay stream|nonstream rules=…` / Web Search 桥接统计行,
以及卡片里的请求列表。启动横幅、沙箱生命周期、警告与上游错误不受开关影响,始终输出。

## 三种模式

| 模式 | `/v1/messages` | `/v1/models` |
| --- | --- | --- |
| 官方 | 零改写转发 `api.anthropic.com`,鉴权头原样透传 | 转发 |
| Kimi | relay 契约 + Kimi 补偿链 | 由用户四槽配置本地合成 |
| DeepSeek | deepseek 契约 + DeepSeek 补偿链 | 同上 |

模式存在配置里,切换需重启 Science daemon(它只在启动时读 base_url)。

## 联网搜索总开关

`service.v1.json` 顶层的 `web_search`(默认 `true`)是唯一真相,全局生效、不分渠道。
它对应 Science 0.1.56 起的实例级选项 `enable_web_search`:该选项写在 Science 的
`config.toml` 根表,只在启动时读取,没有应用内设置项也没有环境变量。一个值驱动两层:

```text
service.v1.json: web_search
        │
        ├─ Science 一侧(启动时一次性,science_config.rs)
        │     true  → 不带 --config,启动参数与没有这个开关时逐项相同
        │     false → 生成镜像配置 → serve --config <镜像> …
        │
        └─ 网关一侧(每请求读快照,config.rs / server.rs)
              true  → Kimi 的 typed web_search 走 query-tool bridge
              false → relay 入口摘除 typed web_search,桥不触发
```

**镜像配置**:源文件(只读)+ 根表 `enable_web_search = false`,由 `toml_edit` 解析后写出,
其余键、表与注释原样保留。

| | 官方实例 | 免登录沙箱 |
| --- | --- | --- |
| 源(只读) | `~/.claude-science/config.toml` | `<沙箱 data-dir>/config.toml` |
| 镜像(CSSwitch 写,0600) | `~/.csswitch/science-config/official.toml` | `~/.csswitch/science-config/sandbox.toml` |

- CSSwitch 对真实 `~/.claude-science/` 只读这一个文件,不写、不建任何东西。
- 镜像只会追加限制:开关开启时不带 `--config`,Science 自己配置里的任何值(包括手写的
  `enable_web_search = false`)原样生效。
- `--config` 只换配置文件,不换数据目录,登录与对话不受影响(2026-10-10 用 0.1.60 在隔离
  HOME 实测)。
- 每次启动都重新生成镜像;开启态启动时删除残留的镜像文件。源文件不存在时镜像只有这一个键;
  源文件读不了或解析不了时报错并拒绝启动,不猜着改。

**生效时机**:网关一侧存盘后下一次请求即生效,且不依赖 Science 是否已重启、是否认识这个选项。
Science 一侧要重启:`POST /control/web-search` 在官方实例运行时会重启它,有会话正在进行且未带
`force` 时拒绝并且不改任何状态;镜像生成不出来同样在存盘之前报错。重启失败不回滚,原文报出
(此时开关已改、Science 未按新设置运行;拨回开启再启动就是原始启动方式)。免登录沙箱不自动重启,
响应里用 `sandbox_restart_required` 标出。

官方模式的零改写直通不变:该模式下的关闭效果完全来自 Science 一侧。DeepSeek 平时把搜索声明
留给上游原生执行,关闭态同样在入口被摘除。

**回滚注意**:旧版本把这个开关存在渠道里(`kimi.web_search`)。新版本读到「顶层没有该键且
`kimi.web_search == false`」时迁移为总开关关闭,保存后只写顶层键。因此代码回滚到旧版本后,
旧二进制读不到渠道级旧键,会把「关」读成「开」。`~/.csswitch/science-config/` 是派生文件,
可直接删除。

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

- Kimi:`anthropic_compat.rs`(请求侧)+ `kimi_search_noise.rs`
  (搜索轮响应侧:噪声头 / 幻影对剥离、配对键采钥)
- DeepSeek:`deepseek_compat.rs`
- 策略(如 thinking 政策)来自 provider contract,不靠环境变量传递。
  环境变量只作显式 override 并留日志。

## 失败处理

- 上游错误保留 status,向客户端返回可诊断的错误体;不伪造成功。
- `CSSWITCH_DEBUG_UPSTREAM_ERROR=1` 时额外打印上游错误正文(仅当上游真的
  返回了 HTTP 状态;传输层失败的 detail 可能带 URL 凭证,那条路径保持静默)。
- 凭证从不出现在日志或控制面响应里:配置接口只报告"是否已配置"。

## 配置

`~/.csswitch/service.v1.json`(0600):模式、端口、两个渠道的 base_url / key / 四槽、
`request_log`(请求日志开关)、`web_search`(联网搜索总开关)。

`~/.csswitch/science-config/`(0700,文件 0600):联网搜索关闭时交给 Science 的镜像配置,
每次启动重写,见「联网搜索总开关」。

文件名刻意避开 `config.json` —— 那是旧桌面端的文件名,它会把不认识的 schema
当作自己的旧版本迁移,结果清空自身 provider 列表(实测踩过一次)。
