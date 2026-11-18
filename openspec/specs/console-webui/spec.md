# console-webui Specification

## Purpose

控制台是 CSSwitch 唯一的界面：单个内嵌 HTML 文件（`desktop/gateway/ui/index.html`），用 `include_str!` 编进二进制，跑在 loopback 上。
本规格约束它的构成、文案、凭证处理、危险操作的确认、状态刷新与结果提示。改动 `desktop/gateway/ui/index.html` 时适用——它是唯一的前端文件。
验收方式见 [WebUI quality guidelines](../../conventions/frontend/quality-guidelines.md)。

## Requirements

### Requirement: 说明文字从简

界面不是文档（用户 2026-10-10 的明确要求）。

- 静态说明每张卡片 MUST 至多一行短句，只回答「这是什么」；标签、占位文字、按钮名已经说清的不再重复。
- MUST NOT 写内部机制与实现词：兼容桥、镜像配置、缓存多久、文件路径之类一律不进界面。
- 后果 SHALL 在动作发生时说：会打断会话的操作用确认框，做完之后用一句结果提示（「已保存,重启 Science 后生效。」这种长度）。
- 机制、边界、出处写进 README 与 `openspec/`。想往界面里加第二句话之前，先问它是不是其实属于文档。

#### Scenario: 卡片上的静态说明

- **WHEN** 查看任意一张卡片
- **THEN** 它的静态说明至多一行，不含内部机制、实现词或文件路径

#### Scenario: 保存配置成功

- **WHEN** 保存配置成功
- **THEN** key 输入框被清空，提示写明生效时机：「已保存,重启 Science 后生效。」

### Requirement: 单文件零依赖

控制台 MUST 保持单文件、零依赖：不引入框架、构建步骤或外部 CDN。它跑在 loopback，引外部资源既拖慢启动也制造离线故障。

#### Scenario: 离线打开控制台

- **WHEN** 在没有外网的机器上打开控制台
- **THEN** 页面完整可用，不请求任何外部资源

### Requirement: 图标内联在同一个文件

图标 MUST 也在这一个文件里：页眉标题旁是一份内联 SVG；页签图标由脚本把**这同一份**序列化成 `data:image/svg+xml` 设到 `<link rel="icon">` 上，
在读状态之前就设好（服务连不上、或退出后页眉被换掉，页签图标都还在）。MUST NOT 引图片文件、加第二份拷贝或另开一个返回图标的路由。
图形来自 `packaging/icon/console-glyph.svg`（`gen.py` 生成，与应用图标的简化稿同一组参数），改图后把新内容同步进来。
图标自带深色底板，深色主题下靠 `var(--line)` 的一圈细线与背景分开。

#### Scenario: 服务连不上或已退出

- **WHEN** 服务连不上，或退出后页眉被换掉
- **THEN** 页签图标仍在

#### Scenario: 深色主题下的页眉图标

- **WHEN** 系统处于深色模式
- **THEN** 页眉图标的底板仍与背景分得开

### Requirement: 版本号取自状态接口

版本号 MUST 取自状态接口（`state.version`），MUST NOT 写死在页面里：页面与服务编在同一个二进制中，显示的就是后台实际在跑的版本。
更早的服务不报告版本号，那时 SHALL 留空，MUST NOT 显示 `undefined`。

#### Scenario: 状态带版本号

- **WHEN** 状态接口返回 `version`
- **THEN** 页眉右侧显示它，与 `--version` 一致

#### Scenario: 状态里没有 version

- **WHEN** 状态里没有 `version`（更早的服务）
- **THEN** 页眉版本留空

### Requirement: 文案对两种启动方式都成立

同一个页面既被安装版使用，也被 `./csswitch` 使用。涉及「怎么再次启动」「日志在哪」的句子 MUST 把两种情况都说到，MUST NOT 只写其中一种。

#### Scenario: 退出服务之后

- **WHEN** 用户退出服务
- **THEN** 退出后的卡片用一句话说明怎么再次启动，对安装版与 `./csswitch` 都成立

### Requirement: 主题跟随系统

