//! 推理请求的调用类型:会话的主对话、子任务,还是 Science 的某个后台调用。
//!
//! 只看两样东西:`metadata.user_id` 的结构,以及请求形状。请求头不随调用类型变化,不看。
//! Science 0.1.72 的格式(静态核对):`user_id` 是一个 JSON 字符串
//! `{"session_id":"<root32>[:<frame24>][:<lane>]","frame_id":"<frame32>"}`。
//! - 根 frame 的主对话没有后缀;子 frame(子 agent、reviewer 等)是 `root:frame24`;
//! - 带 lane 的调用在最后多一段,lane 匹配 `^[a-z0-9]{1,6}$`(`ttl` 标题、`mem` 记忆提取、
//!   `amc` auto mode 分类器……);
//! - 不带 lane 的另有两类靠形状认:`max_tokens == 1` 是缓存保活,
//!   `tool_choice.type == "none"` 是同一会话的 fork(记忆提取、补充等)。
//!
//! 对外只有推导出的类型。会话 id、frame id 与 `metadata` 的其它内容不离开这个模块:
//! 原样输出的只有 lane 代号,它必须过正则(至多 6 个小写字母数字),最短的 id 也有 24 位。
//! 这里只读请求、不改请求:官方直通转发的仍是原样的字节。

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallKind {
    /// 会话的主对话(根 frame)。
    Main,
    /// 子 frame:子 agent、reviewer 等。
    Sub,
    /// 缓存保活:`max_tokens == 1`,不带 lane。
    KeepAlive,
    /// 同一会话的 fork:`tool_choice.type == "none"`,不带 lane。
    Fork,
    /// 带 lane 后缀的后台调用,值是那段小写代号。
    Lane(String),
    /// 没有 `metadata.user_id`,或它不是上面的格式。
    Unknown,
}

impl CallKind {
    /// 服务日志里 `lane=` 的取值;认不出时是 `-`。
    pub fn as_str(&self) -> &str {
        match self {
            Self::Main => "main",
            Self::Sub => "sub",
            Self::KeepAlive => "keepalive",
            Self::Fork => "fork",
            Self::Lane(lane) => lane,
            Self::Unknown => "-",
        }
    }

    /// 控制面日志条目里的取值;认不出时没有(序列化成 `null`)。
    pub fn known(&self) -> Option<&str> {
        (*self != Self::Unknown).then(|| self.as_str())
    }
}

/// 已解析成 JSON 的请求体(relay 路径)。必须在补偿链改写之前取:
/// 补偿链会改 `tool_choice`。
pub fn of_request(request: &Value) -> CallKind {
    classify(
        request.get("metadata"),
        request.get("max_tokens"),
        request.get("tool_choice"),
    )
}

/// 原始请求体字节(官方直通)。只取需要的三个字段,其余跳过,不为整个请求建树。
pub fn of_body(body: &[u8]) -> CallKind {
    #[derive(Deserialize)]
    struct Shape {
        #[serde(default)]
        metadata: Option<Value>,
        #[serde(default)]
        max_tokens: Option<Value>,
        #[serde(default)]
        tool_choice: Option<Value>,
    }
    // 结构体也能从 JSON 数组按位置反序列化;请求体必须是对象,与 `of_request` 口径一致。
    if body.trim_ascii_start().first() != Some(&b'{') {
        return CallKind::Unknown;
    }
    match serde_json::from_slice::<Shape>(body) {
        Ok(shape) => classify(
            shape.metadata.as_ref(),
            shape.max_tokens.as_ref(),
            shape.tool_choice.as_ref(),
        ),
        Err(_) => CallKind::Unknown,
    }
}

