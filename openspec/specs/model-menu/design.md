# model-menu 设计注记

## 签名

```text
static_profile::MENU_POSITIONS = [                        # (家族名 = 角色, id)；顺序 = 去重的先后，默认槽在前
    ("sonnet", "claude-sonnet-19"),
    ("opus",   "claude-opus-19"),
    ("haiku",  "claude-haiku-19") ]
StaticProfileResolver::models_response() -> Value
StaticProfileResolver::default_menu_id() -> &str          # 用户配置生成的目录下恒为 claude-sonnet-19
models::science_safe_display_name(id, candidate) -> String
StaticProfileResolver::resolve(requested)                 # 不因本规格而改
```

## 清单示例

| 默认 / 高质量 / 快速 / Fable | 清单 | `default_menu_id` |
| --- | --- | --- |
| K3 / 空 / K2.8 / 空 | sonnet-19=K3，haiku-19=K2.8 | `claude-sonnet-19` |
| k3-256k / k3 / kimi-for-coding / 空（出厂的 Kimi） | sonnet-19=k3-256k，opus-19=k3，haiku-19=kimi-for-coding | `claude-sonnet-19` |
| A / B / C / D | sonnet-19=A，opus-19=B，haiku-19=C，`claude-csswitch-D`=D | `claude-sonnet-19` |
| A / A / A / 空 | sonnet-19=A | `claude-sonnet-19` |
| A / B / B / 空 | sonnet-19=A，opus-19=B | `claude-sonnet-19` |
| A / 空 / C / C | sonnet-19=A，haiku-19=C | `claude-sonnet-19` |
| A / 空 / 空 / D | sonnet-19=A，`claude-csswitch-D`=D | `claude-sonnet-19` |

## 理由与教训

- 错误：为了让 Science 的默认模型「找得到」，把官方 id 列回清单 —— 两个真实模型在菜单里出现七条。
- 错误：去重从高质量槽开始（opus → sonnet → haiku），想让主力模型显示在第一位 —— 高质量槽留空时
  默认条目变成 `claude-opus-19`，在免登录沙箱里一切正常，换成订阅档不够格的真实登录，Science 就把
  默认模型换成清单里没有的 id。

## 必需测试

- `static_profile`：上表各种配置的清单与 `default_menu_id`（恒为 `claude-sonnet-19`）；
  清单里没有 Science 自己会用的那几个官方 id；
  **清单里每个 id 经 `resolve` 得到的上游模型等于该条目显示的模型**；两个显式槽共用一个模型而显示名不同时
  只列一次、用第一个槽的名字；官方 id 与旧 selector 仍可解析；外部目录的默认 route 不在主菜单位置时
  以自己的 selector 作默认条目；三个位置 id 是「家族名 + 同一个 1–2 位数字版本号」并按家族名解析到
  对应角色；显示名留空的槽仍出现在清单里。
- `profile`：0.10.0 写出的 `service.v1.json` 读写一轮逐字节不变。
- `control`：`launch_overrides` 的默认条目对两个实例各取各的渠道。
- `models`：小写连字符写法的名字被换成带空格的写法且换出来的不再是那种写法；没有连字符、带大写、
  带空格或点号、非 ASCII 的名字原样不动。
- loopback：第三方模式的清单含位置 id，不含 Science 自己会用的官方 id，也不把同一个模型用
  `claude-csswitch-*` 再列一遍；以清单里每个 id 为 model 的请求到达它显示的模型；以官方 id 或旧 selector
  为 model 的请求仍到达对应槽位，不认识的 id 不被解析；保存槽位不重启 Science、保存结果说明重启后生效；
  另配或清空高质量槽 / 快速槽并重启，镜像里的 `default_model` 都是 `claude-sonnet-19`。
- 这些测试只证明网关一侧。Science 的菜单规则变了，它们不会自己变红：Science 升级后要在真机上
  重跑 `test/LIVE_ACCEPTANCE.md` §H。

## Wrong vs Correct

### Wrong

```rust
// 清单与解析各走各的:清单读角色绑定,另给位置 id 建一张别名表去解析
data.push(json!({"id": "claude-opus-19", "display_name": quality_slot.label(), ..}));
aliases.insert("claude-opus-19", default_selector.clone()); // 两处一旦不同步,菜单显示 A、请求到 B
```

### Correct

```rust
// 清单里的每一条,都是「这个 id 实际会解析到的那条 route」
for (_role, id) in MENU_POSITIONS {
    let Some(index) = self.resolved_index(id) else { continue };
    // 已列过的 route 跳过;显示名取自这条 route
}
```

## 模型解析

用户在 WebUI 里为每个渠道填四个槽(默认 / 高质量 / 快速 / Fable),
每槽是「上游模型 ID + 显示名」,空槽继承默认槽。服务据此生成一份静态模型目录:

- 每个不同的模型 ID 产出一条 route,selector 形如 `claude-csswitch-<model-id>`;
- 四个角色(sonnet / opus / haiku / fable)绑到对应 selector:默认槽 = sonnet,高质量 = opus,
  快速 = haiku,Fable = fable。