颜色 MUST 全部走 CSS 变量，`prefers-color-scheme` 下只重定义变量。

#### Scenario: 系统切到深色模式

- **WHEN** 系统处于深色模式
- **THEN** 控制台以深色主题渲染

### Requirement: 凭证只进不出

API Key 输入框可以填，但服务端返回的状态里只有 `has_api_key: true/false`。页面上任何地方 MUST NOT 渲染 key 本身。
留空提交 SHALL 表示「保持不变」，不是「清空」——否则每次改模型槽都要重填 key。

#### Scenario: 已保存过 key

- **WHEN** 渠道已保存 API Key 后重新打开控制台
- **THEN** 页面只知道 `has_api_key` 为真，任何地方都不显示 key 本身

#### Scenario: 留空提交

- **WHEN** 用户只改模型槽、key 输入框留空提交
- **THEN** 已保存的 key 保持不变

### Requirement: 危险操作要确认

退出服务，以及会打断进行中会话的操作（切换模式、切换联网搜索总开关、切换运行中沙箱的渠道），MUST 先 `confirm()`。

- 退出服务会先停掉 Science 与免登录沙箱，确认文案 MUST 写明这一点和会话会被打断；退出后的卡片列出已停止的实例，停止失败的原文逐条展示。
- 操作被服务端以「有会话正在进行」拒绝时，SHALL 把它转成确认框再带 `force` 重试，而不是直接报错了事；
  用户取消时界面回到操作前的状态（服务端此时什么都没改）。
- 沙箱渠道的拒绝文案是「沙箱正在运行」（沙箱没有可靠的会话数，只要在运行就要确认），同样处理。

#### Scenario: 退出服务

- **WHEN** 用户点「退出服务」
- **THEN** 确认框写明会停掉 Science 与免登录沙箱、会话会被打断
- **AND** 退出后的卡片列出已停止的实例，停止失败的原文逐条展示

#### Scenario: 服务端因有会话而拒绝

- **WHEN** 操作被服务端以「有会话正在进行」或「沙箱正在运行」拒绝
- **THEN** 界面弹确认框；确认后带 `force` 重试

#### Scenario: 用户取消确认

- **WHEN** 用户在确认框里取消
- **THEN** 界面回到操作前的状态，服务端什么都没改

### Requirement: 两组模式卡片各管各的实例

两组模式卡片 MUST 用同一套渲染，各管各的实例：正式实例的在「当前连接」卡片（选中态看 `state.mode`），
沙箱的在「免登录沙箱」卡片（看 `state.sandbox_mode`，只有 Kimi / DeepSeek）。两张卡片各用一行说明它管的是哪个实例。
沙箱没有官方 Claude（它是虚拟登录），MUST NOT 放一张置灰的卡片；原因写在 README 里，不写在界面上。

#### Scenario: 两组卡片的选中态

- **WHEN** 正式实例与沙箱处于不同渠道
- **THEN** 「当前连接」卡片按 `state.mode` 选中，「免登录沙箱」卡片按 `state.sandbox_mode` 选中

#### Scenario: 沙箱卡片里的渠道

- **WHEN** 查看「免登录沙箱」卡片
- **THEN** 只有 Kimi 与 DeepSeek 两张渠道卡片，没有置灰的官方卡片

### Requirement: 提示 Science 不是经 CSSwitch 启动的

状态里有 `science.unmanaged` 且 Science 在运行时，「当前连接」卡片 MUST 在按钮行下面显示这一行（原文）：

> Science 不是经 CSSwitch 启动的,当前选择没有生效。先「停止」再「启动 Science」。

是不是经 CSSwitch 启动的由服务端判定（[official-science-instance](../official-science-instance/spec.md)「识别不是经 CSSwitch 启动的 daemon」），
页面 MUST NOT 自己推导，只负责显示。它随周期轮询更新，不显示原因代码。

#### Scenario: 状态里带 unmanaged

- **WHEN** 轮询读到 Science 在运行且带 `science.unmanaged`
- **THEN** 「当前连接」卡片出现上面那一行

#### Scenario: 恢复成经 CSSwitch 启动

