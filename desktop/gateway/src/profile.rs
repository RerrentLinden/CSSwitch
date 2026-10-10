//! 用户配置存储:`~/.csswitch/service.v1.json`(0600)。
//!
//! **文件名必须与旧桌面端的 `config.json` 区分开**:旧应用会把不认识的
//! schema 当成自己的旧版本配置去迁移,结果是它把自己的 provider 列表清空。
//! 实测踩过一次,不要合并这两个文件。
//!
//! 保存激活模式与各渠道的连接配置。模型配置沿用桌面端的四槽语义:
//! 默认槽必填,高质量/快速/Fable 留空自动继承默认槽;每槽可分别设置
//! 上游模型 ID 与显示名(显示名进 Science 的模型菜单)。

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// 激活模式。官方模式直通 api.anthropic.com,渠道模式走对应契约的补偿链。
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    #[default]
    Official,
    Kimi,
    Deepseek,
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Official => "official",
            Mode::Kimi => "kimi",
            Mode::Deepseek => "deepseek",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "official" => Some(Mode::Official),
            "kimi" => Some(Mode::Kimi),
            "deepseek" => Some(Mode::Deepseek),
            _ => None,
        }
    }

    /// 渠道模式对应的 gateway adapter 与 provider contract。
    pub fn contract(&self) -> Option<(&'static str, &'static str)> {
        match self {
            Mode::Official => None,
            Mode::Kimi => Some(("relay", "kimi-anthropic-relay")),
            Mode::Deepseek => Some(("deepseek", "deepseek-native")),
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Mode::Official => "官方 Claude",
            Mode::Kimi => "Kimi",
            Mode::Deepseek => "DeepSeek",
        }
    }
}

/// 一个模型槽:上游模型 ID + 可选显示名。显示名留空时回落到模型 ID。
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelSlot {
    #[serde(default)]
    pub model_id: String,
    #[serde(default)]
    pub display_name: String,
}

impl ModelSlot {
    pub fn new(model_id: &str, display_name: &str) -> Self {
        Self {
            model_id: model_id.to_string(),
            display_name: display_name.to_string(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.model_id.trim().is_empty()
    }

    fn label(&self) -> String {
        let name = self.display_name.trim();
        if name.is_empty() {
            self.model_id.trim().to_string()
        } else {
            name.to_string()
        }
    }
}

/// 一个渠道的完整连接配置。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Channel {
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    /// 默认槽(均衡),必填。
    pub default_model: ModelSlot,
    /// 高质量 / 快速 / Fable 槽,留空继承默认槽。
    #[serde(default)]
    pub quality_model: ModelSlot,
    #[serde(default)]
    pub fast_model: ModelSlot,
    #[serde(default)]
    pub fable_model: ModelSlot,
}

impl Channel {
    /// 测试用:等价于 Kimi 默认配置。
    #[cfg(test)]
    pub fn from_defaults_for_test() -> Self {
        Self::kimi_default()
    }

    fn kimi_default() -> Self {
        Self {
            base_url: "https://api.kimi.com/coding".into(),
            api_key: String::new(),
            default_model: ModelSlot::new("k3-256k", "Kimi K3 256k"),
            quality_model: ModelSlot::new("k3", "Kimi K3"),
            fast_model: ModelSlot::new("kimi-for-coding", "Kimi K2.7"),
            fable_model: ModelSlot::default(),
        }
    }

    fn deepseek_default() -> Self {
        Self {
            base_url: "https://api.deepseek.com/anthropic".into(),
            api_key: String::new(),
            default_model: ModelSlot::new("deepseek-v4-pro", "DeepSeek V4 Pro"),
            quality_model: ModelSlot::default(),
            fast_model: ModelSlot::new("deepseek-v4-flash", "DeepSeek V4 Flash"),
            fable_model: ModelSlot::default(),
        }
    }

