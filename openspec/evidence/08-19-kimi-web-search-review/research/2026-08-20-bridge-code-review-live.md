# 统一 bridge 交付的独立 code review 真机证据(2026-08-20)

- 审查对象:`git diff HEAD`(13 文件,+917/−160)+ 未跟踪 `kimi_web_search_adapter.rs`(1503 行),基线 dbf2aa0。
- 方法:/code-review high(8 查找角度 → 单票验证)+ 内置浏览器真机实测 + 网关日志/落盘监听。
- 运行现场:实测前经 `./csswitch` 构建门重启,进程 text inode 与磁盘产物一致(46362270)。
  注意:重启前 8788 上的旧进程 inode 与磁盘不一致——交付方最后一次构建后未替换运行进程。

## 真机实测(监听窗口:service.log 210 行之后)

| 轮次 | 模型 | 结果 | adapter 日志 |
|---|---|---|---|
| 搜索(Node.js LTS) | K3 | 直播卡片+14 results+落地正文 ✓ | bridged=1 queries=1 calls=2 main 10.0s nested 15.2s |
| 不联网追问 | K3 | 正确复述 ✓ | bridged=0 calls=1 main 8.2s |
| **整页重载** | K3 | **卡片消失 ✗**(正文保留) | — |
| 搜索(Go 版本) | K2.7 | 直播卡片+13 results+落地正文 ✓ | bridged=1 queries=1 calls=2 main 6.7s nested 19.1s |
| **整页重载** | K2.7 | **卡片保留 ✓** | — |
| max_tokens=1 探针 | K3 | 正常经 bridge 单跳 | bridged=0 main 501ms |

窗口内:`upstream_failure` 0、`pairing-repair` 0、adapter 轮 6。
每个 bridged 轮 nested 响应命中 `search-noise-text-strip`(noise=1, adopted=0)。

## 两个真机实锤(推翻交付声称)

1. **内部工具名泄漏(2/2)**:两个测试 frame 的落盘 assistant thinking 原文分别含
   `Use __csswitch_kimi_web_search_query.` 与该工具参数 schema 复述。
   清洗只按 tool_use.name 匹配,不查 thinking/text。交付的「内部 ID 不暴露」失效。
2. **K3 重载丢卡片**:K3 轮 frame_messages 只存 thinking+text(搜索整对被 Science
   落盘丢弃)→ 重载后卡片消失;K2.7 轮整对落盘 → 卡片保留。同一二进制、同一
   2-call 路径,落盘结果按模型分化;交付验收表「重载恢复 三模型通过」不可复现。
   D5 症状(搜索证据活不过落盘)在桥接路径回归。

## 审查结论(ReportFindings 已出 10 条,severity 排序)

CONFIRMED 5 条:thinking 泄漏、K3 落盘丢对、nested 白名单 fail-open(catalog 出厂
5/8 个 upstream id 在名单外,含 kimi preset 默认 kimi-k3)、合并流 >1MiB 触发
validator 整块上限流式 502(非流式 200 不对称)、搜索+普通工具混合轮在 synthesis
路径确定性 502(与 merge_response 显式支持同形状自相矛盾)。
PLAUSIBLE 4 条:响应过滤主路径死代码(use_noise_filter 恒 false,潜伏回归+遥测
脱钩)、桥接轮首字节零输出直到全部阶段完成(实测 26s,交付记录 156s,无 ping)、
usage 同键跨阶段类型不一致即整轮 502、tool_choice any 继承进 synthesis。
simplification 1 条:兜底冗余簇(三重校验/弃块校验/不可能溢出防护/可推导状态/
整份深拷/证据 3–5 份物化且上限后置/成功路径必弃的诊断串)。

REFUTED(已排除,不列缺陷):validator 重写帧导致 round_trip 误判(validator 原样
回吐字节);parse_adapter_message 脱敏绕过(serde_json Value 解析错误不含原文);
synthesis 拼装历史违反 validate_relay_tool_history(既有守卫链前置覆盖)。

## 修复轮结果(2026-08-20 02:0x,同日执行)

10 条发现 9 修 1 留:F1 改名根治泄漏(INTERNAL_TOOL_NAME="web_search",构造上不可泄)、
F3 删白名单全模型无条件强制、F4 证据上限进直通合并路径+Validator 分块 feed、
F5 混合轮跳过 synthesis 交回 Science、F6 复活无条件 Kimi 过滤+清 zombie 规则、
F7 立即写头+阶段注释帧、F8 usage 冲突取末值记告警、F9 复用 degrade helper、
F10 冗余簇净删约 130 行。157 单测 + 三层门禁 exit 0。

修复轮真机复验(fixed build,inode 一致):
- K3-256k 搜索轮:卡片+12 results+正文,merged_shape 仪表上线,重载(最新轮时)卡片在;
- K3 搜索轮:3 调用含 synthesis,尾随幻觉剥离 stripped_client_search_tail=1,
  搜索后自主调 bash 混合轮完整跑通(F5 真机旁证);
- 新 frame 落盘零 `__csswitch` 字符串(泄漏根治实证;唯一残留在修复前的历史 frame);
- max_tokens=1 探针 ~500ms 直通;全窗口零 upstream_failure、零 pairing-repair。

### F2 最终定性(唯一未修项)

机制已完整钉死:**Science 会在会话推进一轮后把旧 assistant 消息规整为纯 text**
(真机:Go 会话追问后,旧轮 pair 从 frame_messages 被追溯剥离、卡片消失;
新轮卡片正常)。桥接卡片因此只在「自己还是最新轮」期间存活;原生路径的卡片
则跨轮持久(2026-08-19 R6 六轮验收)。昨日「K3 丢卡 vs K2.7 存活」实为
「K3 测试发过追问、K2.7 没有」,与模型无关。
所有可黑盒检视的线上维度(块序、srvtoolu 键前缀、usage 原样透传、
shell 空 input + delta 单发)均与原生等形;Science 持久卡片层的选收依据
无法从外部确定。已落 merged_shape / pair_key_prefix 仪表;
下一步需原生 vs 桥接 SSE 字节级抓包对差(原生流式路径现已不对 Science 服务,
需临时构建),或接受「桥接卡片按轮短暂」为产品代价,由用户决策。

## 未验证与边界

- K3-256k 未由本轮独立复测(沿用交付方数据:搜索 118–144s、追问 156s,<180s)。
- 泄漏与丢卡片之外的 PLAUSIBLE 项均为代码级验证,未构造真机触发。
- 搜索结果内容正确性不在验收范围(provider 内容质量)。
- 本轮测试消息经用户授权;未读取凭证、账号库或非测试对话。
