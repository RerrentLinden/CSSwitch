# console-webui 设计注记

## 实现注意

- 全局 `input` 是 `width:100%`：复选框必须显式 `width:auto`，否则铺满整行。
- 让周期轮询重跑表单渲染或给开关赋值，用户看到的是「我明明改了却自己变回去了」。
- 切换完成后只调 `poll()`，不调 `refresh()`：后者会重绘渠道配置表单。

## 验收

`cargo test` 不覆盖任何一行 HTML/CSS/JS。验收动线与隔离方式见
[WebUI quality guidelines](../../conventions/frontend/quality-guidelines.md)「可视化验收是必需项」。