- **WHEN** 用户先「停止」再「启动 Science」，下一次轮询不再带 `science.unmanaged`
- **THEN** 那一行随之隐藏

#### Scenario: Science 未运行

- **WHEN** Science 未运行
- **THEN** 不显示那一行

### Requirement: 带重启的请求在途时禁用控件

带重启的请求在途时，MUST 禁用触发它的控件。切换运行中沙箱的渠道要停、起一次沙箱，好几秒：在途时两张沙箱渠道卡片禁用，
周期轮询重绘卡片时 MUST 保持禁用；中途再点会留下错配状态。完成后 SHALL 只调 `poll()` 刷状态，不调 `refresh()`——后者会重绘渠道配置表单。

#### Scenario: 沙箱渠道切换在途

- **WHEN** 沙箱渠道切换请求在途，期间发生一次周期轮询
- **THEN** 两张沙箱渠道卡片保持禁用

#### Scenario: 切换完成

- **WHEN** 切换请求完成
- **THEN** 状态被刷新，渠道配置表单不被重绘

### Requirement: 失败要看得见

控制面返回的错误原文 MUST 直接展示，MUST NOT 吞掉或替换成泛化文案。

#### Scenario: 服务不可达

- **WHEN** 服务不可达
- **THEN** 页面顶部插入明确的错误卡片，不是白屏

#### Scenario: 模型探测失败

- **WHEN** 获取可用模型失败
- **THEN** 展示上游原文，不显示空清单

### Requirement: 改动即提交的开关以服务端为准

改动即提交的开关，失败后 MUST 以服务端为准。请求失败不等于什么都没发生——联网搜索总开关的「Science 重启失败」发生在存盘之后。
失败时 SHALL 重新读一次状态给复选框赋值，读不到才退回操作前的勾选；MUST NOT 无条件把勾选拨回去。请求进行中 MUST 禁用复选框。

#### Scenario: 切换联网搜索总开关失败

- **WHEN** 切换联网搜索总开关失败
- **THEN** 展示错误原文，并按服务端当前状态给复选框赋值

#### Scenario: 切换沙箱渠道失败

- **WHEN** 切换沙箱渠道失败
- **THEN** 展示错误原文；选中态以服务端当前状态为准（重启失败发生在存盘之后）

#### Scenario: 失败后读不到状态

- **WHEN** 请求失败且状态也读不到
- **THEN** 复选框退回操作前的勾选

### Requirement: 周期轮询只刷状态

周期轮询（`setInterval`）MUST 只刷状态，MUST NOT 重绘配置表单，也 MUST NOT 给开关赋值：它是给 Science 运行状态用的。
让它重跑表单渲染会把用户填到一半的地址、模型槽无声覆盖回已保存值，让它给开关赋值会把正在提交的开关弹回去。
全量重绘只在首次加载与保存 / 切换之后做；开关在首次加载与自己的操作完成后赋值。

#### Scenario: 表单填到一半时轮询

- **WHEN** 用户把渠道表单填到一半，期间发生一次周期轮询
- **THEN** 已填的地址与模型槽不被覆盖

#### Scenario: 开关提交期间轮询

- **WHEN** 开关的请求在途，期间发生一次周期轮询
- **THEN** 开关不被弹回

### Requirement: 控件放在它的作用范围所在的位置

只对一个渠道起作用的控件 MUST 只在那个渠道的标签页渲染，别的标签页不该出现一个按了不起作用的开关。
全局生效的控件（联网搜索总开关对三种模式与免登录沙箱都生效）MUST 放在全局卡片里，MUST NOT 塞进某个渠道的表单，更不要每个渠道各放一个。

#### Scenario: 联网搜索总开关的位置

- **WHEN** 查看控制台
- **THEN** 联网搜索总开关在「当前连接」卡片里，渠道标签页里没有第二个

### Requirement: 结果提示只说已经成立的事

