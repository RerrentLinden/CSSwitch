use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const ENV_NAME: &str = "CSSWITCH_STATIC_MODEL_CATALOG_V1";
const MAX_ENCODED_BYTES: usize = 64 * 1024;
const MAX_ROUTES: usize = 64;
const MAX_SELECTOR_BYTES: usize = 160;
const MAX_DISPLAY_BYTES: usize = 256;
const MAX_UPSTREAM_BYTES: usize = 512;

/// Science 主菜单的三个位置:(家族名, 清单里的 id)。家族名也是网关的角色名:sonnet = 默认槽,
/// opus = 高质量槽,haiku = 快速槽。
///
/// 版本号为什么是 `19`:Science 的菜单排序只认家族名后面跟 1–2 位数字版本号的 id,这样的条目才占
/// 主菜单的位置;网关按家族名([`official_role_alias`])把它解析到对应槽位;它又不是任何真实模型的名字。
///
/// 顺序是**去重的优先顺序**:默认槽 → 高质量槽 → 快速槽。同一个模型填在几个槽里时,它只占最靠前的
/// 那个位置,所以默认模型永远在 sonnet 位:交给 Science 的 `default_model` 恒为 `claude-sonnet-19`,
/// 不随别的槽怎么填而变。
///
/// 默认模型不能落在 opus 位:Science 0.1.60 对订阅档不够格的登录(免费档、部分 team /
/// enterprise 席位),会把名字里带 `opus` 的默认模型换成 `claude-sonnet-4-6`(静态检查),
/// 那个 id 不在清单里。免登录沙箱是虚拟的 max 档不受影响,正式实例的真实登录会。
///
/// Science 自己按家族重排菜单(opus → sonnet → haiku),这里的顺序不影响显示顺序。
pub(crate) const MENU_POSITIONS: [(&str, &str); 3] = [
    ("sonnet", "claude-sonnet-19"),
    ("opus", "claude-opus-19"),
    ("haiku", "claude-haiku-19"),
];

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningRoundTrip {
    #[default]
    None,
    Native,
    CsswitchOpaque,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRouteCapabilities {
    #[serde(default)]
    reasoning_round_trip: ReasoningRoundTrip,
    #[serde(default)]
    forced_tool_choice: Option<bool>,
    #[serde(default)]
    structured_output: Option<bool>,
    #[serde(default)]
    vision: Option<bool>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRoute {
    selector_id: String,
    display_name: String,
    upstream_model: String,
    supports_tools: Option<bool>,
    #[serde(default)]
    capabilities: WireRouteCapabilities,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRoles {
    sonnet: String,
    opus: String,
    haiku: String,
    fable: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireLegacyAlias {
    alias: String,
    selector_id: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireCatalog {
    schema_version: u32,
    adapter: String,
    catalog_fp: String,
    default_selector_id: String,
    routes: Vec<WireRoute>,
    role_bindings: WireRoles,
    legacy_aliases: Vec<WireLegacyAlias>,
}

#[derive(Clone, Debug)]
pub struct StaticProfileResolver {
    routes: Vec<WireRoute>,
    adapter: String,
    catalog_fp: String,
    default_selector_id: String,
    role_bindings: WireRoles,
    by_selector: BTreeMap<String, usize>,
    legacy_exact: BTreeMap<String, usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolutionKind {
    ExactSelector,
    LegacyExact,
    OfficialRole,
}

#[derive(Clone, Copy, Debug)]
pub struct ResolvedRoute<'a> {
    route: &'a WireRoute,
    pub kind: ResolutionKind,
}

impl ResolvedRoute<'_> {
    pub fn upstream_model(&self) -> &str {
        &self.route.upstream_model
    }

    pub fn reasoning_round_trip(&self) -> ReasoningRoundTrip {
        self.route.capabilities.reasoning_round_trip
    }

    pub fn supports_forced_tool_choice(&self) -> Option<bool> {
        self.route.capabilities.forced_tool_choice
    }

    pub fn supports_structured_output(&self) -> Option<bool> {
        self.route.capabilities.structured_output
    }

    pub fn supports_vision(&self) -> Option<bool> {
        self.route.capabilities.vision
    }
}

fn valid_text(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn valid_selector(value: &str) -> bool {
    valid_text(value, MAX_SELECTOR_BYTES)
        && value.starts_with("claude-")
        && value.is_ascii()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

pub(crate) fn official_role_alias(value: &str) -> Option<&'static str> {
    if !value.is_ascii() || value.len() > MAX_SELECTOR_BYTES {
        return None;
    }
    let parts: Vec<&str> = value.split('-').collect();
    if parts.len() < 3 || parts[0] != "claude" {
        return None;
    }
    let role = match parts[1] {
        "sonnet" => "sonnet",
        "opus" => "opus",
        "haiku" => "haiku",
        "fable" => "fable",
        _ => {
            let mut historical = &parts[1..];
            if historical.last().is_some_and(|part| {
                part.len() == 8 && part.bytes().all(|byte| byte.is_ascii_digit())
            }) {
                historical = &historical[..historical.len() - 1];
            }
            if historical.len() < 2 || historical.len() > 4 {
                return None;
            }
            let role = match historical.last().copied() {
                Some("sonnet") => "sonnet",
                Some("opus") => "opus",
                Some("haiku") => "haiku",
                _ => return None,
            };
            if historical[..historical.len() - 1].iter().any(|part| {
                part.is_empty() || part.len() > 2 || !part.bytes().all(|byte| byte.is_ascii_digit())
            }) {
                return None;
            }
            return Some(role);
        }
    };
    let mut version = &parts[2..];
    if version
        .last()
        .is_some_and(|part| part.len() == 8 && part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        version = &version[..version.len() - 1];
    }
    if version.is_empty()
        || version.len() > 3
        || version.iter().any(|part| {
            part.is_empty() || part.len() > 2 || !part.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return None;
    }
    Some(role)
}

impl StaticProfileResolver {
    pub fn from_json(encoded: &str) -> Result<Self, String> {
        if encoded.is_empty() || encoded.len() > MAX_ENCODED_BYTES {
            return Err("static model catalog 为空或超过 64 KiB".into());
        }
        let catalog: WireCatalog = serde_json::from_str(encoded)
            .map_err(|error| format!("static model catalog JSON 非法：{error}"))?;
        if catalog.schema_version != 1
            || !valid_text(&catalog.adapter, 64)
            || !catalog.adapter.is_ascii()
            || catalog.catalog_fp.len() != 64
            || !catalog
                .catalog_fp
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || catalog.routes.is_empty()
            || catalog.routes.len() > MAX_ROUTES
        {
            return Err("static model catalog schema 或 route 数量非法".into());
        }
        let computed_fp = wire_catalog_fingerprint(&catalog);
        if catalog.catalog_fp != computed_fp {
            return Err(format!(
                "static model catalog fingerprint 不匹配（claimed={} computed={}）",
                &catalog.catalog_fp[..12],
                &computed_fp[..12],
            ));
        }
        let mut by_selector = BTreeMap::new();
        for (index, route) in catalog.routes.iter().enumerate() {
            if !valid_selector(&route.selector_id)
                || !valid_text(&route.display_name, MAX_DISPLAY_BYTES)
                || !valid_text(&route.upstream_model, MAX_UPSTREAM_BYTES)
                || route.supports_tools == Some(false)
                || by_selector
                    .insert(route.selector_id.clone(), index)
                    .is_some()
            {
                return Err("static model catalog route 非法或 selector 重复".into());
            }
        }
        if !by_selector.contains_key(&catalog.default_selector_id) {
            return Err("static model catalog default selector 悬空".into());
        }
        for selector in [
            &catalog.role_bindings.sonnet,
            &catalog.role_bindings.opus,
            &catalog.role_bindings.haiku,
            &catalog.role_bindings.fable,
        ] {
            if !by_selector.contains_key(selector) {
                return Err("static model catalog role binding 悬空".into());
            }
        }
        let mut legacy_exact = BTreeMap::new();
        for legacy in &catalog.legacy_aliases {
            if !valid_text(&legacy.alias, MAX_UPSTREAM_BYTES)
                || !legacy.alias.is_ascii()
                || by_selector.contains_key(&legacy.alias)
                || legacy_exact.contains_key(&legacy.alias)
            {
                return Err("static model catalog legacy alias 非法或重复".into());
            }
            let Some(index) = by_selector.get(&legacy.selector_id).copied() else {
                return Err("static model catalog legacy alias selector 悬空".into());
            };
            legacy_exact.insert(legacy.alias.clone(), index);
        }
        Ok(Self {
            routes: catalog.routes,
            adapter: catalog.adapter,
            catalog_fp: catalog.catalog_fp,
            default_selector_id: catalog.default_selector_id,
            role_bindings: catalog.role_bindings,
            by_selector,
            legacy_exact,
        })
    }

    pub fn adapter(&self) -> &str {
        &self.adapter
    }

    pub fn catalog_fp(&self) -> &str {
        &self.catalog_fp
    }

    pub fn resolve(&self, requested: &str) -> Option<ResolvedRoute<'_>> {
        if let Some(index) = self.by_selector.get(requested) {
            return Some(ResolvedRoute {
                route: &self.routes[*index],
                kind: ResolutionKind::ExactSelector,
            });
        }
        if let Some(index) = self.legacy_exact.get(requested) {
            return Some(ResolvedRoute {
                route: &self.routes[*index],
                kind: ResolutionKind::LegacyExact,
            });
        }
        let role = official_role_alias(requested)?;
        let selector = match role {
            "sonnet" => &self.role_bindings.sonnet,
            "opus" => &self.role_bindings.opus,
            "haiku" => &self.role_bindings.haiku,
            "fable" => &self.role_bindings.fable,
            _ => return None,
        };
        let index = self.by_selector.get(selector)?;
        Some(ResolvedRoute {
            route: &self.routes[*index],
            kind: ResolutionKind::OfficialRole,
        })
    }

    /// `requested` 解析到的 route 在 `routes` 里的下标。
    fn resolved_index(&self, requested: &str) -> Option<usize> {
        let resolved = self.resolve(requested)?;
        self.by_selector.get(&resolved.route.selector_id).copied()
    }

    /// 清单里的条目:(id, route 下标),按 [`MENU_POSITIONS`] 的去重优先顺序。
    ///
    /// 每个不同的模型只出现一次。先是主菜单的三个位置:每个位置的 id 解析到哪条 route
    /// (直接问 [`Self::resolve`],所以清单显示的与推理实际走的不可能各说各话),那条 route
    /// 还没列过就列在这个 id 下;同一条 route 被几个角色共用时只占最靠前的那个位置。
    /// 剩下还没列过的 route(用户配置里只可能来自 Fable 槽)用自己的 selector 列出,
    /// 在 Science 里进 More models。
    fn menu_entries(&self) -> Vec<(&str, usize)> {
        let mut entries: Vec<(&str, usize)> = Vec::new();
        let listed = |entries: &[(&str, usize)], index: usize| {
            entries.iter().any(|(_, listed)| *listed == index)
        };
        for (_role, id) in MENU_POSITIONS {
            let Some(index) = self.resolved_index(id) else {
                continue;
            };
            if !listed(&entries, index) {
                entries.push((id, index));
            }
        }
        for (index, route) in self.routes.iter().enumerate() {
            if !listed(&entries, index) {
                entries.push((route.selector_id.as_str(), index));
            }
        }
        entries
    }

    fn default_route_index(&self) -> Option<usize> {
        self.by_selector.get(&self.default_selector_id).copied()
    }

    /// 清单里承载默认模型的那一条的 id。Science 的 `default_model` 要指向它:
    /// 第三方地址下 Science 不看清单里有什么,默认模型只认配置里写的那个 id,
    /// 指到清单里没有的 id 上,界面就显示默认模型不可用。
    pub fn default_menu_id(&self) -> &str {
        let default_index = self.default_route_index();
        self.menu_entries()
            .into_iter()
            .find(|(_, index)| Some(*index) == default_index)
            .map_or(self.default_selector_id.as_str(), |(id, _)| id)
    }

    pub fn models_response(&self) -> Value {
        let mut entries = self.menu_entries();
        // 默认模型所在的那一条排最前。Science 会按自己的规则重排,这只是保持
        // 「首项即默认」的既有约定。
        let default_index = self.default_route_index();
        if let Some(position) = entries
            .iter()
            .position(|(_, index)| Some(*index) == default_index)
        {
            let default_entry = entries.remove(position);
            entries.insert(0, default_entry);
        }
        let data: Vec<Value> = entries
            .into_iter()
            .map(|(id, index)| {
                let route = &self.routes[index];
                // 旧配置可能把内部占位词 `default` 写进展示名。Science
                // 应展示用户实际配置的 upstream 模型，而不是伪模型名称。
                let raw_display = if route.display_name.trim().eq_ignore_ascii_case("default") {
                    &route.upstream_model
                } else {
                    &route.display_name
                };
                let display_name =
                    crate::models::science_safe_display_name(&route.upstream_model, raw_display);
                json!({
                    "type": "model",
                    "id": id,
                    "display_name": display_name,
                    "supports_tools": route.supports_tools,
                    "capabilities": {
                        "reasoning_round_trip": match route.capabilities.reasoning_round_trip {
                            ReasoningRoundTrip::None => "none",
                            ReasoningRoundTrip::Native => "native",
                            ReasoningRoundTrip::CsswitchOpaque => "csswitch_opaque",
                        },
                        "forced_tool_choice": route.capabilities.forced_tool_choice,
                        "structured_output": route.capabilities.structured_output,
                        "vision": route.capabilities.vision,
                    },
                    "created_at": "2026-01-01T00:00:00Z",
                })
            })
            .collect();
        json!({
            "data": data,
            "has_more": false,
            "first_id": data.first().and_then(|item| item.get("id")).cloned().unwrap_or(Value::Null),
            "last_id": data.last().and_then(|item| item.get("id")).cloned().unwrap_or(Value::Null),
        })
    }
}

fn fingerprint_text(digest: &mut Sha256, value: &str) {
    digest.update((value.len() as u32).to_be_bytes());
    digest.update(value.as_bytes());
}

/// 由目录 JSON 计算指纹。控制面在生成目录时用它填 `catalog_fp`,
/// 从而与解析侧的校验共用同一套算法(避免两处实现漂移)。
pub fn fingerprint_for_catalog(catalog: &Value) -> String {
    match serde_json::from_value::<WireCatalog>(catalog.clone()) {
        Ok(parsed) => wire_catalog_fingerprint(&parsed),
        Err(_) => String::new(),
    }
}

fn wire_catalog_fingerprint(catalog: &WireCatalog) -> String {
    let mut digest = Sha256::new();
    digest.update(b"csswitch-static-catalog-fp-v1\0");
    digest.update(catalog.schema_version.to_be_bytes());
    fingerprint_text(&mut digest, &catalog.adapter);
    fingerprint_text(&mut digest, &catalog.default_selector_id);
    digest.update((catalog.routes.len() as u32).to_be_bytes());
    for route in &catalog.routes {
        fingerprint_text(&mut digest, &route.selector_id);
        fingerprint_text(&mut digest, &route.display_name);
        fingerprint_text(&mut digest, &route.upstream_model);
        digest.update([match route.supports_tools {
            None => 0,
            Some(false) => 1,
            Some(true) => 2,
        }]);
        fingerprint_text(
            &mut digest,
            match route.capabilities.reasoning_round_trip {
                ReasoningRoundTrip::None => "none",
                ReasoningRoundTrip::Native => "native",
                ReasoningRoundTrip::CsswitchOpaque => "csswitch_opaque",
            },
        );
        for capability in [
            route.capabilities.forced_tool_choice,
            route.capabilities.structured_output,
            route.capabilities.vision,
        ] {
            digest.update([match capability {
                None => 0,
                Some(false) => 1,
                Some(true) => 2,
            }]);
        }
    }
    for role in [
        &catalog.role_bindings.sonnet,
        &catalog.role_bindings.opus,
        &catalog.role_bindings.haiku,
        &catalog.role_bindings.fable,
    ] {
        fingerprint_text(&mut digest, role);
    }
    digest.update((catalog.legacy_aliases.len() as u32).to_be_bytes());
    for legacy in &catalog.legacy_aliases {
        fingerprint_text(&mut digest, &legacy.alias);
        fingerprint_text(&mut digest, &legacy.selector_id);
    }
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{Channel, ModelSlot};

    /// Science 0.1.60 自己会用的官方 id:新会话的出厂默认(`claude-opus-5`)、旧会话里存着的
    /// 型号,以及它的辅助调用写死的三个(haiku-4-5 / sonnet-5 / sonnet-4-6)。
    /// 清单里不再列它们,但带着它们来的请求必须照常解析。
    const SCIENCE_CANONICAL_ROLE_MODELS: [&str; 5] = [
        "claude-opus-5",
        "claude-sonnet-5",
        "claude-opus-4-8",
        "claude-sonnet-4-6",
        "claude-haiku-4-5-20251001",
    ];

    /// 一份外部生成的目录(尚未填指纹)。
    fn fixture() -> Value {
        json!({
            "schema_version": 1,
            "adapter": "relay",
            "default_selector_id": "claude-csswitch-kimi-plus-111111111111",
            "routes": [
                {"selector_id":"claude-csswitch-kimi-max-222222222222","display_name":"Max","upstream_model":"k3","supports_tools":true},
                {"selector_id":"claude-csswitch-kimi-plus-111111111111","display_name":"Plus","upstream_model":"k3-256k","supports_tools":null},
                {"selector_id":"claude-csswitch-kimi-fast-333333333333","display_name":"Fast","upstream_model":"kimi-for-coding","supports_tools":true}
            ],
            "role_bindings": {
                "sonnet":"claude-csswitch-kimi-plus-111111111111",
                "opus":"claude-csswitch-kimi-max-222222222222",
                "haiku":"claude-csswitch-kimi-fast-333333333333",
                "fable":"claude-csswitch-kimi-max-222222222222"
            },
            "legacy_aliases": [
                {"alias":"k3-256k","selector_id":"claude-csswitch-kimi-plus-111111111111"}
            ]
        })
    }

    /// 给目录填上与内容相符的指纹。
    fn sealed(mut value: Value) -> Value {
        value["catalog_fp"] = Value::String("0".repeat(64));
        let wire: WireCatalog = serde_json::from_value(value.clone()).unwrap();
        value["catalog_fp"] = Value::String(wire_catalog_fingerprint(&wire));
        value
    }

    fn resolver() -> StaticProfileResolver {
        StaticProfileResolver::from_json(&sealed(fixture()).to_string()).unwrap()
    }

    /// 用户在控制台里配出来的渠道:四个槽,空串表示留空。显示名取 `<模型 ID> 的名字`。
    fn channel(default: &str, quality: &str, fast: &str, fable: &str) -> Channel {
        let slot = |model_id: &str| {
            if model_id.is_empty() {
                ModelSlot::default()
            } else {
                ModelSlot::new(model_id, &format!("{model_id} 的名字"))
            }
        };
        Channel {
            base_url: "https://api.example.invalid".into(),
            api_key: "k".into(),
            default_model: slot(default),
            quality_model: slot(quality),
            fast_model: slot(fast),
            fable_model: slot(fable),
        }
    }

    fn resolver_for(channel: &Channel) -> StaticProfileResolver {
        StaticProfileResolver::from_json(&channel.static_catalog("relay").to_string()).unwrap()
    }

    /// 清单里的 id,按响应里的顺序。
    fn listed_ids(resolver: &StaticProfileResolver) -> Vec<String> {
        resolver.models_response()["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["id"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn resolution_order_is_exact_legacy_role_then_unknown() {
        let resolver = resolver();
        assert_eq!(
            resolver
                .resolve("claude-csswitch-kimi-plus-111111111111")
                .unwrap()
                .kind,
            ResolutionKind::ExactSelector
        );
        assert_eq!(
            resolver.resolve("k3-256k").unwrap().kind,
            ResolutionKind::LegacyExact
        );
        let resolved = resolver
            .resolve("claude-csswitch-kimi-plus-111111111111")
            .unwrap();
        assert_eq!(resolved.reasoning_round_trip(), ReasoningRoundTrip::None);
        assert_eq!(resolved.supports_forced_tool_choice(), None);
        assert_eq!(resolved.supports_structured_output(), None);
        assert_eq!(resolved.supports_vision(), None);
        assert_eq!(
            resolver
                .resolve("claude-opus-4-8-20250514")
                .unwrap()
                .upstream_model(),
            "k3"
        );
        assert_eq!(
            resolver
                .resolve("claude-3-5-sonnet-20241022")
                .unwrap()
                .upstream_model(),
            "k3-256k"
        );
        for rejected in [
            "",
            "claude-opus",
            "claude-opus-4-8-latest",
            "claude-csswitch-kimi-k3",
            "claude-opus-4-8-forged",
        ] {
            assert!(resolver.resolve(rejected).is_none(), "{rejected}");
        }
    }

    #[test]
    fn menu_lists_each_model_once_in_the_main_positions() {
        // 四个槽(默认 / 高质量 / 快速 / Fable)→ 清单里的(id, 模型)(响应里的顺序:默认模型
        // 所在的那条最前,其余按去重的优先顺序 默认 → 高质量 → 快速,最后是进 More models 的)。
        // 前四行是常见的四种配置,后三行是去重的边角。
        type Case = ([&'static str; 4], &'static [(&'static str, &'static str)]);
        let cases: [Case; 7] = [
            // 两个模型、高质量槽为空(继承默认槽):默认模型只占自己的 sonnet 位,
            // opus 位不再列一遍。去重之前这份配置在菜单里是七个条目。
            (
                ["k3", "", "kimi-for-coding", ""],
                &[
                    ("claude-sonnet-19", "k3"),
                    ("claude-haiku-19", "kimi-for-coding"),
                ],
            ),
            // 出厂的 Kimi 渠道:三个不同的模型各占一个位置。
            (
                ["k3-256k", "k3", "kimi-for-coding", ""],
                &[
                    ("claude-sonnet-19", "k3-256k"),
                    ("claude-opus-19", "k3"),
                    ("claude-haiku-19", "kimi-for-coding"),
                ],
            ),
            // 四个都不同:Fable 槽的模型没有主菜单位置,用自己的 selector 列出。
            (
                ["a", "b", "c", "d"],
                &[
                    ("claude-sonnet-19", "a"),
                    ("claude-opus-19", "b"),
                    ("claude-haiku-19", "c"),
                    ("claude-csswitch-d", "d"),
                ],
            ),
            (["a", "a", "a", ""], &[("claude-sonnet-19", "a")]),
            // 高质量与快速是同一个模型:它只占 opus 位,haiku 位不再列一遍。
            (
                ["a", "b", "b", ""],
                &[("claude-sonnet-19", "a"), ("claude-opus-19", "b")],
            ),
            // Fable 槽与快速槽同模型:已经在主菜单里,不进 More models。
            (
                ["a", "", "c", "c"],
                &[("claude-sonnet-19", "a"), ("claude-haiku-19", "c")],
            ),
            // 只多配了 Fable 槽。
            (
                ["a", "", "", "d"],
                &[("claude-sonnet-19", "a"), ("claude-csswitch-d", "d")],
            ),
        ];
        for (slots, expected) in cases {
            let resolver = resolver_for(&channel(slots[0], slots[1], slots[2], slots[3]));
            let response = resolver.models_response();
            let listed: Vec<(&str, &str)> = response["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|model| {
                    (
                        model["id"].as_str().unwrap(),
                        model["display_name"].as_str().unwrap(),
                    )
                })
                .collect();
            let expected: Vec<(&str, String)> = expected
                .iter()
                .map(|(id, model)| (*id, format!("{model} 的名字")))
                .collect();
            assert_eq!(
                listed,
                expected
                    .iter()
                    .map(|(id, name)| (*id, name.as_str()))
                    .collect::<Vec<_>>(),
                "{slots:?}"
            );
            // 默认槽先占位,所以默认条目恒在 sonnet 位,与别的槽怎么填无关。它不能带 opus:
            // Science 对订阅档不够格的登录会把这样的默认模型换成一个清单里没有的 id。
            assert_eq!(resolver.default_menu_id(), "claude-sonnet-19", "{slots:?}");
            // Science 的 default_model 要指向的那一条,显示的就是默认槽的模型。
            assert_eq!(
                resolver
                    .resolve(resolver.default_menu_id())
                    .map(|route| route.upstream_model().to_string()),
                Some(slots[0].to_string()),
                "{slots:?}"
            );
            assert_eq!(response["first_id"], "claude-sonnet-19", "{slots:?}");
            for id in SCIENCE_CANONICAL_ROLE_MODELS {
                assert!(
                    !listed.iter().any(|(listed, _)| *listed == id),
                    "官方 id {id} 不得再出现在清单里:{slots:?}"
                );
            }
        }
    }

    #[test]
    fn every_listed_id_resolves_to_the_model_it_displays() {
        // 清单与解析是两段代码:菜单上写着模型 X 的条目,选中后请求必须真的发给 X。
        // 这里的预期只来自配置本身(每个模型的显示名各不相同),不来自清单的生成逻辑。
        for slots in [
            ["k3", "", "kimi-for-coding", ""],
            ["k3-256k", "k3", "kimi-for-coding", ""],
            ["a", "b", "c", "d"],
            ["a", "a", "a", ""],
            ["a", "b", "b", ""],
            ["a", "", "c", "c"],
            ["a", "", "", "d"],
        ] {
            let resolver = resolver_for(&channel(slots[0], slots[1], slots[2], slots[3]));
            let response = resolver.models_response();
            let models = response["data"].as_array().unwrap();
            let mut shown = Vec::new();
            for model in models {
                let id = model["id"].as_str().unwrap();
                let displayed = model["display_name"]
                    .as_str()
                    .unwrap()
                    .strip_suffix(" 的名字")
                    .unwrap_or_else(|| panic!("{id} 的显示名不是配置里的任何一个"));
                assert_eq!(
                    resolver
                        .resolve(id)
                        .map(|route| route.upstream_model().to_string()),
                    Some(displayed.to_string()),
                    "{slots:?}:条目 {id} 显示的是 {displayed}"
                );
                shown.push(displayed);
            }
            // 每个配了的模型恰好出现一次。
            let mut configured: Vec<&str> =
                slots.into_iter().filter(|slot| !slot.is_empty()).collect();
            configured.sort_unstable();
            configured.dedup();
            shown.sort_unstable();
            assert_eq!(shown, configured, "{slots:?}");
        }
    }

    #[test]
    fn official_and_legacy_ids_are_unlisted_but_still_resolve() {
        // 旧会话里存着官方 id 或旧的 claude-csswitch-* selector;Science 自己的辅助调用写死
        // 官方 id。它们不在清单里了,请求仍然要按原来的规则到达对应的模型。
        let resolver = resolver();
        let listed = listed_ids(&resolver);
        assert_eq!(
            listed,
            ["claude-sonnet-19", "claude-opus-19", "claude-haiku-19"],
            "三条 route 各占一个主菜单位置,默认的排最前"
        );
        for (id, upstream) in [
            ("claude-opus-5", "k3"),
            ("claude-sonnet-5", "k3-256k"),
            ("claude-opus-4-8", "k3"),
            ("claude-sonnet-4-6", "k3-256k"),
            ("claude-haiku-4-5-20251001", "kimi-for-coding"),
        ] {
            assert!(
                SCIENCE_CANONICAL_ROLE_MODELS.contains(&id),
                "夹具前提:{id} 是 Science 自己会用的 id"
            );
            assert!(!listed.iter().any(|listed| listed == id), "{id} 不再列出");
            let resolved = resolver
                .resolve(id)
                .unwrap_or_else(|| panic!("{id} 必须仍然可解析"));
            assert_eq!(resolved.upstream_model(), upstream, "{id}");
            assert_eq!(resolved.kind, ResolutionKind::OfficialRole, "{id}");
        }
        for (selector, upstream) in [
            ("claude-csswitch-kimi-max-222222222222", "k3"),
            ("claude-csswitch-kimi-plus-111111111111", "k3-256k"),
            ("claude-csswitch-kimi-fast-333333333333", "kimi-for-coding"),
        ] {
            assert!(
                !listed.iter().any(|listed| listed == selector),
                "{selector} 已经并进主菜单位置"
            );
            let resolved = resolver.resolve(selector).unwrap();
            assert_eq!(resolved.upstream_model(), upstream);
            assert_eq!(resolved.kind, ResolutionKind::ExactSelector);
        }
    }

    #[test]
    fn default_entry_may_be_a_route_outside_the_main_positions() {
        // 环境变量入口接受外部生成的目录:默认 route 可以不绑定任何主菜单角色。
        // 这时它以自己的 selector 列出,默认条目就是它,而不是悄悄换成第一个主菜单位置。
        let mut catalog = fixture();
        catalog["routes"].as_array_mut().unwrap().push(json!({
            "selector_id": "claude-csswitch-extra",
            "display_name": "Extra",
            "upstream_model": "extra",
            "supports_tools": true
        }));
        catalog["default_selector_id"] = json!("claude-csswitch-extra");
        let resolver = StaticProfileResolver::from_json(&sealed(catalog).to_string()).unwrap();
        assert_eq!(resolver.default_menu_id(), "claude-csswitch-extra");
        assert_eq!(
            listed_ids(&resolver),
            [
                "claude-csswitch-extra",
                "claude-sonnet-19",
                "claude-opus-19",
                "claude-haiku-19"
            ]
        );
    }

    #[test]
    fn menu_position_ids_are_a_family_name_plus_a_short_numeric_version() {
        // Science 的主菜单只认「家族名 + 1–2 位数字版本号」这种写法的 id,写成别的样子
        // (三位数、多一段、带点号)条目会掉进 More models;网关靠家族名把请求解析到槽位。
        // 换版本号时只改了其中一个位置,或者换成了 Science 不认的写法,这里要红。
        let mut versions = Vec::new();
        for (role, id) in MENU_POSITIONS {
            let version = id
                .strip_prefix(&format!("claude-{role}-"))
                .unwrap_or_else(|| panic!("{id} 不是 claude-{role}-<版本号>"));
            assert!(
                (1..=2).contains(&version.len())
                    && version.bytes().all(|byte| byte.is_ascii_digit()),
                "{id}"
            );
            assert_eq!(official_role_alias(id), Some(role), "{id}");
            versions.push(version);
        }
        versions.dedup();
        assert_eq!(versions.len(), 1, "三个位置用同一个版本号:{versions:?}");
    }

    #[test]
    fn a_model_shared_by_two_slots_is_listed_once_under_the_first_slots_name() {
        // 同一个模型填在两个显式槽里、两边的显示名不同:目录只给它一条 route(默认 → 高质量 →
        // 快速 → Fable 里第一个槽的那一套),清单与解析都指向这一条。这里把「第一个槽优先」
        // 钉成事实,免得哪天悄悄变成「最后一个槽优先」。
        let mut shared = channel("shared", "shared", "fast", "fable");
        shared.default_model.display_name = "Shared A".into();
        shared.quality_model.display_name = "Shared B".into();

        let resolver = resolver_for(&shared);
        assert_eq!(
            listed_ids(&resolver),
            [
                "claude-sonnet-19",
                "claude-haiku-19",
                "claude-csswitch-fable"
            ]
        );
        let response = resolver.models_response();
        assert_eq!(response["data"][0]["display_name"], "Shared A");
        // 占的是默认槽的位置;被它共用的高质量槽的 id 与旧的 selector 也到同一个模型。
        for id in [
            "claude-sonnet-19",
            "claude-opus-19",
            "claude-csswitch-shared",
        ] {
            assert_eq!(resolver.resolve(id).unwrap().upstream_model(), "shared");
        }
    }

    #[test]
    fn models_response_never_exposes_default_as_a_display_name() {
        let mut resolver = resolver();
        let default_index = *resolver
            .by_selector
            .get(&resolver.default_selector_id)
            .unwrap();
        resolver.routes[default_index].display_name = "default".into();
        let response = resolver.models_response();
        // 占位词换成上游模型;而 `k3-256k` 这种写法 Science 会当成内部 id、把整条丢掉,
        // 所以清单里出现的是它肯显示的写法。
        assert_eq!(response["data"][0]["display_name"], "K3 256k");
    }

    #[test]
    fn a_slot_without_a_display_name_still_shows_up_in_sciences_menu() {
        // 显示名留空的槽回落到模型 ID。`deepseek-v4-pro` 这种小写连字符写法的名字,
        // Science 会把整条从菜单里丢掉(0.1.60 真机确认):默认模型若是这样,新会话就没有默认模型。
        let mut bare = channel("deepseek-v4-pro", "", "k3", "");
        bare.default_model.display_name.clear();
        bare.fast_model.display_name.clear();
        let response = resolver_for(&bare).models_response();
        let names: Vec<&str> = response["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["display_name"].as_str().unwrap())
            .collect();
        // `k3` 没有连字符,Science 照常显示,原样不动。
        assert_eq!(names, ["DeepSeek V4 Pro", "k3"]);
    }

    #[test]
    fn machine_shaped_claude_display_name_is_humanized_without_changing_id() {
        let mut resolver = resolver();
        let default_index = *resolver
            .by_selector
            .get(&resolver.default_selector_id)
            .unwrap();
        resolver.routes[default_index].upstream_model = "claude-sonnet-5".into();
        resolver.routes[default_index].display_name = "claude-sonnet-5".into();
        let response = resolver.models_response();
        assert_eq!(response["data"][0]["id"], "claude-sonnet-19");
        assert_eq!(response["data"][0]["display_name"], "Claude Sonnet 5");
    }

    #[test]
    fn rejects_tampered_catalog_even_when_claimed_fingerprint_is_well_formed() {
        let mut value = serde_json::to_value(json!({
            "schema_version": 1,
            "adapter": "relay",
            "default_selector_id": "claude-csswitch-kimi-plus-111111111111",
            "routes": [{"selector_id":"claude-csswitch-kimi-plus-111111111111","display_name":"Plus","upstream_model":"k3-256k","supports_tools":true}],
            "role_bindings": {
                "sonnet":"claude-csswitch-kimi-plus-111111111111",
                "opus":"claude-csswitch-kimi-plus-111111111111",
                "haiku":"claude-csswitch-kimi-plus-111111111111",
                "fable":"claude-csswitch-kimi-plus-111111111111"
            },
            "legacy_aliases": []
        })).unwrap();
        value["catalog_fp"] = Value::String("0".repeat(64));
        let wire: WireCatalog = serde_json::from_value(value.clone()).unwrap();
        value["catalog_fp"] = Value::String(wire_catalog_fingerprint(&wire));
        value["routes"][0]["upstream_model"] = json!("tampered");
        assert!(StaticProfileResolver::from_json(&value.to_string())
            .unwrap_err()
            .contains("fingerprint"));
    }
}
