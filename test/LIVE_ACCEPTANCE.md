# 真机验收清单

`test/run_all.sh` 的三层全部离线。下面这些只能在真实环境里得到答案:官方登录态、
第三方端点的真实行为、Science 的真实工具轮次。每次改动 provider 补偿链或
daemon 控制逻辑后跑一遍。

前提:本机已安装并登录 Claude Science;需要时由用户提供渠道 API Key
(只经 WebUI 输入,不写进仓库、不进日志)。

## A. 官方模式

1. 打开 CSSwitch 应用(或从源码运行 `./csswitch`),在控制台确认「官方 Claude」为当前连接。
2. 控制台点「启动 Science」→ 打开 Science → 用自己的账号登录。
3. 检查:项目与历史会话完整、无 session 过期横幅。
4. 发一轮普通对话,确认流式输出正常。
5. 发一轮需要工具的任务(例如让它跑一段 python),确认内核执行与结果正确。
6. Science 设置页 → About:**必须显示自动更新可用**(不是 "Automatic updates are off")。

## B. 第三方模式(Kimi / DeepSeek 各跑一遍)

1. 控制台「渠道配置」填 base_url 与 API Key,点「获取可用模型」应列出真实模型。
2. 四槽按需填写;留空槽应继承默认槽。保存。
3. 切换到该模式,确认 Science 自动重启并给出新登录链接。
4. 进入 Science,**新开一个干净会话**(同一对话不混供应商),确认模型菜单里
   显示的是你填的显示名,每个模型只出现一次,默认选中的是默认槽的模型(细节见 §H)。
5. 发一轮带工具的任务,确认执行与结果正确。
6. 回控制台看「请求日志」:不应有 error 行;日志里不得出现任何 key 或消息内容。

## C. 已知需要盯的兼容点

以下缺陷有过实测记录,回归时优先看它们:

| 现象 | 对应补偿规则 | 判据 |
| --- | --- | --- |
| Kimi:辅助调用 400 `tool_choice 'specified' is incompatible with thinking enabled` | `provider.kimi.specified-tool-choice-disables-thinking` | 日志里该规则命中,且无 upstream_failure |
| Kimi K3:回答里完全没有 thinking 块 | `provider.kimi.thinking-upstream-default` | K3 会话应能看到思考过程 |
| Kimi:带 PDF 附件后此轮及以后全部 400 | `provider.kimi.document-block-placeholder` | 该轮继续,且模型说明附件未送达 |
| Kimi:Science 声明 Web Search 但模型不搜索/无最终正文 | `provider.kimi.web-search.query-tool-adapter` | **K3、K3-256k、K2.7 分别新建会话**：搜索时日志 `bridged=1` 且 calls=2/3，query/card/text 正确；下一轮明确不搜索时 `bridged=0,calls=1`；整页重载卡片仍在。三模型不能互相替代验收 |
| 关掉「联网搜索」总开关后仍然搜索(官方 / Kimi / DeepSeek) | `tool.relay.web-search-disabled-by-config`(只是网关一侧的兜底) | 完整动线与判据见 §E。要点:三种模式各开一个**全新会话**问必须联网才能答的问题,都不得出现搜索卡片;Kimi / DeepSeek 的 relay 日志行里 `rules=` **不含**该规则才说明是 Science 自己不再声明,含它说明 Science 一侧没生效、是网关在兜 |
| Kimi:搜索过一轮之后中途关掉「联网搜索」 | 无(未修边界) | 已有搜索历史的会话里关掉总开关再追问:**观察下一轮是否 400**。关闭态不清洗历史搜索块,上游能否接受"未声明该工具却回放其历史"尚无证据;若 400,记下报文与 `CSSWITCH_DEBUG_TOOL_SKELETON=1` 的历史骨架,不要就地加兜底 |
| Kimi:搜索过一轮之后全部 400 `tool_call_id  is not found` | `provider.kimi.web-search-result-pairing-repair` | **必须在全新会话里测**:第 1 轮联网搜索,第 2 轮追问。搜索轮正常不够,要看下一轮不报错。诊断用 `CSSWITCH_DEBUG_TOOL_SKELETON=1` 看历史骨架 |
| Kimi:回答开头/结尾出现 `Search results for query:` 文本 | `provider.kimi.search-noise-text-strip` | 联网搜索轮的助手内容里**不得出现**该文本头;日志应见 `noise=N` 命中 |
| Kimi:不搜索的轮次出现空 Server Tool 框并让后续轮 400 | `provider.kimi.empty-search-pair-strip` | 只有前置 `name=web_search` 且 result `content` 精确为数组 `[]` 才剥离；缺失/string/object/null 必须保留或显式失败。在有过搜索的会话里发“不用搜索”问题，后续轮不得 400。历史搜索卡片重显属 Science 行为，不算新搜索 |
| DeepSeek:多轮 thinking 后 400(旧 BUG-083) | `provider.deepseek.tool-thinking-history-replay` | 连续多轮带工具的对话不报错。2026-08-19 已实测通过 |
| DeepSeek:`thinking auto` 被拒 | `provider.deepseek.thinking-auto-adaptive` | 首轮即可正常 |

