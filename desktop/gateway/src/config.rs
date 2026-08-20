#[derive(Clone, Debug)]
pub struct GatewayConfig {
    pub provider: String,
    pub port: u16,
    pub auth_secret: Option<String>,
    pub api_key: Option<String>,
    pub upstream_url: String,
    pub models_url: Option<String>,
    pub relay_thinking: Option<String>,
    /// Validated contract selected by the Tauri control plane. Standalone
    /// invocations may use the unique adapter fallback, but managed launches
    /// always bind an exact id plus the embedded catalog digest.
    pub(crate) provider_contract: Option<crate::provider_contracts::ProviderRuntimeContract>,
    pub intent: GatewayIntent,
    /// Non-Codex profiles receive a validated, non-sensitive selector snapshot.
    pub static_model_resolver: Option<crate::static_profile::StaticProfileResolver>,
    pub shim_mode: String,
    /// Opaque per-spawn identity supplied by the process manager.
    /// Standalone invocations may leave it empty, but managed launches always set it.
    pub launch_id: String,
    /// 激活渠道是否提供联网搜索。`false` 时 typed `web_search` 在 relay 请求
    /// 入口就被摘除,整条补偿链一致地按「本轮没有搜索声明」处理。
    /// 只有单进程服务会把它置为 `false`(用户在控制台里关);standalone 网关
    /// 没有用户配置文件,恒为 `true`,不接受环境变量 override。
    pub web_search: bool,
}

pub const GATEWAY_INTENT_ENV: &str = "CSSWITCH_GATEWAY_INTENT";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GatewayIntent {
    Formal,
    ScratchModels,
    ScratchMessage,
}

impl GatewayIntent {
    fn from_env() -> Result<Self, String> {
        match std::env::var(GATEWAY_INTENT_ENV).ok().as_deref() {
            None | Some("") | Some("formal") => Ok(Self::Formal),
            Some("scratch-models") => Ok(Self::ScratchModels),
            Some("scratch-message") => Ok(Self::ScratchMessage),
            Some(_) => Err(format!("{GATEWAY_INTENT_ENV} 非法")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Formal => "formal",
            Self::ScratchModels => "scratch-models",
            Self::ScratchMessage => "scratch-message",
        }
    }
}

pub const UPSTREAM_UA: &str = "CSSwitch/0.2 (+https://github.com/SuperJJ007/CSSwitch)";
pub const DEFAULT_UPSTREAM_URL: &str = "https://api.deepseek.com/anthropic/v1/messages";

pub const DEEPSEEK_MODELS: &[(&str, &str)] = &[
    ("claude-opus-4-8", "DeepSeek V4 Pro"),
    ("claude-haiku-4-5", "DeepSeek V4 Flash"),
];

/// DSML shim 已随 DeepSeek 的 OpenAI 格式路径一起退役;保留函数是为了让
/// 残留的环境变量显式归零,而不是被当成有效配置。
pub fn canonical_shim_mode(_provider: &str, _raw: Option<&str>) -> &'static str {
    "off"
}

/// thinking 策略解析:契约声明为准,环境变量作为显式 override(会记一行日志,
/// 便于诊断"为什么补偿链和契约不一致")。两者都为空时返回 None。
pub fn resolve_relay_thinking(env_value: Option<&str>, contract_policy: &str) -> Option<String> {
    let contract = contract_policy.trim();
    let contract = (!contract.is_empty()).then(|| contract.to_string());
    let env = env_value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    match (env, contract) {
        (Some(env), Some(contract)) if env != contract => {
            crate::log_line!(
                "relay thinking policy overridden by env: contract={contract} env={env}"
            );
            Some(env)
        }
        (Some(env), _) => Some(env),
        (None, contract) => contract,
    }
}

pub fn provider_supported(provider: &str, shim: &str) -> bool {
    matches!(provider, "deepseek" | "relay") && shim == "off"
}

