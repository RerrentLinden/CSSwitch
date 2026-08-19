# 目录结构

单个 Rust crate:`desktop/gateway`。

```
desktop/gateway/
├── ui/index.html            控制台(单文件,include_str! 内嵌)
└── src/
    ├── main.rs              子命令分发:serve(默认) / official-passthrough
    ├── control.rs           单进程装配:HTTP 路由、控制面 API、WebUI
    ├── profile.rs           用户配置:模式、渠道、四模型槽、静态目录生成
    ├── science.rs           Science daemon 控制器(启停/状态/登录链接)
    ├── config.rs            GatewayConfig:契约装配与策略解析
    ├── server.rs            推理:模式路由、relay 分支、SSE 转发
    ├── anthropic_compat.rs  Anthropic 中继:relay flavor、Kimi 补偿
    ├── deepseek_compat.rs   DeepSeek 补偿链
    ├── kimi_search_noise.rs Kimi 搜索轮响应侧:噪声/幻影对剥离、配对键采钥
    ├── official_passthrough.rs 官方直通 + 诊断子命令
    ├── messages.rs          上游传输(超时、鉴权、错误保留)
    ├── models.rs            模型清单响应
    ├── static_profile.rs    模型目录解析与指纹
    ├── provider_contracts.rs  契约加载与校验
    ├── anthropic_sse.rs     SSE 生命周期校验
    ├── auth.rs / connect.rs 本地鉴权、CONNECT 隧道
    └── ...
```

配置与契约在 `catalog/`(JSON,编进二进制)。测试在 `test/`(三层门禁)。

## 约定

- 新增 provider 专属补偿:独立模块(如 `deepseek_compat.rs`),不要塞进
  `anthropic_compat.rs` —— 后者是共用的中继逻辑。
- 模块声明按字母序排在 `lib.rs`。
- 删除子系统时同批删除其模块、契约条目、测试与文档,不留注释掉的死代码。