## D. 不要做的事

- 不要给官方实例传 `--data-dir`:会造出隔离实例,对话分家。
- 不要给官方实例传 `--no-auto-update`:官方实例必须保留自动更新。
- `--config` 只指向 CSSwitch 生成的镜像(`~/.csswitch/science-config/`),而且只在两种情况下出现:
  联网搜索关闭,或这个实例处于第三方模式;官方模式 + 联网搜索开启时不要传。镜像里除了用户原有的内容,
  只会多两个根键:`enable_web_search`、`default_model`。不要改写 Science 自己的
  `~/.claude-science/config.toml` —— CSSwitch 对那个目录只读这一个文件。
- 不要把官方模型 id(`claude-opus-5` 等)列回第三方模式的模型清单:菜单会重新出现重复条目。
- 不要把服务配置写成 `~/.csswitch/config.json`:旧桌面端占用该文件名,
  它会把不认识的 schema 当自己的旧版本迁移,结果清空自身 provider 列表。
- 不要为了让免登录沙箱用上官方 Claude 而把真实登录借给它(读取 OAuth token、把真实登录钥匙串
  挂进沙箱):沙箱只有 Kimi / DeepSeek 两个渠道可选,见 §G。

## E. 联网搜索总开关

离线门禁只能证明「关闭时 Science 的启动参数带上了镜像配置」和「网关摘除了搜索声明」。
关闭后 Science 发来的请求是否真的不再声明 typed `web_search`,只能真机看,下面这一节就是为它准备的。

**已有的真机记录(2026-10-10,安装版 0.9.0,Science 0.1.60,免登录沙箱 + Kimi `k3`,
每一步都是全新会话):**

| 状态 | 会话里 | relay 日志(主请求) |
| --- | --- | --- |
| 开 | 出现搜索卡片 | 当时「记录请求日志」关着,未留记录 |
| 关,沙箱未重启 | 模型回答没有联网搜索工具 | `rules=` 含 `tool.relay.web-search-disabled-by-config`:Science 仍在声明,网关摘除 |
| 关,沙箱已重启(进程带 `--config <镜像>`) | 模型回答没有联网搜索工具 | `rules=` 里没有该规则,`server_tool_types=-`:**Science 自己不再声明** |
| 再开,沙箱已重启(进程不带 `--config`) | 搜索卡片 + 正确答案 | `rules=` 含 `provider.kimi.web-search.query-tool-adapter`,`bridged=1 queries=1 upstream_calls=2` |

开关触发的官方实例重启也在同一轮里跑过(关 → 带 `--config` 启动;开 → 不带),进程持续运行。

**仍未执行**:官方模式与 DeepSeek 模式下的第 1、3、5 步;第 6 步(官方实例的历史对话、登录、
自动更新不受影响)只确认了进程照常启动,没有逐项核对;第 8 步的失败路径。

前提:Science ≥ 0.1.56(更早的版本不认识 `enable_web_search`,对这个键是忽略还是拒绝启动未测)。
用 `./csswitch` 在终端前台启动,relay 日志行直接看终端(用安装版时看
`~/.csswitch/service.log`,内容相同);「记录请求日志」保持开启。

1. 开关开启(默认)时,官方 / Kimi / DeepSeek 各开一个**全新会话**问一个必须联网才能答的问题,
   确认三种模式当前都会搜索 —— 没有这一步,后面的「不搜索」不说明任何问题。
2. 控制台「当前连接」卡片关掉「联网搜索」。Science 正在运行时应提示已重启;有会话正在进行时
   应先弹确认框,取消后开关保持原样、Science 不重启。
