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
   显示的是你填的显示名。
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
- `--config` 只允许出现在联网搜索关闭态,且只指向 CSSwitch 生成的镜像
  (`~/.csswitch/science-config/`);其它任何场合都不要传。不要改写 Science 自己的
  `~/.claude-science/config.toml` —— CSSwitch 对那个目录只读这一个文件。
- 不要把服务配置写成 `~/.csswitch/config.json`:旧桌面端占用该文件名,
  它会把不认识的 schema 当自己的旧版本迁移,结果清空自身 provider 列表。

## E. 联网搜索总开关

离线门禁只能证明「关闭时 Science 的启动参数带上了镜像配置」和「网关摘除了搜索声明」。
**关闭后 Science 发来的请求是否真的不再声明 typed `web_search`,截至 2026-10-10 未验证**
(隔离取证只做到「带 `enable_web_search = false` 的配置能正常启动、数据目录与登录不变」,
用的是 0.1.60)。下面这一节就是为它准备的。

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