结果提示 MUST 只说已经成立、且用户需要知道的事。成功提示按服务端响应逐项拼
（`science_restarted` →「Science 已重启。」，`sandbox_restart_required` →「沙箱要重启后才生效。」），没发生的事不说，
用户用不上的内部生效细节（网关哪一侧何时生效）也不说。说出来的每一句 MUST 是真的：宁可少说一句，不说一句对某种情况不成立的话。

#### Scenario: 切换联网搜索总开关成功

- **WHEN** 切换联网搜索总开关成功
- **THEN** 提示先说结果（已开启 / 已关闭），再按响应追加已经发生的事：`science_restarted` →「Science 已重启。」，
  `sandbox_restart_required` →「沙箱要重启后才生效。」；没发生的不说

#### Scenario: 切换沙箱渠道成功

- **WHEN** 切换沙箱渠道成功
- **THEN** 提示按响应如实写：`sandbox_restarted` → 已切换并重启；否则 → 下次启动沙箱时生效

### Requirement: 请求日志区分实例

请求日志的「实例 · 渠道」列 MUST 显示 `正式 · kimi` / `沙箱 · deepseek` 这样的写法：两个实例各有各的模式，只写渠道分不清是哪一边。

#### Scenario: 两个入口各发过请求

- **WHEN** 正式入口与沙箱入口各发过请求
- **THEN** 日志表格分别显示「正式 · <渠道>」与「沙箱 · <渠道>」

### Requirement: 请求日志标出调用类型

请求日志的「实例 · 渠道」后面 MUST 用一个小标签显示条目的 `detail.lane`（取值见 [request-log](../request-log/spec.md)「明细标出调用类型」）：
`main` →「主对话」，`sub` →「子任务」，`keepalive` →「后台·保活」，`fork` →「后台·分支」；已收录的代号显示中文名
（如 `ttl` →「后台·标题」、`sniff` →「后台·结束判断」）；没收录、但符合代号写法（至多 6 位小写字母数字）的显示「后台·<代号>」。
`lane` 为 `null`、不是字符串或不合代号写法时 MUST NOT 显示标签——标签直接拼进表格的 HTML，不合写法的值一律不渲染。

#### Scenario: 一条消息带出的几次请求

- **WHEN** 日志里有标题调用、主对话与结束判断三类条目
- **THEN** 它们分别带「后台·标题」「主对话」「后台·结束判断」标签

#### Scenario: 没收录的代号

- **WHEN** 条目的 `lane` 是一个没收录、但符合写法的代号 `xyz`
- **THEN** 标签是「后台·xyz」

#### Scenario: 认不出的请求

- **WHEN** 条目的 `lane` 是 `null`
- **THEN** 「实例 · 渠道」后面没有标签

### Requirement: 渠道配置表单的槽位

渠道配置 MUST 渲染 base_url、API Key 与四个模型槽，每槽只有模型 ID 与显示名称两个输入框；默认槽必填，其余留空继承默认。
「获取可用模型」SHALL 直接向上游拉清单。显示名就是 Science 模型菜单里看到的名字。

#### Scenario: 打开渠道配置

- **WHEN** 打开某个渠道的配置
- **THEN** 四槽渲染正确，每槽只有模型 ID 与显示名称两个输入框

#### Scenario: 获取可用模型

- **WHEN** 用户点「获取可用模型」
- **THEN** 清单直接从上游拉取并展示

### Requirement: 窄窗口按组换行

「当前连接」卡片的状态行 MUST 分组：两个状态标签一组，「启动 Science」「停止」「打开 Science」一组；放不下时整组换行，
组内再放不下才各自换行。「退出服务」不在组内，单独靠右。「免登录沙箱」卡片的三个按钮同样成一组。
窗口窄于 600 px 时，两组模式卡片 MUST 改为单列。360 px 起页面 MUST NOT 出现横向滚动，元素 MUST NOT 越出卡片。

#### Scenario: 约 600 px 宽的窗口

- **WHEN** 窗口约 600 px 宽
- **THEN** 三个操作按钮在同一行，「退出服务」靠右

#### Scenario: 窄于 600 px

- **WHEN** 窗口窄于 600 px（例如 480 或 360 px）
- **THEN** 模式卡片单列排列，页面没有横向滚动