3. 关闭后三种模式各开一个**全新会话**,问同类问题:都不得出现搜索卡片。
4. 看第 3 步里 Kimi 与 DeepSeek 两轮对应的 relay 日志行(`POST /v1/messages relay …`):

   | 观察 | 结论 |
   | --- | --- |
   | `server_tool_types=` 里没有 `web_search_*`,且 `rules=` 里**没有** `tool.relay.web-search-disabled-by-config` | 没有东西可摘:Science 自己不再声明,Science 一侧生效 |
   | `rules=` 里**有** `tool.relay.web-search-disabled-by-config` | Science 仍在声明,是网关在兜:Science 一侧**没有生效**,记下 Science 版本与启动参数 |

   决定性的是 `rules=` 那一项(关闭态下网关会先摘掉声明,`server_tool_types=` 本来就看不到它)。
   官方模式是零改写直通,没有 relay 日志行,只能按第 3 步的行为判定。
   需要更直接的证据时,用 `CSSWITCH_DEBUG_CAPTURE_DIR=<目录>` 启动服务,看
   `1-science-request.json` 的 `tools` 里有没有 `web_search_*`(文件含完整对话正文,
   权限 0600,排障后删除)。
5. 重新打开开关:三种模式的联网搜索恢复(Kimi 的日志重新出现
   `provider.kimi.web-search.query-tool-adapter`)。
6. 整个过程前后,官方实例的历史对话与项目完整、无需重新登录、设置页 About 仍显示自动更新可用;
   `~/.claude-science/config.toml` 的内容与修改时间没有变化。
7. 免登录沙箱:沙箱运行中切换开关,控制台应提示需要重启沙箱;重启沙箱后重复第 3、4 步。
8. 失败路径:把 `~/.claude-science/config.toml` 临时改成非法 TOML(先备份),关闭开关应当报出
   文件路径与出错行列、开关保持开启、Science 不重启;恢复文件后再关闭应当成功。

## F. 安装版(CSSwitch.app)

离线能证明的是两件事:应用包本身合法(`packaging/build_app.sh` 的自检),以及包内二进制
被直接执行时启动器的行为(loopback 层的「安装版启动器」一段:拉起、不重复拉起、环境导入、
日志权限、版本不一致与端口被占时调用了提示)。**经 Finder 的真实双击、提示框是否真的看得见、
系统的放行流程、图标在系统各处的显示,只能在真机上确认。** 截至 2026-10-10 这一节未执行。

前提:自检全过的 `dist/CSSwitch-<版本>-arm64.dmg`;本机没有别的 CSSwitch 服务在跑
(控制台「退出服务」或 `./csswitch stop`);已安装并登录 Claude Science。
把应用放进「应用程序」需要用户自己动手或明确授权,Agent 不替用户安装。

必过项:

1. 打开 DMG,把 CSSwitch 拖进「应用程序」,双击。
   判据:默认浏览器打开控制台;页眉右侧的版本号与 DMG 文件名里的一致;Dock 与菜单栏都没有
   CSSwitch 的图标;没有任何提示框。
   经下载得到的 DMG 首次打开会被系统拦下,到「系统设置 → 隐私与安全性」放行后应能正常打开
   —— 把系统实际给出的提示文案记下来(目前未验证,README 里只写了去哪里放行)。
2. 再次双击。
   判据:只是再开一次控制台;`pgrep -fl 'CSSwitch serve'` 只有一行,pid 与第一次相同。
3. 在控制台点「退出服务」。
   判据:上面的 `pgrep` 不再有输出;再双击能重新启动并打开控制台。
4. 从应用启动后,官方模式与一个第三方渠道各发一轮对话(§A 第 4 步、§B 第 5 步)。
   判据:两轮都正常。在需要代理才能访问 `api.anthropic.com` 的网络里,官方模式这一轮就是
   「shell 里配置的代理到了服务进程」的验证;`~/.csswitch/service.log` 里应有一行
   `启动器:已从登录 shell 导入:…`,列出的变量名包含你在 shell 里配置的代理变量。
   若出现的是 `没有取到登录 shell 的环境…`,记下后面的原因。

只有真机看得到、顺带确认的:

5. `ls -l ~/.csswitch/service.log` 是 `-rw-------`。
6. 启动失败的提示:退出服务后,在终端里 `nc -lk 127.0.0.1 8788` 占住端口,再双击应用。
   判据:几秒后出现「CSSwitch 无法启动」提示框,正文里有日志路径和一行
   `无法监听 127.0.0.1:8788`;提示框在最前面,没有被别的窗口挡住。
   提示框由 `osascript` 弹出,它会不会被挡住离线测不了 —— 被挡住就是缺陷,请记下。
   验完按 Ctrl-C 结束 `nc`。
7. 版本不一致的提示:后台跑着另一个版本的服务(例如从旧 commit 用 `./csswitch` 起的)时双击应用。
   判据:先出现「后台服务不是当前版本」的提示框,关掉之后才打开控制台;后台服务没有被重启。
8. 图标:Finder(图标视图与列表视图)、Launchpad、Spotlight 里显示的都是新图标;
   列表视图那样的小尺寸下仍能认出交叉的双链与橙色开关。
