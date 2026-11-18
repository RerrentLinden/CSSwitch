# model-menu Specification

## Purpose

第三方模式下 Science 的模型菜单与默认模型全部由网关返回的模型清单决定；清单与请求解析各说各话时，
用户选的模型与实际到达的模型就不是同一个。本规格约束清单的组成、默认条目、显示名与解析的一致性。
改动 `static_profile.rs` 的 `models_response` / `MENU_POSITIONS`，或 `/v1/models`（含 `/sandbox/v1/models`）返回的任何字段时适用。

Science 一侧的依据（0.1.60，只读静态检查，出处见 [design.md](./design.md)「模型解析」）：

- 主菜单三个位置按 id 的写法分配：`claude-opus*` / `claude-sonnet*` / `claude-haiku*` 各取能被
  `^claude-(opus|sonnet|haiku)-N(-M)?(-YYYYMMDD)?$` 解析的最新一条；其余进 More models。
- 新会话的默认模型只取配置里的 `default_model`，不看它在不在清单里。
- 显示名是小写连字符写法（判据 `^[a-z][a-z0-9]*(-[a-z0-9]+)+$`）或以 emoji 开头的条目，被它整条从菜单里丢掉
  （2026-10-10 真机确认 `k3-256k`）。

## Requirements

### Requirement: 四槽生成静态模型目录

用户为每个渠道填四个槽（默认 / 高质量 / 快速 / Fable），每槽是「上游模型 ID + 显示名」，空槽继承默认槽。
服务 MUST 据此生成一份静态模型目录：每个不同的模型 ID 产出一条 route，selector 形如 `claude-csswitch-<model-id>`；
四个角色绑到对应 selector：默认槽 = sonnet，高质量 = opus，快速 = haiku，Fable = fable。
目录 MUST 带指纹校验，生成与解析共用同一套算法。

#### Scenario: 空槽

- **WHEN** 高质量、快速或 Fable 槽留空
- **THEN** 该槽的角色绑定到默认槽的模型

#### Scenario: 两个槽填同一个模型

- **WHEN** 两个槽的模型 ID 相同
- **THEN** 目录里只有一条 route，两个角色都绑到它

### Requirement: 模型名的解析顺序

请求里的模型名 MUST 按这个顺序解析（`StaticProfileResolver::resolve`）：精确 selector → 历史别名 →
`claude-<opus|sonnet|haiku|fable>-<版本>` 这种写法按家族名落到角色绑定。

#### Scenario: Science 的辅助调用

- **WHEN** Science 的辅助调用用官方名字（如 `claude-haiku-4-5-20251001`）发来请求
- **THEN** 请求按家族名落到快速槽的模型

#### Scenario: 精确 selector

- **WHEN** 请求的 model 是某条 route 的 selector
- **THEN** 直接解析到这条 route

### Requirement: 清单按位置去重组成

清单 MUST 按 `MENU_POSITIONS` 的顺序组成（sonnet → opus → haiku，即默认槽 → 高质量槽 → 快速槽）：每个位置取它**解析到**的 route，
已经列过的 route 不再列；其余没列过的 route（只可能来自 Fable 槽）以自己的 `claude-csswitch-*` selector 列出。
官方 id（`claude-opus-5` 等）MUST NOT 列出：列回去，菜单里同一个模型又是好几条。各种槽位组合的清单见 [design.md](./design.md)「清单示例」。

#### Scenario: 四个槽各不相同

- **WHEN** 四个槽分别是 A / B / C / D
- **THEN** 清单是 `claude-sonnet-19`=A、`claude-opus-19`=B、`claude-haiku-19`=C、`claude-csswitch-D`=D

#### Scenario: 高质量或快速槽留空或重复

- **WHEN** 高质量 / 快速槽留空，或与前面的槽是同一个模型
- **THEN** 那个位置不列
- **AND** 带着那个位置的 id 来的请求仍按角色绑定解析

#### Scenario: Fable 槽

- **WHEN** Fable 槽的模型与前三个都不同
- **THEN** 它以 `claude-csswitch-<slug>` 列出（进 More models）；与其中一个相同则不列

#### Scenario: 清单里没有官方 id

- **WHEN** 生成第三方模式的清单
- **THEN** 清单不含 Science 自己会用的官方 id，也不把同一个模型用 `claude-csswitch-*` 再列一遍

### Requirement: 默认条目恒在 sonnet 位

去重 MUST 从默认槽开始，默认条目恒在 sonnet 位（`claude-sonnet-19`），不会被别的槽挤到别的位置上。
`default_menu_id` MUST NOT 是名字里带 `opus` 的 id——Science 0.1.60 对订阅档不够格的登录（免费档、部分 team / enterprise 席位）
会把这样的默认模型换成 `claude-sonnet-4-6`，那个 id 不在清单里（静态检查所得；免登录沙箱是虚拟的 max 档，不受影响，
所以这条只在正式实例的真实登录上才暴露）。Science 显示菜单时自己按家族排序，清单里的先后不决定显示顺序。