/// 分类本身。边界:
/// - 先认 `session_id`。`user_id` 缺失、不是字符串、不是 JSON 对象、`session_id` 不是字符串,
///   或 `session_id` 不是下面三种形状之一 → `Unknown`。认不出 `session_id` 时请求形状不算数:
///   `max_tokens == 1` 本身不说明这是 Science 的保活。
/// - `session_id` 按 `:` 分段:第一段是 32 位十六进制(根 frame);第二段若是 24 位十六进制
///   就是 frame,否则必须是 lane;有第三段时第二段必须是 frame、第三段必须是 lane。
///   frame 的判断在 lane 之前(lane 至多 6 位,两者其实不会撞上)。空段、多于三段 → `Unknown`。
/// - 有 lane → `Lane`,不再看形状。没有 lane 时依次看 `max_tokens == 1` → `KeepAlive`、
///   `tool_choice.type == "none"` → `Fork`;都不是 → 有 frame 段为 `Sub`,否则 `Main`。
/// - id 段大小写都认(十六进制),lane 只认小写。
fn classify(
    metadata: Option<&Value>,
    max_tokens: Option<&Value>,
    tool_choice: Option<&Value>,
) -> CallKind {
    let Some(session_id) = metadata
        .and_then(|metadata| metadata.get("user_id"))
        .and_then(Value::as_str)
        .and_then(|user_id| serde_json::from_str::<Value>(user_id).ok())
        .and_then(|user_id| {
            user_id
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
    else {
        return CallKind::Unknown;
    };
    let mut segments = session_id.split(':');
    let root = segments.next().unwrap_or_default();
    if !is_hex_id(root, 32) {
        return CallKind::Unknown;
    }
    let (has_frame, lane) = match (segments.next(), segments.next(), segments.next()) {
        (None, _, _) => (false, None),
        (Some(second), None, _) if is_hex_id(second, 24) => (true, None),
        (Some(second), None, _) if is_lane(second) => (false, Some(second)),
        (Some(frame), Some(lane), None) if is_hex_id(frame, 24) && is_lane(lane) => {
            (true, Some(lane))
        }
        _ => return CallKind::Unknown,
    };
    if let Some(lane) = lane {
        return CallKind::Lane(lane.to_owned());
    }
    if max_tokens.and_then(Value::as_u64) == Some(1) {
        return CallKind::KeepAlive;
    }
    if tool_choice
        .and_then(|choice| choice.get("type"))
        .and_then(Value::as_str)
        == Some("none")
    {
        return CallKind::Fork;
    }
    if has_frame {
        CallKind::Sub
    } else {
        CallKind::Main
    }
}

fn is_hex_id(segment: &str, len: usize) -> bool {
    segment.len() == len && segment.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Science 的 lane 正则 `^[a-z0-9]{1,6}$`。
fn is_lane(segment: &str) -> bool {
    (1..=6).contains(&segment.len())
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::{of_body, of_request, CallKind};

    const ROOT: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";
    const FRAME24: &str = "aa11bb22cc33dd44ee55ff66";

    /// Science 实际发出的形状:`user_id` 是 JSON 字符串,frame_id 是 32 位。
    fn user_id(session_id: &str) -> Value {
        Value::String(
            json!({"session_id": session_id, "frame_id": "ffffffffffffffffffffffffffffffff"})
                .to_string(),
        )
    }

    fn request(metadata: Option<Value>, max_tokens: u64, tool_choice: Option<Value>) -> Value {
        let mut body = json!({
            "model": "claude-opus-5",
            "max_tokens": max_tokens,
            "messages": [{"role": "user", "content": "hi"}],
        });
        if let Some(metadata) = metadata {
            body["metadata"] = metadata;
        }
        if let Some(choice) = tool_choice {
            body["tool_choice"] = choice;
        }
        body
    }

    fn with_session(session_id: &str) -> Option<Value> {
        Some(json!({"user_id": user_id(session_id)}))
    }

    /// 同一张表同时过两个入口:relay 用已解析的 JSON,官方直通用原始字节。
    /// 两个入口口径不一,正式实例在官方与第三方模式下就会给同一条请求标出不同的类型。
    #[test]
    fn classifies_science_metadata_through_both_entry_points() {
        let lane = |name: &str| CallKind::Lane(name.into());
        let none = Some(json!({"type": "none"}));
        let cases: Vec<(&str, Value, CallKind)> = vec![
            (
                "主对话",
                request(with_session(ROOT), 32000, None),
                CallKind::Main,
            ),
            (
                "子 frame",
                request(with_session(&format!("{ROOT}:{FRAME24}")), 32000, None),
                CallKind::Sub,
            ),
            (
                "root:lane",
                request(with_session(&format!("{ROOT}:ttl")), 32000, None),
                lane("ttl"),
            ),
            (
                "root:frame24:lane",
                request(with_session(&format!("{ROOT}:{FRAME24}:mem")), 2048, None),
                lane("mem"),
            ),
            (
                "带数字的 lane",
                request(with_session(&format!("{ROOT}:kllm")), 4096, None),
                lane("kllm"),
            ),
            (
                "保活",
                request(with_session(ROOT), 1, None),
                CallKind::KeepAlive,
            ),
            (
                "子 frame 的保活",
                request(with_session(&format!("{ROOT}:{FRAME24}")), 1, None),
                CallKind::KeepAlive,
            ),
            (
                "有 lane 时不看形状",
                request(with_session(&format!("{ROOT}:ttl")), 1, none.clone()),
                lane("ttl"),
            ),
            (
                "fork",
                request(with_session(ROOT), 2048, none.clone()),
                CallKind::Fork,
            ),
            (
                "强制工具不是 fork",
                request(
                    with_session(ROOT),
                    2048,
                    Some(json!({"type": "tool", "name": "emit_memories"})),
                ),
                CallKind::Main,
            ),
            (
                "id 段大写也认",
                request(with_session(&ROOT.to_ascii_uppercase()), 32000, None),
                CallKind::Main,
            ),
            (
                "user_id 不是 JSON",
                request(Some(json!({"user_id": "{\"session_id\":"})), 32000, None),
                CallKind::Unknown,
            ),
            (
                "user_id 是别的客户端的写法",
                request(
                    Some(json!({"user_id": format!("user_{ROOT}_session_{ROOT}")})),
                    32000,
                    None,
                ),
                CallKind::Unknown,
            ),
            (
                "缺 metadata,即使形状像保活",
                request(None, 1, None),
                CallKind::Unknown,
            ),
            (
                "metadata 里没有 user_id",
                request(Some(json!({})), 32000, None),
                CallKind::Unknown,
            ),
            (
                "user_id 不是字符串",
                request(Some(json!({"user_id": {"session_id": ROOT}})), 32000, None),
                CallKind::Unknown,
            ),
            (
                "session_id 不是字符串",
                request(
                    Some(json!({"user_id": json!({"session_id": 42}).to_string()})),
                    32000,
                    None,
                ),
                CallKind::Unknown,
            ),
            (
                "lane 含大写",
                request(with_session(&format!("{ROOT}:TTL")), 32000, None),
                CallKind::Unknown,
            ),
            (
                "lane 超长",
                request(with_session(&format!("{ROOT}:abcdefg")), 32000, None),
                CallKind::Unknown,
            ),
            (
                "callSite 不是 lane(rc_enrich)",
                request(with_session(&format!("{ROOT}:rc_enrich")), 32000, None),
                CallKind::Unknown,
            ),
            (
                "根 id 长度不对",
                request(with_session(&ROOT[1..]), 32000, None),
                CallKind::Unknown,
            ),
            (
                "三段时第二段必须是 frame",
                request(with_session(&format!("{ROOT}:ttl:mem")), 32000, None),
                CallKind::Unknown,
            ),
            (
                "多于三段",
                request(
                    with_session(&format!("{ROOT}:{FRAME24}:ttl:mem")),
                    32000,
                    None,
                ),
                CallKind::Unknown,
            ),
            (
                "空段",
                request(with_session(&format!("{ROOT}:")), 32000, None),
                CallKind::Unknown,
            ),
        ];
        for (name, body, expected) in cases {
            assert_eq!(of_request(&body), expected, "{name}(已解析)");
            let bytes = serde_json::to_vec(&body).unwrap();
            assert_eq!(of_body(&bytes), expected, "{name}(原始字节)");
        }
    }

    #[test]
    fn unparseable_bodies_are_unknown() {
        // 按字段顺序排成数组:结构体能按位置把它反序列化成「保活」,已解析的入口却认不出。
        // 请求体不是对象时两个入口都必须是 Unknown。
        let positional = json!([{"user_id": user_id(ROOT)}, 1, null]);
        assert_eq!(of_request(&positional), CallKind::Unknown);
        for body in [
            b"not json".to_vec(),
            b"".to_vec(),
            serde_json::to_vec(&positional).unwrap(),
        ] {
            assert_eq!(of_body(&body), CallKind::Unknown);
        }
    }
}