9. 控制台的浏览器页签上显示图标(Safari 与 Chrome 各看一眼;页签图标是 SVG data URI,
   旧版 Safari 是否显示未验证)。

## G. 两个实例各用各的渠道

离线门禁证明的是网关一侧:`/sandbox/v1/*` 按沙箱自己的渠道路由、正式实例切到官方后沙箱入口
不被直通、被拒绝的切换不改配置。下面两件事只有真机能回答:

- **真实 Science 是否真的把请求发到带 `/sandbox` 前缀的地址。** 离线的依据只有对 0.1.60 二进制的
  静态检查(SDK 是 `baseURL + path` 直接拼接)。第 1 步是它的动态证明。
- **确认后的「保存 → 停止沙箱 → 启动沙箱」。** 离线只有判定函数的单测(真实的沙箱启动要调
  `security` 并占用固定端口,不进门禁)。第 3 步才真的跑这条路径。

**已有的真机记录(2026-10-10,安装版 0.10.0 @ `a6009df`,Science 0.1.60,正式实例与沙箱都在
示例项目里新开会话,每条消息只要求回复「收到」):**

| 步 | 结果 | 看到的 |
| --- | --- | --- |
| 1 | 通过 | 正式实例停在官方模式时沙箱照常运行;沙箱模型菜单是 Kimi K3,正式实例是 Opus 5.5,两边都正常回复;请求日志同时有 `沙箱 · kimi` 与 `正式 · official`(后者状态 200) |
| 2 | 通过 | 弹出确认,文案与设计一致;取消后沙箱渠道仍是 Kimi,沙箱进程 pid 不变 |
| 3 | 通过 | 提示「沙箱已切换到DeepSeek并重启」;沙箱 pid 变了,正式实例的 pid 与启动时间都没变,它那边的会话没有出现被重启打断的提示;重新进入沙箱后模型菜单是 DeepSeek V4 Pro,新会话记为 `沙箱 · deepseek`,此前的会话仍在列表里 |
| 4 | 通过 | 正式实例 官方 → Kimi → 官方,各重启一次;沙箱 pid 始终不变,选中态始终是 DeepSeek;同一分钟内 `正式 · kimi` 与 `沙箱 · deepseek` 交错出现 |
| 5 | 通过 | 停着时切回 Kimi 没有确认框,提示「下次启动沙箱时生效」;正式实例处于官方模式时沙箱启动成功,模型菜单回到 Kimi K3,新会话记为 `沙箱 · kimi` |
| 6 | 通过 | 等 17 秒,两组卡片的选中态都没变 |
| 7 | 通过 | 先把沙箱渠道设成 DeepSeek(Kimi 是默认值,分辨不出是否真的存住),退出服务、重新打开应用:正式实例仍是官方,沙箱仍是 DeepSeek |

整轮请求日志 32 条,0 条错误。两件只有真机能回答的事都有了答案:Science 0.1.60 确实把沙箱的请求发到
带 `/sandbox` 前缀的地址;确认后的「保存 → 停止沙箱 → 启动沙箱」在真实沙箱上跑通。

这一轮的边界:

- 操作在 Claude 桌面端的内置浏览器里完成。它会自动取消原生 `confirm()`:第 2 步的取消就是原生弹窗
  被取消的结果;第 3 步与第 7 步的「确认」由脚本替弹窗作答,弹窗外观没有截图。浏览器面板不可见时
  不能按坐标点击,控制台里的一部分点击是用脚本触发同一个按钮的点击事件。
- 沙箱重启之后,之前开着的沙箱页面会显示已登出(Science 提示去运行 `claude-science url …`),
  要回控制台点「打开沙箱」重新进入。切换成功的提示里没有写这一点。
- 没有测:带工具的任务轮次、联网搜索、两个渠道的其它模型槽、第 2 步里「原来的会话还能继续」
  (只确认了沙箱没有重启)、`./csswitch status` 的输出(状态取自控制面接口)、Science 的其它版本。

前提:Science 0.1.60(其它版本的 SDK 是否同样直接拼接 base_url 未核对);正式实例已登录;
Kimi 与 DeepSeek 两个渠道都已填好 Key;「记录请求日志」保持开启;每一步的对话都用**全新会话**。
从旧版本升级上来的环境先把沙箱重启一次:旧版本启动的沙箱带的是不含前缀的 base_url,
它的请求仍会落在正式实例的路由上。