    /// 四个角色槽的最终取值,空槽继承默认槽。
    fn resolved_slots(&self) -> [(&'static str, ModelSlot); 4] {
        let inherit = |slot: &ModelSlot| {
            if slot.is_empty() {
                self.default_model.clone()
            } else {
                slot.clone()
            }
        };
        [
            ("sonnet", self.default_model.clone()),
            ("opus", inherit(&self.quality_model)),
            ("haiku", inherit(&self.fast_model)),
            ("fable", inherit(&self.fable_model)),
        ]
    }

    pub fn validate(&self) -> Result<(), String> {
        if !(self.base_url.starts_with("http://") || self.base_url.starts_with("https://")) {
            return Err("地址必须是 http(s):// 开头的 URL".into());
        }
        if self.default_model.is_empty() {
            return Err("默认模型必填".into());
        }
        if self.api_key.trim().is_empty() {
            return Err("缺少 API Key".into());
        }
        Ok(())
    }

    /// 生成网关静态模型目录(`CSSWITCH_STATIC_MODEL_CATALOG_V1` 的内容)。
    /// selector_id 由槽位与模型 ID 派生,保证 Science 侧稳定可辨。
    pub fn static_catalog(&self, adapter: &str) -> Value {
        let slots = self.resolved_slots();
        // 同一模型 ID 在多个槽复用时只产出一条 route,避免菜单里出现重复项。
        let mut routes: Vec<Value> = Vec::new();
        let mut selector_by_model: BTreeMap<String, String> = BTreeMap::new();
        for (_role, slot) in slots.iter() {
            let model_id = slot.model_id.trim().to_string();
            if selector_by_model.contains_key(&model_id) {
                continue;
            }
            let selector = selector_id(&model_id);
            selector_by_model.insert(model_id.clone(), selector.clone());
            routes.push(json!({
                "selector_id": selector,
                "display_name": slot.label(),
                "upstream_model": model_id,
                "supports_tools": true,
                "capabilities": {
                    "reasoning_round_trip": "none",
                    "forced_tool_choice": true,
                    "structured_output": true,
                    "vision": null
                }
            }));
        }
        let role_binding = |role: &str| -> String {
            let slot = slots
                .iter()
                .find(|(name, _)| *name == role)
                .map(|(_, slot)| slot)
                .unwrap_or(&self.default_model);
            selector_by_model
                .get(slot.model_id.trim())
                .cloned()
                .unwrap_or_default()
        };
        let mut catalog = json!({
            "schema_version": 1,
            "adapter": adapter,
            "catalog_fp": "0".repeat(64),
            "default_selector_id": selector_id(self.default_model.model_id.trim()),
            "routes": routes,
            "role_bindings": {
                "sonnet": role_binding("sonnet"),
                "opus": role_binding("opus"),
                "haiku": role_binding("haiku"),
                "fable": role_binding("fable")
            },
            "legacy_aliases": []
        });
        let fingerprint = crate::static_profile::fingerprint_for_catalog(&catalog);
        catalog["catalog_fp"] = json!(fingerprint);
        catalog
    }
}

/// 模型 ID → selector。Science 只接受 `claude-` 前缀且字符受限的选择器,
/// 因此把模型 ID 规范化后加上稳定前缀。
fn selector_id(model_id: &str) -> String {
    let mut slug: String = model_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    slug.truncate(96);
    format!("claude-csswitch-{}", slug.trim_matches('-'))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default)]
    pub mode: Mode,
    #[serde(default = "default_port")]
    pub port: u16,
    pub kimi: Channel,
    pub deepseek: Channel,
    /// 是否记录每条请求的日志(终端里的请求明细行与控制台「请求日志」面板)。
    /// 启动信息、警告与错误不受它影响。
    ///
    /// **默认必须是 `true`**:`bool` 的 serde 原生默认是 `false`,而存量
    /// `service.v1.json` 没有这个键,直接用它等于升级后静默关掉日志。
    #[serde(default = "default_request_log")]
    pub request_log: bool,
    /// 联网搜索总开关(全局,不分渠道)。`false` 时经 CSSwitch 启动的 Science 以
    /// `enable_web_search = false` 运行,网关同时断开 web_search 桥。
    ///
    /// **默认必须是 `true`**:`bool` 的 serde 原生默认是 `false`,而存量
    /// `service.v1.json` 没有这个键,直接用它等于升级后静默关掉联网搜索。
    /// 存量文件里渠道级旧键的迁移见 [`Profile::parse`]。
    #[serde(default = "default_web_search")]
    pub web_search: bool,
}

fn default_port() -> u16 {
    8788
}

fn default_request_log() -> bool {
    true
}

fn default_web_search() -> bool {
    true
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            mode: Mode::Official,
            port: default_port(),
            kimi: Channel::kimi_default(),
            deepseek: Channel::deepseek_default(),
            request_log: default_request_log(),
            web_search: default_web_search(),
        }
    }
}

impl Profile {
    pub fn channel(&self, mode: &Mode) -> Option<&Channel> {
        match mode {
            Mode::Official => None,
            Mode::Kimi => Some(&self.kimi),
            Mode::Deepseek => Some(&self.deepseek),
        }
    }

