# 目录结构

单个 Rust crate:`desktop/gateway`。

```
desktop/gateway/
├── ui/index.html            控制台(单文件,include_str! 内嵌)
└── src/
    ├── main.rs              入口判定:启动器 / --version / serve(默认) / official-passthrough
    ├── launcher.rs          安装版启动器:状态探测、版本比对、登录 shell 环境白名单、脱离式拉起、提示
    ├── service_log.rs       serve --log-file:日志落盘(0600)与进程内按大小轮转
    ├── control.rs           单进程装配:HTTP 路由、控制面 API、WebUI
    ├── profile.rs           用户配置:模式、渠道、四模型槽、静态目录生成
    ├── science.rs           Science daemon 控制器(启停/状态/登录链接)
    ├── science_config.rs    联网搜索关闭态的镜像配置(源 config.toml 只读 + enable_web_search=false)
    ├── sandbox.rs           免登录沙箱编排(隔离 HOME/固定端口 8790·8791/钥匙串校验/兜底停止)
    ├── sandbox_forge.rs     沙箱虚拟登录铸造(v2 GCM 三件套/幂等三态/三道护栏)
    ├── config.rs            GatewayConfig:契约装配与策略解析
    ├── server.rs            推理:模式路由、relay 分支、SSE 转发
    ├── anthropic_compat.rs  Anthropic 中继:relay flavor、Kimi 补偿
    ├── deepseek_compat.rs   DeepSeek 补偿链
    ├── kimi_web_search_adapter.rs Kimi Web Search query-tool bridge 与响应重放
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

安装版的打包在仓库根的 `packaging/`,产物写到 `dist/`(不入库):

```
packaging/
├── build_app.sh             构建 + 组装 .app + ad-hoc 签名 + 自检 + DMG
├── Info.plist.in            Info.plist 模板(@VERSION@ / @MIN_MACOS@ 由构建脚本填)
├── icon/
│   ├── gen.py               按参数生成三张 SVG(图形的唯一来源)
│   ├── AppIcon.svg          主稿(64 px 及以上)
│   ├── AppIcon-small.svg    简化稿(16 / 32 px)
│   ├── console-glyph.svg    控制台页眉 / 页签图标用的精简版(内联进 ui/index.html)
│   ├── build_icon.sh        SVG → 十张 PNG → AppIcon.icns(需要 Google Chrome)
│   └── AppIcon.icns         入库;应用构建只拷贝它
└── brand/                   宣传图源文件
```

## 约定

- 新增 provider 专属补偿:独立模块(如 `deepseek_compat.rs`),不要塞进
  `anthropic_compat.rs` —— 后者是共用的中继逻辑。
- 模块声明按字母序排在 `lib.rs`。
- 删除子系统时同批删除其模块、契约条目、测试与文档,不留注释掉的死代码。
