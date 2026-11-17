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