    pub fn channel_mut(&mut self, mode: &Mode) -> Option<&mut Channel> {
        match mode {
            Mode::Official => None,
            Mode::Kimi => Some(&mut self.kimi),
            Mode::Deepseek => Some(&mut self.deepseek),
        }
    }

    pub fn load() -> Self {
        let Ok(text) = std::fs::read_to_string(config_path()) else {
            return Self::default();
        };
        // 配置损坏时不猜测:回落到默认值,让用户在 WebUI 里重填。
        Self::parse(&text).unwrap_or_default()
    }

    /// 解析 `service.v1.json` 的文本,并迁移旧的渠道级联网搜索开关。
    ///
    /// 旧版本把开关存在每个渠道里(`kimi.web_search`),界面只在 Kimi 页签暴露过它。
    /// 迁移只有一条:顶层没有 `web_search` 且 `kimi.web_search == false` → 总开关为
    /// `false`;其余一律按默认值读成开启。用户显式关掉的东西不能在升级后悄悄打开。
    /// 顶层字段一旦存在就以它为准,不再回看旧键;旧键是未知字段,读入时被丢弃,
    /// 下次保存不再写出。
    pub fn parse(text: &str) -> Result<Self, serde_json::Error> {
        let raw: Value = serde_json::from_str(text)?;
        let disabled_in_legacy_kimi = raw.get("web_search").is_none()
            && raw.pointer("/kimi/web_search") == Some(&Value::Bool(false));
        let mut profile: Self = serde_json::from_value(raw)?;
        if disabled_in_legacy_kimi {
            profile.web_search = false;
        }
        Ok(profile)
    }

    pub fn save(&self) -> Result<(), String> {
        let dir = config_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建配置目录失败:{e}"))?;
        let text = serde_json::to_string_pretty(self).map_err(|e| format!("序列化配置失败:{e}"))?;
        let path = config_path();
        std::fs::write(&path, text).map_err(|e| format!("写入配置失败:{e}"))?;
        restrict_permissions(&path)
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("设置配置文件权限失败:{e}"))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &std::path::Path) -> Result<(), String> {
    Ok(())
}

pub fn config_dir() -> PathBuf {
    home().join(".csswitch")
}

