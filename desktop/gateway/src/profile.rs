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
}

fn default_port() -> u16 {
    8788
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            mode: Mode::Official,
            port: default_port(),
            kimi: Channel::kimi_default(),
            deepseek: Channel::deepseek_default(),
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
        serde_json::from_str(&text).unwrap_or_default()
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

    #[test]
    fn selector_ids_are_gateway_safe() {
        assert_eq!(selector_id("k3-256k"), "claude-csswitch-k3-256k");
        assert_eq!(
            selector_id("deepseek-ai/DeepSeek-V4"),
            "claude-csswitch-deepseek-ai-DeepSeek-V4"
        );
    }
}