#### Scenario: 用户配置生成的目录

- **WHEN** 目录由用户配置生成，无论高质量槽与快速槽是否留空
- **THEN** `default_menu_id` 是 `claude-sonnet-19`

#### Scenario: 外部目录的默认 route 不在主菜单位置

- **WHEN** 外部生成的目录里默认 route 不绑定任何主菜单角色
- **THEN** 它以自己的 selector 列出并排最前，`default_menu_id` 就是这个 selector，不悄悄换成第一个主菜单位置

### Requirement: 清单语义等于解析语义

位置 id MUST NOT 另建别名表，SHALL 经 `official_role_alias` 按家族名落到角色绑定；`models_response` 也经同一条解析路径取 route。
清单里显示的模型名 MUST 属于 `resolve(id)` 返回的那个上游模型。

#### Scenario: 按清单里的 id 发请求

- **WHEN** 请求的 model 是清单里的任意一个 id
- **THEN** 请求到达该条目显示的那个上游模型

### Requirement: 旧 id 仍可解析

官方 id 与旧的 `claude-csswitch-<slug>` 不在清单里，但 `resolve` 的结果 MUST 不变
（旧会话，以及 Science 固定用官方 id 发起的辅助调用）。`resolve(requested)` 不因本规格而改。

#### Scenario: 请求带官方 id 或旧 selector

- **WHEN** 请求的 model 是官方 id 或旧的 `claude-csswitch-<slug>`
- **THEN** 照常解析到对应槽位

#### Scenario: 不认识的 id

- **WHEN** 请求的 model 是不认识的 id
- **THEN** 它不被解析

### Requirement: 位置 id 的版本号

位置 id 的版本号（`19`，三个位置用同一个）MUST 同时满足三条，换之前逐条确认：`official_role_alias` 返回对应角色；
能被 Science 的主菜单正则解析（1–2 位数字）；不是任何真实模型的名字。

#### Scenario: 三个位置 id 的写法

- **WHEN** 检查 `MENU_POSITIONS`
- **THEN** 三个 id 都是「家族名 + 同一个 1–2 位数字版本号」，并按家族名解析到对应角色

### Requirement: 同一模型只列一条

同一个模型出现在多个显式槽里时 MUST 只有一条 route，显示名取 `默认 → 高质量 → 快速 → Fable` 顺序里第一个槽的。

#### Scenario: 两个槽共用一个模型而显示名不同

- **WHEN** 两个显式槽指向同一个模型，显示名不同
- **THEN** 清单只列一次，用顺序里第一个槽的显示名

### Requirement: 显示名经过改写

清单里的名字 MUST 一律过 `models::science_safe_display_name`。槽的显示名留空时回落到模型 ID，
而 `deepseek-v4-pro` 这种写法会让 Science 丢掉整条——默认模型若被丢掉，新会话就没有默认模型。模型 id 不因改写而变。

#### Scenario: 显示名留空或是小写连字符写法

- **WHEN** 槽的显示名留空，或是小写连字符写法
- **THEN** 清单里换成 Science 肯显示的写法（带空格），换出来的不再是那种写法，条目仍在清单里

#### Scenario: 其它写法的显示名

- **WHEN** 显示名没有连字符、带大写、带空格或点号，或含非 ASCII
- **THEN** 原样不动

#### Scenario: 显示名是占位词

- **WHEN** 显示名是占位词 `default`
- **THEN** 换成上游模型名，再过一遍显示名改写

### Requirement: 保存槽位不重启 Science

`save_config` MUST NOT 重启 Science（会打断会话）。保存结果的 `note` MUST 固定是一句「已保存,重启 Science 后生效。」，
不按改了什么另拼说明（界面文案从简，见 [console-webui](../console-webui/spec.md)）。

#### Scenario: 保存槽位

- **WHEN** 用户保存模型槽
- **THEN** Science 不被重启，保存结果的 `note` 是「已保存,重启 Science 后生效。」

#### Scenario: 改槽后重启

- **WHEN** 另配或清空高质量槽 / 快速槽并重启
- **THEN** 镜像里的 `default_model` 都是 `claude-sonnet-19`

### Requirement: 不新增配置键

`ModelSlot` MUST 只有 `model_id` 与 `display_name` 两个字段。0.10.0 写出的 `service.v1.json` 读写一轮后 MUST 逐字节不变。

#### Scenario: 读写旧配置

- **WHEN** 读入 0.10.0 写出的 `service.v1.json` 再写回
- **THEN** 文件逐字节不变
