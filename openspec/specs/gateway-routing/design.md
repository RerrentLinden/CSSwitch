# gateway-routing 设计注记

## 端点合同的由来

2026-08 实测:Science 经 base_url 只调这两个端点,登录 / 会话存储 / 内核 /
Reviewer 都走 daemon 自身。所以没有第三类端点需要处理,其余一律显式 404 ——
新版本 Science 若开始调别的端点,会立刻暴露而不是被静默兜底。
免登录沙箱调的是同样两个端点,只是前面多一段 `/sandbox`;前缀下的其它路径同样显式 404。
`csswitch-gateway official-passthrough --port N --log F` 是诊断子命令,
catch-all 转发并记录脱敏端点清单,用于回归这个结论。