fn normalize_anthropic_v1_base(base: &str) -> String {
    let mut root = base.trim().trim_end_matches('/').to_string();
    for suffix in ["/messages", "/models"] {
        if root.ends_with(suffix) {
            root.truncate(root.len() - suffix.len());
            while root.ends_with('/') {
                root.pop();
            }
            break;
        }
    }
    if !root.ends_with("/v1") {
        root.push_str("/v1");
    }
    root
}

fn joined_endpoints(
    join: crate::provider_contracts::EndpointJoin,
    transport: &str,
    base: &str,
) -> Result<(String, Option<String>), String> {
    use crate::provider_contracts::EndpointJoin;

    if transport != "anthropic_messages" {
        return Err("provider contract transport cannot use a profile endpoint".into());
    }
    match join {
        EndpointJoin::AnthropicV1 => {
            let root = normalize_anthropic_v1_base(base);
            Ok((format!("{root}/messages"), Some(format!("{root}/models"))))
        }
    }
}

fn upstream_url_for(
    provider: &str,
    default_upstream: String,
    override_raw: Option<String>,
) -> String {
    if provider == "deepseek" {
        override_raw
            .filter(|v| !v.trim().is_empty())
            .unwrap_or(default_upstream)
    } else {
        default_upstream
    }
}

impl GatewayConfig {
    /// 从用户配置直接装配渠道运行配置(单进程服务用)。
    /// 与 `from_env_args` 的差别只是输入来源:策略仍旧来自 provider contract,
    /// 模型目录来自用户在 WebUI 里填的四个槽。
    pub fn for_channel(profile: &crate::profile::Profile) -> Result<Self, String> {
        let (adapter, contract_id) = profile.mode.contract().ok_or("官方模式不使用渠道配置")?;
        let channel = profile
            .channel(&profile.mode)
            .ok_or("当前模式没有渠道配置")?;
        channel.validate()?;

        let contract = crate::provider_contracts::load_runtime_contract(
            adapter,
            Some(contract_id),
            Some(&crate::provider_contracts::catalog_digest()),
        )?;
        let base = channel.base_url.trim().trim_end_matches('/');
        let (upstream_url, models_url) =
            joined_endpoints(contract.endpoint_join, &contract.transport, base)?;
        let catalog = channel.static_catalog(adapter);
        let resolver =
            crate::static_profile::StaticProfileResolver::from_json(&catalog.to_string())?;
        let relay_thinking = resolve_relay_thinking(None, &contract.thinking_policy);

        Ok(Self {
            provider: adapter.to_string(),
            port: profile.port,
            auth_secret: None,
            api_key: Some(channel.api_key.trim().to_string()),
            upstream_url,
            models_url,
            relay_thinking,
            provider_contract: Some(contract),
            intent: GatewayIntent::Formal,
            static_model_resolver: Some(resolver),
            shim_mode: "off".to_string(),
            launch_id: String::new(),
            web_search: channel.web_search,
        })
    }

