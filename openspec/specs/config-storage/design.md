# config-storage 设计注记

## 理由与教训

旧桌面端拥有 `~/.csswitch/config.json`，它会把任何不认识的 schema 当成自己的**旧版本**去迁移——
实测结果是它把自身的 provider 列表迁移成了空数组，用户配置当场丢失（已从备份恢复）。

## 必需测试

loopback 层断言：保存配置后 `~/.csswitch/config.json` **不存在**（沙箱 HOME 内），配置文件权限为 `0600`。
这条断言是为防止重蹈覆辙而存在的，删除它等于删掉护栏。
