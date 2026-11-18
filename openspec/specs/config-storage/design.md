# config-storage 设计注记

## 理由与教训

旧桌面端拥有 `~/.csswitch/config.json`，它会把任何不认识的 schema 当成自己的**旧版本**去迁移——
实测结果是它把自身的 provider 列表迁移成了空数组，用户配置当场丢失（已从备份恢复）。

## 必需测试

loopback 层断言：保存配置后 `~/.csswitch/config.json` **不存在**（沙箱 HOME 内），配置文件权限为 `0600`。
这条断言是为防止重蹈覆辙而存在的，删除它等于删掉护栏。

## 配置文件一览

`~/.csswitch/service.v1.json`(0600):`mode`(正式实例的模式)、`sandbox_mode`(免登录沙箱的
渠道,`kimi` | `deepseek`)、端口、两个渠道的 base_url / key / 四槽(两个实例共用)、
`request_log`(请求日志开关)、`web_search`(联网搜索总开关)、`skip_approvals`(跳过审批总开关,
关着时不写进文件)。

`~/.csswitch/science-config/`(0700,文件 0600):交给 Science 的镜像配置,每次启动重写,
见 [science-launch-overrides](../science-launch-overrides/design.md)「启动覆盖项」。

`~/.csswitch/service.log`、`service.log.1`(0600):安装版的服务日志,见 [installed-app-launcher](../installed-app-launcher/design.md)。
从源码运行时不产生。

文件名刻意避开 `config.json` —— 那是旧桌面端的文件名,它会把不认识的 schema
当作自己的旧版本迁移,结果清空自身 provider 列表(实测踩过一次)。