    pub fn from_env_args(args: Vec<String>) -> Result<Self, String> {
        let mut provider = "deepseek".to_string();
        let mut port: Option<u16> = None;
        let mut auth_token_arg: Option<String> = None;

        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--provider" => {
                    i += 1;
                    provider = args.get(i).ok_or("--provider 缺少值")?.clone();
                }
                "--port" => {
                    i += 1;
                    let raw = args.get(i).ok_or("--port 缺少值")?;
                    port = Some(raw.parse().map_err(|_| format!("非法端口：{raw}"))?);
                }
                "--auth-token" => {
                    i += 1;
                    auth_token_arg = Some(args.get(i).ok_or("--auth-token 缺少值")?.clone());
                }
                other => return Err(format!("未知参数：{other}")),
            }
            i += 1;
        }

        let shim = canonical_shim_mode(
            &provider,
            std::env::var("CSSWITCH_TOOLUSE_SHIM").ok().as_deref(),
        );
        if !provider_supported(&provider, shim) {
            return Err(format!(
                "只支持 deepseek 与 relay 两个 adapter（provider={provider}, shim={shim}）"
            ));
        }

        let expected_contract_id = std::env::var("CSSWITCH_PROVIDER_CONTRACT_ID").ok();
        let expected_contract_digest = std::env::var("CSSWITCH_PROVIDER_CONTRACT_DIGEST").ok();
        let provider_contract = crate::provider_contracts::load_runtime_contract(
            &provider,
            expected_contract_id.as_deref(),
            expected_contract_digest.as_deref(),
        )?;
        let key_env = provider_contract.api_key_env.as_deref().unwrap_or("");
        let api_key = match provider_contract.auth_mode.as_str() {
            "csswitch_oauth" | "none" => None,
            "api_key" => Some(
                std::env::var(key_env)
                    .ok()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| format!("缺少 {key_env}"))?,
            ),
            _ => return Err("provider contract auth mode is unsupported".into()),
        };
        let auth_secret = std::env::var("CSSWITCH_AUTH_TOKEN")
            .ok()
            .filter(|v| !v.is_empty())
            .or(auth_token_arg)
            .filter(|v| !v.is_empty());
        let mut models_url = None;
        let mut relay_thinking = None;
        let default_upstream = if provider_contract.endpoint_policy == "profile_required" {
            let base_env = if matches!(
                provider_contract.transport.as_str(),
                "openai_chat" | "openai_responses"
            ) {
                "CSSWITCH_OPENAI_BASE_URL"
            } else {
                "CSSWITCH_RELAY_BASE_URL"
            };
            let base = std::env::var(base_env)
                .ok()
                .map(|value| value.trim().trim_end_matches('/').to_string())
                .filter(|v| {
                    !v.is_empty() && (v.starts_with("http://") || v.starts_with("https://"))
                })
                .ok_or_else(|| format!("{provider} 需要 {base_env}=http(s)://..."))?;
            let (inference, discovered_models) = joined_endpoints(
                provider_contract.endpoint_join,
                &provider_contract.transport,
                &base,
            )?;
            models_url = discovered_models;
            if provider_contract.transport == "anthropic_messages" {
                // 策略的权威来源是 provider contract 的 thinking_policy。
                // 曾经只读环境变量,任何忘记注入的启动路径都会静默丢掉全部
                // thinking 类补偿(实测表现为上游 400)。env 现在只是显式 override。
                relay_thinking = resolve_relay_thinking(
                    std::env::var("CSSWITCH_RELAY_THINKING").ok().as_deref(),
                    &provider_contract.thinking_policy,
                );
            }
            inference
        } else {
            DEFAULT_UPSTREAM_URL.to_string()
        };
        let upstream_url = upstream_url_for(
            &provider,
            default_upstream,
            std::env::var("CSSWITCH_UPSTREAM_URL").ok(),
        );
        let launch_id = std::env::var("CSSWITCH_LAUNCH_ID")
            .unwrap_or_default()
            .trim()
            .to_string();
        let managed_launch_id = (24..=128).contains(&launch_id.len())
            && launch_id.chars().all(|value| value.is_ascii_hexdigit());
        if managed_launch_id
            && (expected_contract_id.is_none() || expected_contract_digest.is_none())
        {
            return Err(
                "managed gateway launch requires an exact provider contract identity".into(),
            );
        }
        let static_model_resolver = std::env::var(crate::static_profile::ENV_NAME)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| crate::static_profile::StaticProfileResolver::from_json(&value))
            .transpose()?;
        let intent = GatewayIntent::from_env()?;
        match intent {
            GatewayIntent::Formal | GatewayIntent::ScratchMessage => {
                let resolver = static_model_resolver.as_ref().ok_or_else(|| {
                    format!("{provider} 缺少 {}", crate::static_profile::ENV_NAME)
                })?;
                if resolver.adapter() != provider {
                    return Err("静态模型目录 adapter 与 gateway provider 不一致".into());
                }
            }
            GatewayIntent::ScratchModels if static_model_resolver.is_some() => {
                return Err("scratch-models 禁止注入静态模型目录".into());
            }
            GatewayIntent::ScratchModels => {}
        }
        Ok(Self {
            provider,
            port: port.ok_or("--port 必填")?,
            auth_secret,
            api_key,
            upstream_url,
            models_url,
            relay_thinking,
            provider_contract: Some(provider_contract),
            intent,
            static_model_resolver,
            shim_mode: shim.to_string(),
            launch_id,
            web_search: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{joined_endpoints, provider_supported, upstream_url_for};
    use crate::provider_contracts::EndpointJoin;

    #[test]
    fn provider_support_matrix_accepts_only_canonical_shims() {
        assert!(provider_supported("deepseek", "off"));
        assert!(!provider_supported("deepseek", "rewrite"));
        assert!(provider_supported("relay", "off"));
        assert!(!provider_supported("relay", "rewrite"));
        for retired in ["qwen", "openai-custom", "openai-responses", "codex"] {
            assert!(!provider_supported(retired, "off"), "{retired} 已退役");
        }
    }

    #[test]
    fn thinking_policy_comes_from_the_contract_without_any_env() {
        // 回归:曾经只读 env,忘记注入就静默丢掉全部 thinking 补偿(上游 400)。
        assert_eq!(
            super::resolve_relay_thinking(None, "upstream_default"),
            Some("upstream_default".to_string())
        );
        assert_eq!(
            super::resolve_relay_thinking(Some("  "), "deepseek_native"),
            Some("deepseek_native".to_string())
        );
        assert_eq!(super::resolve_relay_thinking(None, ""), None);
    }

    #[test]
    fn env_overrides_the_contract_thinking_policy() {
        assert_eq!(
            super::resolve_relay_thinking(Some("enabled"), "upstream_default"),
            Some("enabled".to_string())
        );
        assert_eq!(
            super::resolve_relay_thinking(Some("enabled"), ""),
            Some("enabled".to_string())
        );
    }

    #[test]
    fn anthropic_endpoint_join_handles_bare_v1_and_full_message_urls() {
        assert_eq!(
            joined_endpoints(
                EndpointJoin::AnthropicV1,
                "anthropic_messages",
                "http://cpa.ga17.com"
            )
            .unwrap(),
            (
                "http://cpa.ga17.com/v1/messages".into(),
                Some("http://cpa.ga17.com/v1/models".into())
            )
        );
        assert_eq!(
            joined_endpoints(
                EndpointJoin::AnthropicV1,
                "anthropic_messages",
                "https://api.kimi.com/coding/v1/messages"
            )
            .unwrap(),
            (
                "https://api.kimi.com/coding/v1/messages".into(),
                Some("https://api.kimi.com/coding/v1/models".into())
            )
        );
        assert_eq!(
            joined_endpoints(
                EndpointJoin::AnthropicV1,
                "anthropic_messages",
                "https://api.kimi.com/coding/v1"
            )
            .unwrap(),
            (
                "https://api.kimi.com/coding/v1/messages".into(),
                Some("https://api.kimi.com/coding/v1/models".into())
            )
        );
        assert_eq!(
            joined_endpoints(
                EndpointJoin::AnthropicV1,
                "anthropic_messages",
                "https://relay.example.test/anthropic"
            )
            .unwrap(),
            (
                "https://relay.example.test/anthropic/v1/messages".into(),
                Some("https://relay.example.test/anthropic/v1/models".into())
            )
        );
    }

    #[test]
    fn upstream_override_is_native_only() {
        let poison = Some("http://127.0.0.1:1/poison".to_string());
        assert_eq!(
            upstream_url_for(
                "deepseek",
                "https://default/deepseek".to_string(),
                poison.clone()
            ),
            "http://127.0.0.1:1/poison"
        );
        // relay 的上游由契约 + base_url 决定,环境变量不得越过它。
        assert_eq!(
            upstream_url_for("relay", "http://candidate/v1/messages".to_string(), poison),
            "http://candidate/v1/messages"
        );
    }
}