1. 正式实例选「官方 Claude」,沙箱卡片里选 Kimi。启动 Science 与沙箱,两边同时各发一轮对话。
   判据:正式实例是官方模式时沙箱照样能启动(旧版本会拒绝,要求先切到第三方模式);
   两边都正常出字;沙箱的模型菜单是 Kimi 渠道里填的显示名;控制台「请求日志」里同时有
   `正式 · official` 与 `沙箱 · kimi` 的条目,沙箱的条目没有错误。
   若沙箱的请求在日志里记成了 `正式 · …`,或沙箱里直接报错而日志里没有 `沙箱 · …` 的条目:
   Science 没有按带前缀的 base_url 发请求。记下 Science 版本并停在这里,备选方案见
   [docs/ARCHITECTURE.md](../docs/ARCHITECTURE.md)「两个实例各选各的」。
2. 沙箱运行中,在沙箱卡片点 DeepSeek,在确认框里**取消**。
   判据:弹出了确认框;取消后选中态仍是 Kimi,沙箱没有重启(原来的会话还能继续),
   `./csswitch status` 里的沙箱渠道仍是 Kimi。
3. 再点一次 DeepSeek 并确认。
   判据:提示「沙箱已切换到 DeepSeek 并重启」;正式实例**没有重启**(它那边的会话没有出现
   `This session was interrupted by a restart`);用「打开沙箱」进入后模型菜单换成 DeepSeek 渠道的
   显示名,新会话的请求记为 `沙箱 · deepseek`;沙箱里原有的对话历史还在。
4. 切换正式实例的模式(官方 → Kimi → 官方)。
   判据:沙箱卡片的选中态始终是 DeepSeek,沙箱没有重启;沙箱里再发一轮仍记为 `沙箱 · deepseek`,
   正式实例在 Kimi 模式下的那一轮记为 `正式 · kimi`。
5. 停止沙箱,在沙箱卡片切回 Kimi。
   判据:没有确认框,提示「下次启动沙箱时生效」;随后启动沙箱,新会话记为 `沙箱 · kimi`。
6. 等一个轮询周期(15 秒):两组卡片的选中态都不弹回。
7. 控制台「退出服务」后重新打开。
   判据:沙箱卡片的选中态与退出前一致;正式实例的模式也与退出前一致。

## H. 模型菜单

离线门禁证明的是网关一侧:清单里每个模型只列一次、第三方模式启动时带镜像配置并在其中指定默认模型、
旧的模型 id 仍能路由。Science 拿到这些之后怎么显示菜单、怎么选默认模型,只有真机能回答。

前提:Science 0.1.60;Kimi 渠道已填好 Key;「记录请求日志」保持开启;每一步的对话都用**全新会话**,
每条消息只要求回复「收到」或跑一条最小的工具调用。Science 升级后这一节要重跑:它的菜单规则变了,
这里的做法就要跟着复查。读 Science 一侧的菜单条目与 `default_model_id` 时,在已登录的页面里取
`/api/models`。

1. 免登录沙箱选 Kimi,启动并进入。打开模型菜单。
   判据:每个不同的模型只有一条;主菜单的三个位置分别是高质量槽、默认槽、快速槽的模型,留空或与前面
   同模型的位置不出现(默认槽的模型总在说明为 Most efficient for simple tasks 的那个位置上);
   Fable 槽的模型与前三个都不同时在 More models 里;没有以官方 Claude 型号命名的重复条目。
2. 新开会话,不动模型菜单(浏览器此前没有手动选过模型)。
   判据:默认选中的是默认槽的模型。`ps` 里沙箱进程带 `--config`,
   `~/.csswitch/science-config/sandbox.toml` 里是 `default_model = "claude-sonnet-19"`
   (不论别的槽怎么填,它都在 sonnet 位;带 `opus` 的 id 会被 Science 对订阅档不够格的登录换掉)。
3. 默认模型发一轮带工具的任务;再切到快速槽的模型新开会话发一轮。
   判据:都正常出字并完成工具调用;服务日志里对应的 relay 行没有错误。
4. 把某个槽的显示名清空(模型 ID 用小写连字符写法的,如 `k3-256k`),重启沙箱。
   判据:它仍然出现在 Science 的菜单里,名字是换过写法的(`K3 256k`)。验完把显示名改回去。
5. 打开一个此前用旧模型 id 建立的会话,直接追问一条。
   判据:正常回复;服务日志里这条请求路由到了对应槽位的模型。
6. 正式实例停在官方模式时启动一次。
   判据:`ps` 里**不带** `--config`;`~/.csswitch/science-config/official.toml` 不存在。
7. 整轮结束后看「请求日志」与服务日志。
   判据:没有 error 行;沙箱的请求记为 `沙箱 · kimi`。
