# science-launch-overrides Specification

## Purpose

CSSwitch 需要在不改写用户 Science 配置的前提下，给 Science 加上少量启动时的设置：联网搜索总开关与第三方模式的默认模型。
做法是生成一份镜像配置并用 `--config` 传给 daemon。本规格约束镜像的生成、覆盖项的取值、联网搜索总开关的切换顺序与旧配置迁移。
官方实例与免登录沙箱共用这套机制；改动 `science_config.rs`、`control::launch_overrides` 或 `POST /control/web-search` 时适用。

## Requirements

### Requirement: 覆盖项由调用方传入

启动覆盖项（`LaunchOverrides`）MUST 由调用方传给 `start`，Science 控制模块内部 MUST NOT 读配置文件。
`--config` 是否出现 MUST 由 `launch_config` 的返回值唯一决定。

#### Scenario: launch_config 返回无镜像

- **WHEN** `launch_config` 返回 `Ok(None)`
- **THEN** 启动参数里没有 `--config`

#### Scenario: launch_config 返回镜像路径

- **WHEN** `launch_config` 返回镜像路径
- **THEN** 启动参数带 `--config <该路径>`

### Requirement: 镜像至多写两个根表键

镜像里 MUST 至多写两个**根表**键（`enable_web_search`、`default_model`），别的一概不写：

| `web_search` | `default_model` | 结果 |
| --- | --- | --- |
| true | None | `Ok(None)`：不生成、不带参数，并尽力删除残留镜像 |
| false | None | 镜像 = 源 + `enable_web_search = false` |
| true | Some(id) | 镜像 = 源 + `default_model = "<id>"` |
| false | Some(id) | 镜像 = 源 + 两个键 |

`skip_approvals` 不进镜像。

#### Scenario: 联网搜索开启且没有默认模型

- **WHEN** `web_search = true` 且 `default_model = None`
- **THEN** 不生成镜像、不带 `--config`，并尽力删除残留镜像

#### Scenario: 只关闭联网搜索

- **WHEN** `web_search = false` 且 `default_model = None`
- **THEN** 镜像是源内容加根表键 `enable_web_search = false`

#### Scenario: 只有默认模型

- **WHEN** `web_search = true` 且 `default_model = Some(id)`
- **THEN** 镜像是源内容加根表键 `default_model = "<id>"`，不写 `enable_web_search`

#### Scenario: 两项都覆盖

- **WHEN** `web_search = false` 且 `default_model = Some(id)`
- **THEN** 镜像是源内容加这两个根表键

### Requirement: 联网搜索开启时不替用户放宽

`web_search` 是联网搜索总开关（`Profile.web_search`，全局、默认开）。开启时 MUST NOT 写 `enable_web_search` 这个键：
Science 自己配置里的任何值（包括手写的 `enable_web_search = false`）原样生效。

#### Scenario: 用户在 Science 配置里手写了关闭

- **WHEN** 总开关开启，而源配置里有 `enable_web_search = false`
- **THEN** CSSwitch 不覆盖这个值

### Requirement: 默认模型只在第三方模式下有值

`default_model` MUST 只在实例处于第三方模式时有值：该渠道清单里承载默认槽的那一条的 id
（`StaticProfileResolver::default_menu_id`，见 [model-menu](../model-menu/spec.md)）。官方模式下 MUST 是 `None`——那是用户自己的设置；
沙箱总是第三方模式。渠道默认槽为空取不到 id 时也是 `None`，MUST NOT 因此拦截启动。

#### Scenario: 官方模式

- **WHEN** 实例处于官方模式
- **THEN** `default_model` 是 `None`

#### Scenario: 第三方模式

- **WHEN** 实例处于第三方模式且渠道默认槽有值
- **THEN** `default_model` 是该渠道清单的默认条目 id

#### Scenario: 渠道默认槽为空

- **WHEN** 第三方模式下取不到默认条目 id
- **THEN** `default_model` 是 `None`，启动不被拦截

### Requirement: 覆盖项只有一个取值处

覆盖项的取值 MUST 只有 `control::launch_overrides(profile, instance)` 一处。五个启动点——`switch_mode`、`start_science`、
`set_web_search`（含存盘前的镜像预生成）、`start_sandbox`、`switch_sandbox_mode`——都 SHALL 用它。
在某个启动点另写一份判断，两个实例就会拿到不一致的默认模型。

#### Scenario: 两个实例各取各的模式

- **WHEN** 正式实例处于官方模式，而沙箱走某个渠道
- **THEN** 正式实例的 `default_model` 是 `None`，沙箱的是 `claude-sonnet-19`

### Requirement: 镜像由源加覆盖键构成

镜像 MUST 等于源文件全部内容加覆盖的键（原本有就覆盖，没有就新增），用 `toml_edit` 读写；
源不存在时镜像只有覆盖的键。每次启动 MUST 重新生成。

#### Scenario: 源里原本就有被覆盖的键

- **WHEN** 源里有 `enable_web_search = true` 或自己的 `default_model`
- **THEN** 镜像里该键被覆盖，不出现重复键

#### Scenario: 源含各种 TOML 结构

- **WHEN** 源含根键、注释、表、行内表、多行字符串、表数组
- **THEN** 镜像除覆盖的键外逐值相等，覆盖的键在根表

#### Scenario: 源文件不存在

