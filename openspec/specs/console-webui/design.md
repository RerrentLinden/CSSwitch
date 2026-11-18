# console-webui 设计注记

## 实现注意

- 全局 `input` 是 `width:100%`：复选框必须显式 `width:auto`，否则铺满整行。
- 让周期轮询重跑表单渲染或给开关赋值，用户看到的是「我明明改了却自己变回去了」。
- 切换完成后只调 `poll()`，不调 `refresh()`：后者会重绘渠道配置表单。
- 归属提示（`#sci-unmanaged`）在 `renderScience()` 里按 `running && !!science.unmanaged` 切换显示；
  `poll()` 每次都调它，所以提示随轮询出现与消失。原因（`app` / `port`）不进界面：用户要做的事两种情况一样。
- 调用类型标签由 `laneTag(detail.lane)` 生成，查 `LANES` 表（服务端代号 → 中文名）；它直接拼进日志表格的
  HTML，所以只渲染表里的键或过了 `^[a-z0-9]{1,6}$` 的代号，其余不显示。新代号先显示成「后台·<代号>」，
  要中文名就往 `LANES` 里加一行。
- `.row` 可以嵌套：内层 `.row` 是一组标签或按钮，放不下时整组换行，组内再放不下才各自换行。
  「退出服务」留在组外，靠 `margin-left:auto` 靠右。
- 模式卡片在 `@media (max-width: 599px)` 下改为一列：三列并排时第三列缩不过卡片上的接口地址
  `api.deepseek.com/anthropic`，原先在 360 / 480 px 下把页面撑到 528 px 宽、出现横向滚动。
  900 px 及以上的排布与改动前相同。

## 验收

`cargo test` 不覆盖任何一行 HTML/CSS/JS。验收动线与隔离方式见
[WebUI quality guidelines](../../conventions/frontend/quality-guidelines.md)「可视化验收是必需项」。

2026-11-18 的几项改动的验收(真机记录在 [test/LIVE_ACCEPTANCE.md](../../../test/LIVE_ACCEPTANCE.md) §K、§L、§M):

- 窄窗口布局:无头浏览器在 360 / 480 / 600 / 900 / 1200 px 五个宽度、Science 未运行 / 运行中 / 未安装三种状态下
  截图并断言无横向滚动、无元素越出卡片,600 px 下三个操作按钮同在一行,900 / 1200 px 下各元素坐标与改动前一致。
  这是隔离环境里的浏览器检查,不是真机;真机只在约 614 px 宽的浏览器面板里看过一次(当时显示着归属提示)。
  「沙箱运行中」状态没有截图。
- 归属提示:隔离环境里的真实控制台,status 输出用替身,报 `app-child` 或其它端口时出现,报 8686 + `launchd` 时不出现。
- 调用类型标签:真实 Science 0.1.72 的流量经假上游,控制台显示「后台·标题」「主对话」「后台·结束判断」。
