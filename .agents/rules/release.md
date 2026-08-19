# 发布规则

- 发布固定到干净 commit;`bash packaging/build_app.sh` 从同一 commit 产出
  `dist/CSSwitch.app`,里面只有服务二进制 + 启动器 + 图标。
- 未经用户明确授权,不覆盖 `/Applications/CSSwitch.app`,不 commit / push / tag /
  发 release。构建或测试成功不构成这些授权。
- 分开记录:门禁三层结果、app 构建、真机验收。签名与公证是另一层结论,
  ad-hoc 签名不是 Developer ID 签名,更不是公证。
- README 与实际形态必须一致才算发布闭环(尤其是使用方式与补偿清单)。