目录带指纹校验,生成与解析共用同一套算法。

**请求里的模型名怎么解析**(`StaticProfileResolver::resolve`):精确 selector → 历史别名 →
`claude-<opus|sonnet|haiku|fable>-<版本>` 这种写法按家族名落到角色绑定。Science 自己的辅助调用
固定用官方名字(如 `claude-haiku-4-5-20251001`),走的就是最后一条。

**模型清单列什么**(`/v1/models`、`/sandbox/v1/models`):每个不同的模型只列一次。

```text
claude-sonnet-19     默认槽的模型
claude-opus-19       高质量槽的模型    留空(继承默认槽)或与默认槽同模型时不列
claude-haiku-19      快速槽的模型      留空或与前面是同一个模型时不列
claude-csswitch-…    Fable 槽的模型    与前面都不同时才列
```

- 前三行是主菜单的三个位置。这三个 id 不是哪个真实型号,按上一段的规则由家族名解析到对应槽位,
  所以清单里显示的模型与请求实际到达的模型由同一份角色绑定得出。
- Science 自己会用的官方 id(新会话出厂默认的 `claude-opus-5`、辅助调用写死的那几个)不出现在
  清单里 —— 列出来,菜单里同一个模型就又有好几条。它们仍然可以解析:旧会话与辅助调用照常路由。
- 去重的先后是默认槽 → 高质量槽 → 快速槽(`static_profile::MENU_POSITIONS`),所以默认槽的模型总在
  sonnet 位,交给 Science 的 `default_model`(见 [science-launch-overrides](../science-launch-overrides/design.md)「启动覆盖项」)恒为 `claude-sonnet-19`。
  不让默认模型落在 opus 位是有意的:Science 0.1.60 对订阅档不够格的登录(免费档、部分 team /
  enterprise 席位)会把名字里带 `opus` 的默认模型换成 `claude-sonnet-4-6`,那个 id 不在清单里
  (静态检查所得;免登录沙箱是虚拟的 max 档,不受影响)。Science 显示菜单时自己按家族排
  (opus、sonnet、haiku),与这里的先后无关。

为什么是这种写法:Science 0.1.60 在第三方模式下只用这份清单生成菜单,主菜单的三个位置按 id 的写法分配
(`claude-opus-*` / `claude-sonnet-*` / `claude-haiku-*` 各取版本号最新的一条,要求写成 `claude-<家族>-<数字>[-<数字>]`),
其余条目进 More models;位置下方的英文说明也按家族写死。版本号取 `19`:它能被这样解析,网关按家族名认得出,
又不是任何真实模型的名字。

**显示名**:Science 把小写连字符写法的显示名(`k3-256k`、`deepseek-v4-pro`)当成内部模型 id,带着这种
名字的条目会被它整条从菜单里丢掉(2026-10-10 用 0.1.60 真机确认)。槽的显示名留空时回落到模型 ID,
正好是这种写法,所以清单里一律换成它肯显示的写法(`K3 256k`、`DeepSeek V4 Pro`);模型 id 不变。

**会话与模型 id**:会话会记住它用的 id。

- 升级前建立的会话记的是旧 id(官方 id 如 `claude-opus-5`,或旧的 `claude-csswitch-*` selector)。
  它们不在清单里,请求仍按上面的规则解析到对应槽位。以 `claude-opus-5` 建立的会话在真机上看过
  (2026-10-10):模型按钮显示的是这个 id 本身而不是模型名,追问正常回复。
- 不支持同一个会话跨供应商继续:切换供应商之后请新开会话。
- Science 还会在浏览器里记住用户最后一次手动选的模型,之后的新会话默认用它,优先于 `default_model`;
  记住的 id 不在清单里时它会自己清掉。所以「新会话默认是默认槽的模型」只对没手动选过模型的浏览器成立。
- 保存槽位会改变各个 id 背后的模型。网关的清单与路由立刻按新配置走;运行中的 Science 手里的菜单
  不保证跟着刷新。保存不重启 Science(会打断会话),保存结果统一提示「重启 Science 后生效」。

**升级与回滚**:

- 升级后,菜单立刻变成不重复的形态。Science 要重启一次才拿到 `default_model`。正常的升级路径
  (控制台「退出服务」)本来就会先停掉两个实例;若升级时 Science 仍在运行,它的默认模型还是
  `claude-opus-5`,不在新清单里,重启后恢复。
- 回滚到旧版本:配置文件不用动(这一版没有新增配置键)。新版本建立的会话(`claude-sonnet-19` 等)
  在旧网关下仍按家族名解析到同一个槽位,只是不在旧清单里。

**证据边界**:上面关于 Science 的说法来自对已安装的 0.1.60 二进制的只读静态检查,其中菜单位置、
默认模型、显示名被丢弃这几条在 2026-10-10 的真机上确认过。真机验收的步骤见
[test/LIVE_ACCEPTANCE.md](../../../test/LIVE_ACCEPTANCE.md) §H,结论也记在那一节。其它版本的 Science 没有核对过:
它的菜单规则变了,这里的做法就要跟着复查。
