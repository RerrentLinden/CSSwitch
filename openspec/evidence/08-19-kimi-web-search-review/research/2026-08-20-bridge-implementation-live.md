# Kimi 全模型 Web Search bridge 实现与 fresh live 证据

- 日期：2026-08-20
- 基线：`dbf2aa0` 后的未提交工作树
- 授权：用户明确授权本任务内的真实 Claude Science/Kimi 测试消息；未读取或输出凭证、账号库、私人日志。

## 最终产品决策

恢复早期 client-tool bridge 的产品语义，作为整个 `kimi-anthropic-relay` 的统一 Science 主路径：

```text
Science typed web_search declaration
  -> main: private ordinary query tool（模型决定是否搜索）
  -> no search: 1 upstream call，普通回答/工具原样返回
  -> search: nested typed web_search（真实 server executor）
       -> nested 有正文：2 calls 完成
       -> nested 只有真实 pair：bounded synthesis，第 3 call 生成正文
  -> 对 Science 只输出真实 search pair + 正文/普通工具，内部 query tool 不可见
```

不是机械 revert 旧提交：保留现有 native executor、噪声/幻影过滤、ID 采钥、历史修复和严格协议边界；
旧约 920 行 bridge 没有原样恢复。新增模块复用当前 transport/filter/SSE validator，并用 fresh live 证据补充：

- nested 指定型 Web Search 必须显式 `thinking: disabled`，因为它绕过正常 request transform。
- nested/search result 已有正文时不 synthesis；无正文时才做第三调用。
- K3 有时在完整真实 pair 后尾随幻觉 client `tool_use(name=web_search)`；只在严格尾部形状下剥离，
  不重搜，进入 synthesis；其它 client tool 仍 fail closed。
- main 原 `max_tokens` 不变；nested `min(main,4096)`；synthesis `min(main,8192)`。
- search evidence 总上限 512 KiB，超限明确失败、不截断。
- native typed-search 路径代码继续作为 nested executor 保留，并通过内部 bypass 防 bridge 递归。

## 同批协议边界修复

1. 只有 `web_search_tool_result.content` **存在、类型为数组且显式 `[]`** 才可判为幻影空结果；
   缺失、string、object、null 均保留/显式处理，不能静默删除。
2. stream/nonstream 响应采钥与幻影剥离只处理前置 `server_tool_use.name=web_search`。
3. 请求历史 pairing repair 只使用 Web Search server use；其它 server tool 不再被错误绑定。

## Direct provider fresh live（当前 bridge）

| 模型 | 结果 | calls / 阶段耗时 | 结构 |
| --- | --- | --- | --- |
| K3 | PASS | 3 calls；main 5.9s / nested 6.8s / synthesis 19.6s | 10 results、pair + text、内部 ID 不可见 |
| K3-256k | PASS | 3 calls；main 38.9s / nested 5.3s / synthesis 35.4s | 4 results、pair + text |
| K2.7 | PASS | 2 calls；main 2.0s / nested 17.0s | 11 results、pair + text |

另有 K3 nonstream 首轮 smoke 在早期中间态暴露 nested 无正文、尾随 client search、synthesis 无提示、
K3-256k 128k nested 超时等问题；每项均先 fail closed，再按上述窄合同修复并重新从 release 实测，
没有把中间失败记成通过。

## Claude Science fresh live（最终 release）

### K2.7

- 全新会话 Zig 搜索：正确 query、13 results、完整正文；adapter `bridged=1`, 2 calls。
- 不联网追问：正确复述；`bridged=0`, 1 call。
- 同会话 ROCm 第二次搜索：正确 query、9 results、完整正文；2 calls。
- 整页重载后两张搜索卡片与追问仍在；无 400 / repair / upstream failure。

### K3-256k

- 全新会话 Bun 搜索：正确 query、3 results、Web Search 卡片与正文；随后普通 GitHub client tool
  继续执行并正常收束，证明 bridge 与 Science 普通工具可组合。
- adapter 搜索轮 3 calls：main 28.9s / nested 5.0s / synthesis 68.6s，总计约 102s。
- 不联网追问正确复述，`bridged=0`；整页重载后卡片与追问仍在。
- 无 400 / repair / upstream failure。长首字节延迟是统一 bridge 的已确认代价，不隐瞒。

### K3

- 全新会话 Deno 搜索：正确 query、14 results、完整正文；本次 nested 自带正文，2 calls：
  main 6.4s / nested 19.5s。
- 不联网追问正确复述，`bridged=0`, 1 call（约19.0s）。
- 整页重载后搜索卡片与追问仍在；无 400 / repair / upstream failure。

## 离线验证

最终工作树：

```text
bash test/run_all.sh
static PASS
149 unit PASS
10 loopback PASS
```

后续 adapter 迭代又增加了真实协议回归；最终独立 check 必须以当前测试数重新跑门禁，不能沿用 149。

## 已知边界与代价

- 所有带 typed declaration 的 Kimi Science 主请求先缓冲 main nonstream；搜索轮多一次或两次上游调用，
  K3-256k 已观察约 102s 首字节延迟。用户明确选择统一 bridge，接受该取舍。
- main 同时产生内部 search 与未解决普通 client tool、且 nested 无正文时，缺普通 tool_result，当前明确
  fail closed；nested 有正文时普通 tool 可保留并以 `stop_reason=tool_use` 交给 Science。
- 未知未来 Kimi model 复用同 contract/bridge，但未宣称已 live；完成合同只覆盖 K3、K3-256k、K2.7。
- 搜索结果的事实正确性属于 provider 内容质量；本验收证明 query/result/card/正文链路，不把结果正文
  当成独立事实核验。
- 未 commit、push、archive；任务保持 `in_progress`。

## Final check 加固后的重新验收

独立 check 随后增加了 query/evidence untrusted provenance、三阶段共享 180s deadline、multi-query
唯一性/数量、fallback filter gate、逐消息 repair 状态。由于这些改变真实请求，前一轮 live 不作为最终
证据；重新构建 release 后再次逐模型新建 Science 会话：

| 模型 | 搜索 | 不联网追问 | 重载 | 最终 adapter 日志 |
| --- | --- | --- | --- | --- |
| K3 | Elixir，14 results，query/card/text 正确 | 正确复述，零新搜索 | card + 追问保留 | 搜索 2 calls：main 6.1s / nested 11.0s；追问 1 call 4.1s |
| K3-256k | Vite，17 results，query/card/text 正确 | 正确复述，零新搜索 | card + 追问保留 | 搜索 2 calls：main 118.2s / nested 26.1s；追问 1 call 155.8s |
| K2.7 | Lua，13 results，query/card/text 正确 | 正确复述，零新搜索 | card + 追问保留 | 搜索 2 calls：main 7.3s / nested 12.1s；追问 1 call 5.2s |

三模型均无 `pairing-repair`、400 或 `upstream_failure`。K3-256k 两轮均低于共享 180s deadline，
但首字节延迟高，继续作为已知产品代价。最终独立 check 的离线证据为 152 unit + 10 loopback 全绿。
