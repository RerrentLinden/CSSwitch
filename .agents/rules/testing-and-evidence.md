# 测试与证据规则

- 默认入口是 `bash test/run_all.sh`,三层:static(fmt + clippy 零告警)、
  unit(cargo test)、loopback(起真实服务进程打真实 HTTP)。三层全绿才算门禁通过。
- 三层全部离线。官方登录、真实 provider key、Science 工具轮次这些只能真机跑,
  清单在 [test/LIVE_ACCEPTANCE.md](../../test/LIVE_ACCEPTANCE.md),结论要单独写。
- 证据层不得混淆:单测 ≠ loopback ≠ 真机;mock 不能写成 live provider;
  某条补偿在 A 端点实测通过,不等于 B 端点也通过——写清证据边界。
- 失败、未运行、环境阻塞、需人工判断一律不得记为通过。缺工具链时 `run_all.sh`
  以 env-blocked 退出,不能当成绿。
- 报告要写:跑了什么命令、什么 commit、退出码、脱敏后的关键输出;
  不要只报历史 pass 数量。
- 测试的唯一目的是发现问题。镜像实现的测试、只测 mock 的测试、为凑覆盖率的测试一律不写;
  写之前先说清它能抓住什么真实回归。