- **WHEN** 需要镜像而源 `config.toml` 不存在
- **THEN** 镜像只有覆盖的键，CSSwitch 不替用户创建源文件

### Requirement: 真实 Science 目录只读

对真实 `~/.claude-science/` MUST 只读 `config.toml` 一个文件，MUST NOT 写或创建任何东西。
官方实例的 source 是 `~/.claude-science/config.toml`，mirror 是 `~/.csswitch/science-config/official.toml`。

#### Scenario: 生成镜像之后

- **WHEN** 镜像生成完成
- **THEN** 源目录零写入

### Requirement: 镜像安全写出

镜像带着用户原配置的全部内容。写出 MUST 走同目录临时文件 → `0600` → rename，所在目录 MUST 是 `0700`。

#### Scenario: 镜像的权限

- **WHEN** 镜像被写出
- **THEN** 镜像文件是 `0600`，所在目录是 `0700`

### Requirement: 源配置读不了时拒绝

需要镜像（联网搜索关闭，或第三方模式）而源 `config.toml` 读不了或解析不了时，MUST 报错并拒绝启动 / 拒绝切换开关，
MUST NOT 写镜像，MUST NOT 回退成空配置。报错给路径、行列与解析器原话，MUST NOT 带出错行的内容（可能是带口令的代理地址）；
并按实际的覆盖项说明为什么需要这个文件，不笼统地只提联网搜索。

#### Scenario: 源无法解析

- **WHEN** 需要镜像而源 `config.toml` 无法解析
- **THEN** 启动或切换被拒绝，镜像未写出
- **AND** 报错包含路径、行列与解析器原话，不包含文件内容

#### Scenario: 源读不了

- **WHEN** 需要镜像而源 `config.toml` 存在但读不了
- **THEN** 启动或切换被拒绝，不当成空配置处理

#### Scenario: 第三方模式下开启联网搜索

- **WHEN** 第三方模式下开启联网搜索而源无法解析
- **THEN** 同样被拒（此时仍要预生成镜像）

### Requirement: 联网搜索总开关按固定顺序切换

切换总开关（`POST /control/web-search`）的顺序 MUST 固定：

1. 判定：值没变 → 不动；官方实例在运行且有活跃会话且未带 `force` → 拒绝。
2. 关闭时先试生成镜像。
3. 存盘并更新运行时快照。
4. 官方实例在运行就 stop → start。

拒绝与镜像失败都在存盘**之前**，开关不会停在「已保存但没生效」。重启失败 MUST NOT 回滚（回滚再重启可能循环），原文报出当前真实状态。
免登录沙箱不自动重启，只在响应里置 `sandbox_restart_required`。请求体的 `enabled` / `force` MUST 是布尔。

#### Scenario: 值没有变化

- **WHEN** 请求的值与当前值相同
- **THEN** 配置与 Science 都不动

#### Scenario: 有活跃会话且未确认

- **WHEN** 官方实例在运行、有活跃会话且请求未带 `force`
- **THEN** 请求被拒绝并报告会话数，配置文件逐字节不变，没有 stop / serve

#### Scenario: 镜像生成失败

- **WHEN** 关闭开关时源配置无法解析
- **THEN** 请求被拒绝，配置文件逐字节不变，没有 stop / serve

#### Scenario: 存盘后重启失败

- **WHEN** 存盘成功而 Science 重启失败
- **THEN** 配置不回滚，响应原文报出当前真实状态

#### Scenario: 沙箱正在运行

- **WHEN** 开关切换成功而免登录沙箱在运行
- **THEN** 沙箱不被自动重启，响应里 `sandbox_restart_required` 为真

#### Scenario: 关闭开关后 Science 起不来

- **WHEN** 关闭开关后 Science 启动失败
- **THEN** 原文报出；官方模式下把开关拨回开启再启动，就是不带 `--config` 的原始启动方式

### Requirement: 回到无覆盖状态时清理残留镜像

切回「官方模式 + 联网搜索开启」而 Science 没在运行时，`switch_mode` MUST 顺手删掉残留镜像
（否则它会留到下次启动，看起来像仍在生效）。

#### Scenario: Science 未运行时切回官方模式

- **WHEN** 联网搜索开启、Science 未运行，用户切回官方模式
- **THEN** 残留镜像被删除

#### Scenario: 官方模式下重新开启联网搜索

- **WHEN** 官方模式下重新开启联网搜索
- **THEN** `serve` 整行回到不带 `--config` 的主线，镜像被删

### Requirement: 旧的渠道级联网搜索开关只迁移一次

旧的渠道级 `kimi.web_search` MUST 只在解析入口迁移一次（`Profile::parse`）：顶层没有 `web_search` 且 `kimi.web_search == false`
→ 总开关为关；顶层字段一旦存在就以它为准。`POST /control/config` 收到 `web_search` MUST 显式报错并指向新端点，MUST NOT 静默忽略。

#### Scenario: 只有旧字段

- **WHEN** 配置顶层没有 `web_search` 且 `kimi.web_search == false`
- **THEN** 总开关为关

#### Scenario: 顶层字段已存在

- **WHEN** 配置顶层有 `web_search`
- **THEN** 以顶层字段为准，旧字段不参与

#### Scenario: 向旧端点提交开关

- **WHEN** `POST /control/config` 的请求体带 `web_search`
- **THEN** 返回明确的错误并指向 `POST /control/web-search`
