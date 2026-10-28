//! 用户配置存储:`~/.csswitch/service.v1.json`(0600)。
//!
//! **文件名必须与旧桌面端的 `config.json` 区分开**:旧应用会把不认识的
//! schema 当成自己的旧版本配置去迁移,结果是它把自己的 provider 列表清空。
//! 实测踩过一次,不要合并这两个文件。
//!
//! 保存两个实例各自的模式(正式实例 `mode`、免登录沙箱 `sandbox_mode`)与各渠道的
//! 连接配置;渠道配置每个渠道一份,两个实例共用。模型配置沿用桌面端的四槽语义:
//! 默认槽必填,高质量/快速/Fable 留空自动继承默认槽;每槽可分别设置
//! 上游模型 ID 与显示名(显示名进 Science 的模型菜单)。

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// 一个实例的模式。官方模式直通 api.anthropic.com(只有正式实例可用),
/// 渠道模式走对应契约的补偿链。
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

/// 推理请求来自哪个 Science 实例。两个实例各有自己的模式(见 [`Profile::mode_for`]),
/// 网关按请求路径认出实例:沙箱的 base_url 多带一段前缀
/// (`sandbox::INFERENCE_PREFIX`)。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Instance {
    /// 用户自己登录的官方实例。
    Official,
    /// 免登录沙箱。
    Sandbox,
}