pub fn config_path() -> PathBuf {
    config_dir().join("service.v1.json")
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_slots_inherit_the_default_model() {
        let channel = Channel {
            base_url: "https://api.example.com".into(),
            api_key: "k".into(),
            default_model: ModelSlot::new("m-default", "Default"),
            quality_model: ModelSlot::default(),
            fast_model: ModelSlot::new("m-fast", "Fast"),
            fable_model: ModelSlot::default(),
        };
        let catalog = channel.static_catalog("relay");
        let bindings = &catalog["role_bindings"];
        assert_eq!(bindings["sonnet"], bindings["opus"], "空的高质量槽继承默认");
        assert_eq!(
            bindings["sonnet"], bindings["fable"],
            "空的 Fable 槽继承默认"
        );
        assert_ne!(bindings["sonnet"], bindings["haiku"], "填了的快速槽独立");
        // 两个不同模型 → 两条 route,不因四个槽而重复。
        assert_eq!(catalog["routes"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn catalog_is_accepted_by_the_gateway_resolver() {
        // 真实回归:目录必须能通过网关的指纹与选择器校验,否则渠道模式起不来。
        let channel = Channel::kimi_default();
        let catalog = channel.static_catalog("relay");
        let resolver =
            crate::static_profile::StaticProfileResolver::from_json(&catalog.to_string()).unwrap();
        assert_eq!(resolver.adapter(), "relay");
        assert!(
            resolver.resolve("claude-opus-5").is_some(),
            "官方角色名可解析"
        );
    }

    #[test]
    fn display_name_falls_back_to_the_model_id() {
        let slot = ModelSlot::new("k3", "");
        assert_eq!(slot.label(), "k3");
    }

    #[test]
    fn validation_rejects_incomplete_channels() {
        let mut channel = Channel::kimi_default();
        assert!(channel.validate().is_err(), "缺 key 应当失败");
        channel.api_key = "k".into();
        assert!(channel.validate().is_ok());
        channel.base_url = "ftp://x".into();
        assert!(channel.validate().is_err());
        channel.base_url = "https://x".into();
        channel.default_model = ModelSlot::default();
        assert!(channel.validate().is_err(), "默认槽必填");
    }

    /// 存量 `service.v1.json` 的形状:没有顶层 `web_search`,`kimi` 里按需带旧键。
    fn legacy_config(kimi_extra: &str, top_extra: &str) -> String {
        format!(
            r#"{{
            "mode": "kimi",
            "port": 8788,
            "kimi": {{
                "base_url": "https://api.kimi.com/coding",
                "api_key": "k",
                "default_model": {{"model_id": "k3", "display_name": "Kimi K3"}}{kimi_extra}
            }},
            "deepseek": {{
                "base_url": "https://api.deepseek.com/anthropic",
                "api_key": "k",
                "default_model": {{"model_id": "deepseek-v4-pro", "display_name": ""}}
            }}{top_extra}
        }}"#
        )
    }

    #[test]
    fn web_search_defaults_on_for_new_and_untouched_legacy_configs() {
        // bool 的 serde 原生默认是 false:少写一个自定义默认值,所有没碰过开关的
        // 老用户升级后就被静默关掉联网搜索。
        assert!(Profile::default().web_search, "新配置默认开启");
        let untouched = Profile::parse(&legacy_config("", "")).unwrap();
        assert!(untouched.web_search, "没有任何开关字段的老配置读成开启");
        let kept_on = Profile::parse(&legacy_config(r#", "web_search": true"#, "")).unwrap();
        assert!(kept_on.web_search, "旧开关为开的老配置读成开启");
    }

    #[test]
    fn legacy_kimi_switch_off_migrates_to_the_global_switch() {
        // 用户在旧界面的 Kimi 页签关过搜索:升级后不能悄悄打开。
        let migrated = Profile::parse(&legacy_config(r#", "web_search": false"#, "")).unwrap();
        assert!(!migrated.web_search, "Kimi 关过 → 总开关为关");

        // 保存后只剩顶层新字段;渠道级旧键不再写出,读回仍是关。
        let saved = serde_json::to_value(&migrated).unwrap();
        assert_eq!(saved["web_search"], json!(false));
        for channel in ["kimi", "deepseek"] {
            assert!(
                saved[channel].get("web_search").is_none(),
                "{channel} 不得再写出渠道级旧键:{saved}"
            );
        }
        let reloaded = Profile::parse(&saved.to_string()).unwrap();
        assert!(!reloaded.web_search, "关闭态必须能存盘并读回");
    }

    #[test]
    fn top_level_web_search_wins_over_the_legacy_channel_key() {
        // 顶层字段一旦存在就是唯一真相:用户在新界面重新打开之后,文件里就算还留着
        // 旧键(例如手工编辑、从备份恢复),也不能把开关再迁回关闭。
        let reopened = Profile::parse(&legacy_config(
            r#", "web_search": false"#,
            r#", "web_search": true"#,
        ))
        .unwrap();
        assert!(reopened.web_search, "顶层为开时不回看旧键");
        let closed = Profile::parse(&legacy_config(
            r#", "web_search": true"#,
            r#", "web_search": false"#,
        ))
        .unwrap();
        assert!(!closed.web_search, "顶层为关时同样以顶层为准");
    }

    #[test]
    fn request_log_defaults_on_and_survives_a_round_trip() {
        let legacy = r#"{
            "mode": "kimi",
            "port": 8788,
            "kimi": {
                "base_url": "https://api.kimi.com/coding",
                "default_model": {"model_id": "k3", "display_name": "Kimi K3"}
            },
            "deepseek": {
                "base_url": "https://api.deepseek.com/anthropic",
                "default_model": {"model_id": "deepseek-v4-pro", "display_name": ""}
            }
        }"#;
        let profile: Profile = serde_json::from_str(legacy).unwrap();
        assert!(profile.request_log, "缺键的老配置必须读成开启");

        let mut profile = Profile::default();
        assert!(profile.request_log, "新配置默认开启");
        profile.request_log = false;
        let text = serde_json::to_string(&profile).unwrap();
        let restored: Profile = serde_json::from_str(&text).unwrap();
        assert!(!restored.request_log, "关闭态必须能存盘并读回");
    }

    #[test]
    fn selector_ids_are_gateway_safe() {
        assert_eq!(selector_id("k3-256k"), "claude-csswitch-k3-256k");
        assert_eq!(
            selector_id("deepseek-ai/DeepSeek-V4"),
            "claude-csswitch-deepseek-ai-DeepSeek-V4"
        );
    }
}