impl Instance {
    pub fn as_str(self) -> &'static str {
        match self {
            Instance::Official => "official",
            Instance::Sandbox => "sandbox",
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
            // 这个 id 自 2026-09-11 起指向 K2.8 Preview(此前是 K2.7)。
            fast_model: ModelSlot::new("kimi-for-coding", "Kimi K2.8"),
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
        // 显示名取第一个出现的槽(默认 → 高质量 → 快速 → Fable)。
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
    /// 正式实例(用户自己登录的 Science)的模式。
    #[serde(default)]
    pub mode: Mode,
    /// 免登录沙箱的渠道,与 `mode` 互不影响。只会是 Kimi / DeepSeek:沙箱用的是
    /// CSSwitch 铸造的虚拟登录,没有真实 Claude 凭证,官方接口必然拒绝它。
    /// 存量文件没有这个键、或被手改成 `official` 时的取值见 [`Profile::parse`]。
    #[serde(default = "default_sandbox_mode")]
    pub sandbox_mode: Mode,
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
    /// 跳过审批总开关(全局,两个实例共用)。开着时经 CSSwitch 启动的 Science 都带
    /// `--dangerously-skip-approvals`:权限卡一律自动批准,直到不带它重启。
    ///
    /// 默认关;关着时不写进文件,存量 `service.v1.json` 读进来再存回去逐字不变。
    #[serde(default, skip_serializing_if = "is_false")]
    pub skip_approvals: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn default_sandbox_mode() -> Mode {
    Mode::Kimi
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
            sandbox_mode: default_sandbox_mode(),
            port: default_port(),
            kimi: Channel::kimi_default(),
            deepseek: Channel::deepseek_default(),
            request_log: default_request_log(),
            web_search: default_web_search(),
            skip_approvals: false,
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

    /// 这个实例的推理走哪个模式。
    pub fn mode_for(&self, instance: Instance) -> &Mode {
        match instance {
            Instance::Official => &self.mode,
            Instance::Sandbox => &self.sandbox_mode,
        }
    }

    pub fn load() -> Self {
        let Ok(text) = std::fs::read_to_string(config_path()) else {
            return Self::default();
        };
        // 配置损坏时不猜测:回落到默认值,让用户在 WebUI 里重填。
        Self::parse(&text).unwrap_or_default()
    }

    /// 解析 `service.v1.json` 的文本,迁移旧的渠道级联网搜索开关,并补出沙箱渠道。
    ///
    /// **联网搜索**:旧版本把开关存在每个渠道里(`kimi.web_search`),界面只在 Kimi 页签
    /// 暴露过它。迁移只有一条:顶层没有 `web_search` 且 `kimi.web_search == false` → 总开关为
    /// `false`;其余一律按默认值读成开启。用户显式关掉的东西不能在升级后悄悄打开。
    /// 顶层字段一旦存在就以它为准,不再回看旧键;旧键是未知字段,读入时被丢弃,
    /// 下次保存不再写出。
    ///
    /// **沙箱渠道**:`sandbox_mode` 存在且是渠道 → 以它为准。键不存在(升级前的文件),
    /// 或值是 `official`(只可能来自手改)→ 按 [`Profile::inferred_sandbox_mode`] 推导。
    /// 推导结果不立刻回写;之后任何一次存盘都会把它写出去。
    pub fn parse(text: &str) -> Result<Self, serde_json::Error> {
        let raw: Value = serde_json::from_str(text)?;
        let disabled_in_legacy_kimi = raw.get("web_search").is_none()
            && raw.pointer("/kimi/web_search") == Some(&Value::Bool(false));
        let sandbox_mode_missing = raw.get("sandbox_mode").is_none();
        let mut profile: Self = serde_json::from_value(raw)?;
        if disabled_in_legacy_kimi {
            profile.web_search = false;
        }
        if sandbox_mode_missing || profile.sandbox_mode == Mode::Official {
            profile.sandbox_mode = profile.inferred_sandbox_mode();
        }
        Ok(profile)
    }

    /// 配置里没有可用的沙箱渠道时取哪个。
    ///
    /// 升级前沙箱跟着正式实例的模式走:它是渠道就沿用,升级前后效果一致。正式实例是官方时
    /// 沙箱原本起不来,没有旧行为可保:取第一个配置完整的渠道(Kimi → DeepSeek),
    /// 都不完整取 Kimi。固定取 Kimi 会让只配了 DeepSeek 的用户升级后指向一个没配的渠道。
    fn inferred_sandbox_mode(&self) -> Mode {
        if self.mode != Mode::Official {
            return self.mode.clone();
        }
        [Mode::Kimi, Mode::Deepseek]
            .into_iter()
            .find(|mode| {
                self.channel(mode)
                    .is_some_and(|channel| channel.validate().is_ok())
            })
            .unwrap_or(Mode::Kimi)
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

    /// 0.10.0 对出厂默认配置写出的 `service.v1.json`,逐字节。
    const CONFIG_WRITTEN_BY_0_10_0: &str = r#"{
  "mode": "official",
  "sandbox_mode": "kimi",
  "port": 8788,
  "kimi": {
    "base_url": "https://api.kimi.com/coding",
    "api_key": "",
    "default_model": {
      "model_id": "k3-256k",
      "display_name": "Kimi K3 256k"
    },
    "quality_model": {
      "model_id": "k3",
      "display_name": "Kimi K3"
    },
    "fast_model": {
      "model_id": "kimi-for-coding",
      "display_name": "Kimi K2.7"
    },
    "fable_model": {
      "model_id": "",
      "display_name": ""
    }
  },
  "deepseek": {
    "base_url": "https://api.deepseek.com/anthropic",
    "api_key": "",
    "default_model": {
      "model_id": "deepseek-v4-pro",
      "display_name": "DeepSeek V4 Pro"
    },
    "quality_model": {
      "model_id": "",
      "display_name": ""
    },
    "fast_model": {
      "model_id": "deepseek-v4-flash",
      "display_name": "DeepSeek V4 Flash"
    },
    "fable_model": {
      "model_id": "",
      "display_name": ""
    }
  },
  "request_log": true,
  "web_search": true
}"#;

    #[test]
    fn a_config_written_by_0_10_0_round_trips_byte_for_byte() {
        // 存量文件读进来再存回去,不能多出键、少掉键或换了顺序 —— 否则每个老用户的配置在第一次
        // 保存时都会被改写,回滚到旧版本后还会看到不认识的键。存盘用的就是 `to_string_pretty`。
        let profile = Profile::parse(CONFIG_WRITTEN_BY_0_10_0).unwrap();
        assert_eq!(
            serde_json::to_string_pretty(&profile).unwrap(),
            CONFIG_WRITTEN_BY_0_10_0
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

    /// 升级前的 `service.v1.json`:没有 `sandbox_mode`。两个渠道的 key 为空即「没配完整」;
    /// `top_extra` 按需补上顶层键。
    fn config_without_sandbox_mode(
        mode: &str,
        kimi_key: &str,
        deepseek_key: &str,
        top_extra: &str,
    ) -> String {
        format!(
            r#"{{
            "mode": "{mode}",
            "kimi": {{
                "base_url": "https://api.kimi.com/coding",
                "api_key": "{kimi_key}",
                "default_model": {{"model_id": "k3", "display_name": ""}}
            }},
            "deepseek": {{
                "base_url": "https://api.deepseek.com/anthropic",
                "api_key": "{deepseek_key}",
                "default_model": {{"model_id": "deepseek-v4-pro", "display_name": ""}}
            }}{top_extra}
        }}"#
        )
    }

    #[test]
    fn configs_from_before_the_sandbox_channel_keep_the_sandbox_where_it_was() {
        let sandbox_mode = |mode: &str, kimi_key: &str, deepseek_key: &str| {
            Profile::parse(&config_without_sandbox_mode(
                mode,
                kimi_key,
                deepseek_key,
                "",
            ))
            .unwrap()
            .sandbox_mode
        };
        // 升级前沙箱跟着正式实例走:它是渠道就沿用,升级前后是同一条链路。
        // 两个渠道都配完整,所以这两条分不出来就说明取的不是正式实例的模式。
        assert_eq!(sandbox_mode("kimi", "k", "k"), Mode::Kimi);
        assert_eq!(sandbox_mode("deepseek", "k", "k"), Mode::Deepseek);
        // 正式实例是官方(升级前沙箱起不来):取第一个配置完整的渠道。
        // 只配了 DeepSeek 的用户不能在升级后被指到没配的 Kimi。
        assert_eq!(sandbox_mode("official", "", "k"), Mode::Deepseek);
        assert_eq!(sandbox_mode("official", "k", "k"), Mode::Kimi);
        // 都不完整:Kimi。
        assert_eq!(sandbox_mode("official", "", ""), Mode::Kimi);
    }

    #[test]
    fn an_explicit_sandbox_channel_wins_and_is_never_official() {
        // 配置里写了就以它为准:不回看正式实例的模式,也不看哪个渠道配置完整。
        let explicit = Profile::parse(&config_without_sandbox_mode(
            "kimi",
            "k",
            "",
            r#", "sandbox_mode": "deepseek""#,
        ))
        .unwrap();
        assert_eq!(explicit.sandbox_mode, Mode::Deepseek);
        assert_eq!(explicit.mode, Mode::Kimi, "正式实例的模式不随沙箱渠道变");

        // 手改成 official:沙箱没有真实登录,不能停在官方上 —— 按没有这个键来推导。
        let hand_edited = Profile::parse(&config_without_sandbox_mode(
            "official",
            "",
            "k",
            r#", "sandbox_mode": "official""#,
        ))
        .unwrap();
        assert_eq!(hand_edited.sandbox_mode, Mode::Deepseek);
        assert_eq!(hand_edited.mode, Mode::Official);

        // 存盘读回:两个实例各存各的。少写出这个键的话,读回时会被推导成正式实例的 Kimi。
        let profile = Profile {
            mode: Mode::Kimi,
            sandbox_mode: Mode::Deepseek,
            ..Profile::default()
        };
        let saved = serde_json::to_value(&profile).unwrap();
        assert_eq!(saved["sandbox_mode"], json!("deepseek"));
        let reloaded = Profile::parse(&saved.to_string()).unwrap();
        assert_eq!(
            (reloaded.mode, reloaded.sandbox_mode),
            (Mode::Kimi, Mode::Deepseek)
        );
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
