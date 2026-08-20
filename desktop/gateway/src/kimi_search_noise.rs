//! Kimi 搜索轮响应侧的噪声整形。
//!
//! 三条窄规则,证据均来自 2026-08-19 的隔离探测与真实会话
//! (任务研究记录 2026-08-19-native-probe.md / 2026-08-19-live-evidence.md):
//!
//! 1. **噪声头剥离** `provider.kimi.search-noise-text-strip`
//!    上游对声明了原生 `web_search_20250305` 的轮次会注入独立 text 块,内容恒为
//!    `Search results for query: <query>`,与相邻 `server_tool_use.input.query`
//!    完全重复;K2.7 还会在 turn 末尾发悬挂噪声头(宣布下一次搜索却直接
//!    end_turn)。整块剥离,零信息损失。
//!
//! 2. **空壳搜索对剥离** `provider.kimi.empty-search-pair-strip`
//!    模型被告知无需搜索时,上游仍可能发出幻影对:`server_tool_use` 连 id 都
//!    没有,紧跟 `content: []` 的 `web_search_tool_result`(同样无 tool_use_id)。
//!    该对经 Science 落盘后成为无 id 孤儿结果块,此后每一轮都 400
//!    `tool_call_id is not found`(真实会话复现);UI 侧则渲染为一个空的
//!    Server Tool 框。整对剥离。判据收窄为**两半都无键且 content 为显式 `[]`**
//!    (与探测记录的幻影形态精确一致);带键的空结果是真实零结果搜索,走采钥保留;
//!    content 缺失或非数组不算空(上游 schema 漂移必须保持可见),按无钥对放行。
//!
//! 3. **搜索对配对键采钥** `provider.kimi.search-pair-id-adopt`
//!    真实搜索对的两半配对键恒不匹配:`server_tool_use.id` 为 `tool_…`,
//!    `web_search_tool_result.tool_use_id` 为 `srvtoolu_…`(live 证据 D5)。
//!    原样放行会让 Science 落盘时丢弃无法配对的 `server_tool_use`,查询词永久
//!    丢失,此后每轮请求都依赖请求侧 pairing-repair 以空壳兜底。放行前把
//!    `use.id` 改写为同对结果块的 `tool_use_id`(**只采用上游已有的键,
//!    不发明新键**);result 侧无键而 use 侧有键时反向补齐。两半都无键的
//!    非空对不归一,如实放行仅记数。采钥不改变块数与索引。
//!
//! 规则 2/3 只对 `name == "web_search"` 的 `server_tool_use` 生效;其它
//! server tool 及其相邻结果块不参与配对判定,字节级直通(2026-08-20 收窄,
//! 防止 `web_search_tool_result` 反向把任意 server tool 认作搜索调用)。
//!
//! 被吞的块不占用输出索引(后续块索引前移补洞);**未命中的流量保持字节级
//! 零改写**。规则 ID 与吞块 / 采钥计数记入服务日志。

use serde_json::Value;

use crate::anthropic_compat::{event_and_data, passthrough, render_sse, split_frame};

pub const RULE_PROVIDER_KIMI_SEARCH_NOISE_TEXT_STRIP: &str =
    "provider.kimi.search-noise-text-strip";
pub const RULE_PROVIDER_KIMI_EMPTY_SEARCH_PAIR_STRIP: &str =
    "provider.kimi.empty-search-pair-strip";
pub const RULE_PROVIDER_KIMI_SEARCH_PAIR_ID_ADOPT: &str = "provider.kimi.search-pair-id-adopt";

/// 上游注入的噪声头前缀(实测恒定)。
pub const NOISE_PREFIX: &str = "Search results for query:";

/// 单帧上限,与旧过滤器一致的有界缓冲。
const MAX_FRAME_BYTES: usize = 1024 * 1024;
/// 判定期挂起帧总字节上限。正常判定在几个帧内完成,该上限只防御病态流。
const MAX_PENDING_BYTES: usize = 256 * 1024;

/// 判定期挂起的一帧:是否属于待判块本体(ping 等穿插帧不是)。
struct HeldFrame {
    bytes: Vec<u8>,
    belongs_to_block: bool,
}

/// 待判 text 块:凑满 `NOISE_PREFIX` 长度即拍板。
struct PendingText {
    accum: String,
}

/// 待判 `server_tool_use` 块:块结束后还要看下一个块才能拍板。
struct PendingServerTool {
    /// 块首帧携带的非空 id(采钥判定要比较键值,不只看有无)。
    id: Option<String>,
    closed: bool,
}

enum PendingKind {
    Text(PendingText),
    ServerTool(PendingServerTool),
}

struct Pending {
    upstream_index: u64,
    kind: PendingKind,
    held: Vec<HeldFrame>,
    held_bytes: usize,
}

#[derive(Default, Clone, Copy)]
pub struct StripStats {
    pub noise_blocks: usize,
    pub pair_blocks: usize,
    pub bytes: usize,
    /// 被采钥归一的搜索对数(每对计 1)。
    pub adopted_pairs: usize,
    /// 两半都无键但内容非空、如实放行的对数(仅记日志,不改写)。
    pub unkeyed_pairs: usize,
}

impl StripStats {
    pub fn total_blocks(&self) -> usize {
        self.noise_blocks + self.pair_blocks
    }

    /// 是否有任何剥离 / 采钥 / 无钥对——决定要不要记日志。
    pub fn any_activity(&self) -> bool {
        self.total_blocks() > 0 || self.adopted_pairs > 0 || self.unkeyed_pairs > 0
    }

    /// 是否改写过响应内容——决定非流式要不要重序列化 body。
    pub fn rewrote_body(&self) -> bool {
        self.total_blocks() > 0 || self.adopted_pairs > 0
    }
}

#[derive(Default)]
pub struct SearchNoiseFilter {
    buf: Vec<u8>,
    stats: StripStats,
    /// 已吞块总数;为 0 时所有输出保持原字节。
    stripped_blocks: usize,
    /// 下一个输出索引(吞块不占位)。
    next_output_index: u64,
    /// 当前敞开的直通块:(上游索引, 输出索引)。
    active_output_block: Option<(u64, u64)>,
    /// 正在吞的块的上游索引,以及它记入哪个计数。
    swallowing: Option<(u64, SwallowKind)>,
    pending: Option<Pending>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SwallowKind {
    Noise,
    Pair,
}

impl SearchNoiseFilter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn stats(&self) -> StripStats {
        self.stats
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<u8>, String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some((frame, sep_len, rest)) = split_frame(&self.buf) {
            let sep = self.buf[frame.len()..frame.len() + sep_len].to_vec();
            out.extend_from_slice(&self.handle_frame(&frame, &sep)?);
            self.buf = rest;
        }
        if self.buf.len() > MAX_FRAME_BYTES {
            return Err("Kimi noise strip frame exceeds the bounded buffer".into());
        }
        Ok(out)
    }

    pub fn finalize(&mut self) -> Result<Vec<u8>, String> {
        if !self.buf.iter().all(u8::is_ascii_whitespace) {
            return Err("Kimi noise strip stream ended inside a frame".into());
        }
        self.buf.clear();
        // 流在未定块中途截断:如实放行已挂起的帧,生命周期由下游校验器判。
        Ok(self.flush_pending())
    }

    fn handle_frame(&mut self, frame: &[u8], sep: &[u8]) -> Result<Vec<u8>, String> {
        let (event, data) = event_and_data(frame);
        let Ok(obj) = serde_json::from_slice::<Value>(&data) else {
            // 非 JSON 帧(注释等):不参与块判定,按穿插帧处理。
            return Ok(self.emit_or_hold_interleaved(frame, sep));
        };
        let kind = obj.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "content_block_start" => self.handle_block_start(event.as_deref(), obj, frame, sep),
            "content_block_delta" => {
                let index = block_index(&obj)?;
                if self.swallowing.is_some_and(|(i, _)| i == index) {
                    self.stats.bytes += frame.len() + sep.len();
                    return Ok(Vec::new());
                }
                if let Some(pending) = self.pending.as_mut() {
                    if pending.upstream_index == index {
                        if let PendingKind::Text(text) = &mut pending.kind {
                            if let Some(delta) = obj
                                .get("delta")
                                .and_then(Value::as_object)
                                .and_then(|delta| delta.get("text"))
                                .and_then(Value::as_str)
                            {
                                text.accum.push_str(delta);
                            }
                        }
                        self.hold_frame(frame, sep, true)?;
                        return Ok(self.decide_text());
                    }
                }
                Ok(self.pass_indexed(index, event.as_deref(), obj, frame, sep))
            }
            "content_block_stop" => {
                let index = block_index(&obj)?;
                if let Some((swallow_index, swallow_kind)) = self.swallowing {
                    if swallow_index == index {
                        self.stats.bytes += frame.len() + sep.len();
                        self.count_swallowed_block(swallow_kind);
                        self.swallowing = None;
                        return Ok(Vec::new());
                    }
                }
                if let Some(pending) = self.pending.as_mut() {
                    if pending.upstream_index == index {
                        match &mut pending.kind {
                            // text 块整体结束仍未凑满前缀:不是噪声,原样放行。
                            PendingKind::Text(_) => {
                                let mut out = self.flush_pending();
                                out.extend_from_slice(&self.pass_indexed(
                                    index,
                                    event.as_deref(),
                                    obj,
                                    frame,
                                    sep,
                                ));
                                self.active_output_block = None;
                                return Ok(out);
                            }
                            // server_tool_use 块结束还不能拍板:等下一个块。
                            PendingKind::ServerTool(server) => {
                                server.closed = true;
                                self.hold_frame(frame, sep, true)?;
                                return Ok(Vec::new());
                            }
                        }
                    }
                }
                let out = self.pass_indexed(index, event.as_deref(), obj, frame, sep);
                self.active_output_block = None;
                Ok(out)
            }
            // turn 级帧到来说明配对不会再出现:先放行挂起块,再放行本帧。
            "message_delta" | "message_stop" => {
                let mut out = self.flush_pending();
                out.extend_from_slice(&passthrough(frame, sep));
                Ok(out)
            }
            // ping / error 等:未定块挂起期间保持顺序,其余直通。
            _ => Ok(self.emit_or_hold_interleaved(frame, sep)),
        }
    }

    fn handle_block_start(
        &mut self,
        event: Option<&str>,
        obj: Value,
        frame: &[u8],
        sep: &[u8],
    ) -> Result<Vec<u8>, String> {
        let index = block_index(&obj)?;
        // 已关闭的 server_tool_use 待判块:下一个块揭晓配对。
        let closed_server_use_id = match self.pending.as_ref() {
            Some(Pending {
                kind: PendingKind::ServerTool(server),
                ..
            }) if server.closed => Some(server.id.clone()),
            _ => None,
        };
        if let Some(use_id) = closed_server_use_id {
            let block = obj.get("content_block").and_then(Value::as_object);
            let is_result = block
                .and_then(|block| block.get("type"))
                .and_then(Value::as_str)
                == Some("web_search_tool_result");
            if is_result {
                let result_key = block
                    .and_then(|block| block.get("tool_use_id"))
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string);
                let content_empty = matches!(
                    block.and_then(|block| block.get("content")),
                    Some(Value::Array(content)) if content.is_empty()
                );
                // 幻影对(两半都无键、内容为空,与探测形态精确一致):吞掉整对。
                if use_id.is_none() && result_key.is_none() && content_empty {
                    let pending = self.pending.take().expect("checked above");
                    let mut out = Vec::new();
                    for held in pending.held {
                        if held.belongs_to_block {
                            self.stats.bytes += held.bytes.len();
                        } else {
                            out.extend_from_slice(&held.bytes);
                        }
                    }
                    self.count_swallowed_block(SwallowKind::Pair);
                    self.stats.bytes += frame.len() + sep.len();
                    self.swallowing = Some((index, SwallowKind::Pair));
                    return Ok(out);
                }
                // 采钥:以 result 侧上游已有的键为准,把 use.id 归一成同一值。
                if let Some(key) = result_key {
                    if use_id.as_deref() != Some(key.as_str()) {
                        let mut out = self.flush_pending_adopting(Some(&key));
                        out.extend_from_slice(
                            &self.start_new_block(index, event, obj, frame, sep)?,
                        );
                        self.stats.adopted_pairs += 1;
                        return Ok(out);
                    }
                    // 已配对(id == key):零命中,原样放行。
                } else if let Some(use_key) = use_id {
                    // 反向采钥:use 有键、result 无键 → 结果块补上 use 侧的键。
                    let mut out = self.flush_pending();
                    out.extend_from_slice(
                        &self.pass_result_start_with_key(index, event, obj, &use_key),
                    );
                    self.stats.adopted_pairs += 1;
                    return Ok(out);
                } else {
                    // 两半都无键但内容非空:不发明配对键,如实放行,仅记数。
                    self.stats.unkeyed_pairs += 1;
                }
            }
            let mut out = self.flush_pending();
            out.extend_from_slice(&self.start_new_block(index, event, obj, frame, sep)?);
            return Ok(out);
        }
        if self.pending.is_some() || self.swallowing.is_some() {
            return Err("Kimi noise strip saw overlapping content blocks".into());
        }
        self.start_new_block(index, event, obj, frame, sep)
    }

    fn start_new_block(
        &mut self,
        index: u64,
        event: Option<&str>,
        obj: Value,
        frame: &[u8],
        sep: &[u8],
    ) -> Result<Vec<u8>, String> {
        let block = obj.get("content_block").and_then(Value::as_object);
        let block_type = block
            .and_then(|block| block.get("type"))
            .and_then(Value::as_str);
        match block_type {
            Some("text") => {
                let seed = block
                    .and_then(|block| block.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                self.pending = Some(Pending {
                    upstream_index: index,
                    kind: PendingKind::Text(PendingText { accum: seed }),
                    held: Vec::new(),
                    held_bytes: 0,
                });
                self.hold_frame(frame, sep, true)?;
                Ok(self.decide_text())
            }
            Some("server_tool_use")
                if block
                    .and_then(|block| block.get("name"))
                    .and_then(Value::as_str)
                    == Some("web_search") =>
            {
                let id = block
                    .and_then(|block| block.get("id"))
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string);
                self.pending = Some(Pending {
                    upstream_index: index,
                    kind: PendingKind::ServerTool(PendingServerTool { id, closed: false }),
                    held: Vec::new(),
                    held_bytes: 0,
                });
                self.hold_frame(frame, sep, true)?;
                Ok(Vec::new())
            }
            _ => Ok(self.pass_block_start(index, event, obj, frame, sep)),
        }
    }

    fn count_swallowed_block(&mut self, kind: SwallowKind) {
        self.stripped_blocks += 1;
        match kind {
            SwallowKind::Noise => self.stats.noise_blocks += 1,
            SwallowKind::Pair => self.stats.pair_blocks += 1,
        }
    }

    /// text 挂起判定:凑满前缀长度即拍板;提前分叉立即放行。
    fn decide_text(&mut self) -> Vec<u8> {
        let Some(pending) = self.pending.as_ref() else {
            return Vec::new();
        };
        let PendingKind::Text(text) = &pending.kind else {
            return Vec::new();
        };
        if text.accum.starts_with(NOISE_PREFIX) {
            let pending = self.pending.take().expect("checked above");
            let mut out = Vec::new();
            for held in pending.held {
                if held.belongs_to_block {
                    self.stats.bytes += held.bytes.len();
                } else {
                    out.extend_from_slice(&held.bytes);
                }
            }
            self.swallowing = Some((pending.upstream_index, SwallowKind::Noise));
            return out;
        }
        let still_possible = text.accum.len() < NOISE_PREFIX.len()
            && NOISE_PREFIX.as_bytes().starts_with(text.accum.as_bytes());
        if still_possible {
            return Vec::new();
        }
        self.flush_pending()
    }

    /// 把未定块按"保留"放行:块首帧登记索引映射,已关闭的块最后清空敞开状态。
    fn flush_pending(&mut self) -> Vec<u8> {
        self.flush_pending_adopting(None)
    }

    /// 同 [`Self::flush_pending`],但 `adopt_id` 非空时把块首帧的
    /// `content_block.id` 采钥为该值后重渲(索引照常登记,不产生空洞)。
    fn flush_pending_adopting(&mut self, adopt_id: Option<&str>) -> Vec<u8> {
        let Some(pending) = self.pending.take() else {
            return Vec::new();
        };
        let closed = matches!(
            &pending.kind,
            PendingKind::ServerTool(PendingServerTool { closed: true, .. })
        );
        let mut out = Vec::new();
        let mut block_started = false;
        for held in pending.held {
            if !held.belongs_to_block {
                out.extend_from_slice(&held.bytes);
                continue;
            }
            let adopt_this_frame = adopt_id.is_some() && !block_started;
            if self.stripped_blocks == 0 && !adopt_this_frame {
                // 零命中路径:字节级零改写。
                if !block_started {
                    block_started = true;
                    self.active_output_block =
                        Some((pending.upstream_index, pending.upstream_index));
                    self.next_output_index = pending.upstream_index + 1;
                }
                out.extend_from_slice(&held.bytes);
                continue;
            }
            let Some((frame, sep)) = split_held(&held.bytes) else {
                out.extend_from_slice(&held.bytes);
                continue;
            };
            let (event, data) = event_and_data(&frame);
            let Ok(mut obj) = serde_json::from_slice::<Value>(&data) else {
                out.extend_from_slice(&held.bytes);
                continue;
            };
            if !block_started {
                block_started = true;
                if let Some(key) = adopt_id {
                    if let Some(block) = obj.get_mut("content_block").and_then(Value::as_object_mut)
                    {
                        block.insert("id".to_string(), Value::String(key.to_string()));
                    }
                    out.extend_from_slice(&self.render_block_start(
                        pending.upstream_index,
                        event.as_deref(),
                        obj,
                    ));
                } else {
                    out.extend_from_slice(&self.pass_block_start(
                        pending.upstream_index,
                        event.as_deref(),
                        obj,
                        &frame,
                        &sep,
                    ));
                }
            } else {
                out.extend_from_slice(&self.pass_indexed(
                    pending.upstream_index,
                    event.as_deref(),
                    obj,
                    &frame,
                    &sep,
                ));
            }
        }
        if closed {
            self.active_output_block = None;
        }
        out
    }

    fn hold_frame(
        &mut self,
        frame: &[u8],
        sep: &[u8],
        belongs_to_block: bool,
    ) -> Result<(), String> {
        let pending = self.pending.as_mut().expect("hold_frame requires pending");
        pending.held_bytes += frame.len() + sep.len();
        if pending.held_bytes > MAX_PENDING_BYTES {
            return Err("Kimi noise strip pending buffer exceeds the bound".into());
        }
        pending.held.push(HeldFrame {
            bytes: passthrough(frame, sep),
            belongs_to_block,
        });
        Ok(())
    }

    fn emit_or_hold_interleaved(&mut self, frame: &[u8], sep: &[u8]) -> Vec<u8> {
        if self.pending.is_some() {
            // 挂起期间的穿插帧(ping 等)同样保序。
            if self.hold_frame(frame, sep, false).is_err() {
                // 超限即刻放弃判定,按保留放行,不吞任何内容。
                let mut out = self.flush_pending();
                out.extend_from_slice(&passthrough(frame, sep));
                return out;
            }
            return Vec::new();
        }
        passthrough(frame, sep)
    }

    fn pass_block_start(
        &mut self,
        upstream_index: u64,
        event: Option<&str>,
        obj: Value,
        frame: &[u8],
        sep: &[u8],
    ) -> Vec<u8> {
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        self.active_output_block = Some((upstream_index, output_index));
        self.rewrite_or_pass(output_index, upstream_index, event, obj, frame, sep)
    }

    /// 块首帧强制重渲(索引照常登记):即使输出索引不变,也要携带改写后的字段。
    fn render_block_start(
        &mut self,
        upstream_index: u64,
        event: Option<&str>,
        mut obj: Value,
    ) -> Vec<u8> {
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        self.active_output_block = Some((upstream_index, output_index));
        if output_index != upstream_index {
            if let Some(map) = obj.as_object_mut() {
                map.insert("index".to_string(), Value::Number(output_index.into()));
            }
        }
        render_sse(event, &obj)
    }

    /// 反向采钥:result 块首帧补上 use 侧的键后放行。
    fn pass_result_start_with_key(
        &mut self,
        upstream_index: u64,
        event: Option<&str>,
        mut obj: Value,
        key: &str,
    ) -> Vec<u8> {
        if let Some(block) = obj.get_mut("content_block").and_then(Value::as_object_mut) {
            block.insert("tool_use_id".to_string(), Value::String(key.to_string()));
        }
        self.render_block_start(upstream_index, event, obj)
    }

    fn pass_indexed(
        &mut self,
        upstream_index: u64,
        event: Option<&str>,
        obj: Value,
        frame: &[u8],
        sep: &[u8],
    ) -> Vec<u8> {
        let output_index = match self.active_output_block {
            Some((upstream, output)) if upstream == upstream_index => output,
            _ => upstream_index.saturating_sub(self.stripped_blocks as u64),
        };
        self.rewrite_or_pass(output_index, upstream_index, event, obj, frame, sep)
    }

    fn rewrite_or_pass(
        &mut self,
        output_index: u64,
        upstream_index: u64,
        event: Option<&str>,
        mut obj: Value,
        frame: &[u8],
        sep: &[u8],
    ) -> Vec<u8> {
        if output_index == upstream_index {
            return passthrough(frame, sep);
        }
        if let Some(map) = obj.as_object_mut() {
            map.insert("index".to_string(), Value::Number(output_index.into()));
        }
        render_sse(event, &obj)
    }
}

fn block_index(obj: &Value) -> Result<u64, String> {
    obj.get("index")
        .and_then(Value::as_u64)
        .ok_or_else(|| "Kimi noise strip content block index is invalid".to_string())
}

fn split_held(bytes: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let (frame, sep_len, _) = split_frame(bytes)?;
    let sep = bytes[frame.len()..frame.len() + sep_len].to_vec();
    Some((frame, sep))
}

/// 非流式响应的同一套规则:删除噪声头 text 块与空壳搜索对,并对相邻搜索对
/// 做与流式一致的配对键采钥。
pub fn strip_nonstream_noise(response: &mut Value) -> StripStats {
    let mut stats = StripStats::default();
    let Some(content) = response.get_mut("content").and_then(Value::as_array_mut) else {
        return stats;
    };
    let mut kept: Vec<Value> = Vec::with_capacity(content.len());
    let drained = std::mem::take(content);
    let mut iter = drained.into_iter().peekable();
    while let Some(mut block) = iter.next() {
        let block_type = block.get("type").and_then(Value::as_str);
        if block_type == Some("text") {
            let is_noise = block
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| text.starts_with(NOISE_PREFIX));
            if is_noise {
                stats.noise_blocks += 1;
                stats.bytes += block
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::len)
                    .unwrap_or(0);
                continue;
            }
        }
        if block_type == Some("server_tool_use")
            && block.get("name").and_then(Value::as_str) == Some("web_search")
        {
            let next_is_result = iter.peek().is_some_and(|next| {
                next.get("type").and_then(Value::as_str) == Some("web_search_tool_result")
            });
            if next_is_result {
                let mut result = iter.next().expect("peeked above");
                let use_id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string);
                let result_key = result
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string);
                let content_empty = matches!(
                    result.get("content"),
                    Some(Value::Array(content)) if content.is_empty()
                );
                match (use_id, result_key) {
                    // 幻影对(两半都无键、内容为空):整对剥离。
                    (None, None) if content_empty => {
                        stats.pair_blocks += 2;
                        continue;
                    }
                    // 采钥:以 result 侧上游已有的键为准。
                    (use_id, Some(key)) if use_id.as_deref() != Some(key.as_str()) => {
                        if let Some(map) = block.as_object_mut() {
                            map.insert("id".to_string(), Value::String(key));
                        }
                        stats.adopted_pairs += 1;
                    }
                    // 反向采钥:use 有键、result 无键。
                    (Some(use_key), None) => {
                        if let Some(map) = result.as_object_mut() {
                            map.insert("tool_use_id".to_string(), Value::String(use_key));
                        }
                        stats.adopted_pairs += 1;
                    }
                    // 两半都无键但内容非空:不发明配对键,仅记数。
                    (None, None) => {
                        stats.unkeyed_pairs += 1;
                    }
                    // 已配对(id == key):零改写。
                    _ => {}
                }
                kept.push(block);
                kept.push(result);
                continue;
            }
        }
        kept.push(block);
    }
    *content = kept;
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sse(frames: &[Value]) -> Vec<u8> {
        let mut out = Vec::new();
        for frame in frames {
            let event = frame.get("type").and_then(Value::as_str).unwrap();
            out.extend_from_slice(&render_sse(Some(event), frame));
        }
        out
    }

    /// 模拟规范客户端:从 start 取壳、仅靠 delta 累积,还原 content 数组。
    fn reconstruct(bytes: &[u8]) -> (Vec<Value>, Option<String>) {
        let mut blocks: Vec<(u64, Value, String)> = Vec::new();
        let mut stop_reason = None;
        let mut buf = bytes.to_vec();
        while let Some((frame, _sep, rest)) = split_frame(&buf) {
            let (_event, data) = event_and_data(&frame);
            buf = rest;
            let Ok(obj) = serde_json::from_slice::<Value>(&data) else {
                continue;
            };
            match obj.get("type").and_then(Value::as_str) {
                Some("content_block_start") => {
                    let index = obj["index"].as_u64().unwrap();
                    assert_eq!(
                        index as usize,
                        blocks.len(),
                        "content block indexes must stay contiguous"
                    );
                    let mut shell = obj["content_block"].clone();
                    let seed = shell
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if shell.get("text").is_some() {
                        shell["text"] = Value::String(String::new());
                    }
                    blocks.push((index, shell, seed));
                }
                Some("content_block_delta") => {
                    let index = obj["index"].as_u64().unwrap();
                    let entry = blocks
                        .iter_mut()
                        .find(|(i, _, _)| *i == index)
                        .expect("delta must target an open block");
                    if let Some(text) = obj["delta"].get("text").and_then(Value::as_str) {
                        entry.2.push_str(text);
                    }
                }
                Some("message_delta") => {
                    if let Some(reason) = obj["delta"].get("stop_reason").and_then(Value::as_str) {
                        stop_reason = Some(reason.to_string());
                    }
                }
                _ => {}
            }
        }
        let content = blocks
            .into_iter()
            .map(|(_, mut shell, text)| {
                if shell.get("text").is_some() {
                    shell["text"] = Value::String(text);
                }
                shell
            })
            .collect();
        (content, stop_reason)
    }

    fn probe_1b_shape() -> Vec<Value> {
        vec![
            json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"k3","stop_reason":null,"usage":{"input_tokens":1,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Search results for query: Rust latest stable"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"server_tool_use","id":"tool_abc","name":"web_search"}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"Rust latest stable\"}"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_x","content":[{"type":"web_search_result","url":"https://example.test","title":"t"}]}}),
            json!({"type":"content_block_stop","index":2}),
            json!({"type":"content_block_start","index":3,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            json!({"type":"content_block_delta","index":3,"delta":{"type":"thinking_delta","thinking":"…"}}),
            json!({"type":"content_block_stop","index":3}),
            json!({"type":"content_block_start","index":4,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":4,"delta":{"type":"text_delta","text":"真正的回答"}}),
            json!({"type":"content_block_stop","index":4}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9}}),
            json!({"type":"message_stop"}),
        ]
    }

    /// 真实幻影对形态(探测 7 / 真实会话第 2 轮原始 SSE):
    /// 无 id 的 server_tool_use + 空内容的 web_search_tool_result。
    fn phantom_pair_shape() -> Vec<Value> {
        vec![
            json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","name":"web_search"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","content":[]}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"thinking_delta","thinking":"…"}}),
            json!({"type":"content_block_stop","index":2}),
            json!({"type":"content_block_start","index":3,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":3,"delta":{"type":"text_delta","text":"直接回答"}}),
            json!({"type":"content_block_stop","index":3}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
            json!({"type":"message_stop"}),
        ]
    }

    #[test]
    fn strips_the_leading_noise_header_and_compacts_indexes() {
        let mut filter = SearchNoiseFilter::new();
        let mut out = filter.feed(&sse(&probe_1b_shape())).unwrap();
        out.extend_from_slice(&filter.finalize().unwrap());
        let (content, stop) = reconstruct(&out);
        assert_eq!(stop.as_deref(), Some("end_turn"));
        let kinds: Vec<&str> = content
            .iter()
            .map(|block| block["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            [
                "server_tool_use",
                "web_search_tool_result",
                "thinking",
                "text"
            ]
        );
        assert_eq!(content[3]["text"], "真正的回答");
        // 探测 1b 的真实形态两半键恒不匹配(tool_… vs srvtoolu_…),
        // 采钥后必须以 result 侧的键配成一对,Science 才能落盘保住查询词。
        assert_eq!(content[0]["id"], "srvtoolu_x");
        assert_eq!(content[1]["tool_use_id"], "srvtoolu_x");
        assert_eq!(filter.stats().noise_blocks, 1);
        assert_eq!(filter.stats().pair_blocks, 0);
        assert_eq!(filter.stats().adopted_pairs, 1);
        assert!(filter.stats().bytes > 0);
    }

    #[test]
    fn strips_the_dangling_trailing_noise_header() {
        // 探测 4(K2.7)形态:结尾悬挂噪声头,没有答案文本。
        // 搜索对用已配对的键,让本测试只盯噪声头剥离。
        let frames = vec![
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Search results for query: q1"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"server_tool_use","id":"srvtoolu_a","name":"web_search"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_a","content":[{"type":"web_search_result","url":"https://example.test","title":"t"}]}}),
            json!({"type":"content_block_stop","index":2}),
            json!({"type":"content_block_start","index":3,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":3,"delta":{"type":"text_delta","text":"Search results for query: q2 下一轮"}}),
            json!({"type":"content_block_stop","index":3}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
            json!({"type":"message_stop"}),
        ];
        let mut filter = SearchNoiseFilter::new();
        let mut out = filter.feed(&sse(&frames)).unwrap();
        out.extend_from_slice(&filter.finalize().unwrap());
        let (content, _) = reconstruct(&out);
        let kinds: Vec<&str> = content
            .iter()
            .map(|block| block["type"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["server_tool_use", "web_search_tool_result"]);
        assert_eq!(filter.stats().noise_blocks, 2);
    }

    #[test]
    fn strips_the_phantom_empty_search_pair() {
        let mut filter = SearchNoiseFilter::new();
        let mut out = filter.feed(&sse(&phantom_pair_shape())).unwrap();
        out.extend_from_slice(&filter.finalize().unwrap());
        let (content, stop) = reconstruct(&out);
        assert_eq!(stop.as_deref(), Some("end_turn"));
        let kinds: Vec<&str> = content
            .iter()
            .map(|block| block["type"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["thinking", "text"]);
        assert_eq!(content[1]["text"], "直接回答");
        assert_eq!(filter.stats().pair_blocks, 2);
        assert_eq!(filter.stats().noise_blocks, 0);
    }

    #[test]
    fn keeps_a_matched_zero_result_search_byte_identical() {
        // 已配对(id == tool_use_id)的真实零结果搜索:零命中,字节级原样。
        let frames = vec![
            json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtoolu_same","name":"web_search"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"nothing\"}"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_same","content":[]}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"message_stop"}),
        ];
        let input = sse(&frames);
        let mut filter = SearchNoiseFilter::new();
        let mut out = filter.feed(&input).unwrap();
        out.extend_from_slice(&filter.finalize().unwrap());
        assert_eq!(out, input, "matched pairs must pass byte identical");
        assert_eq!(filter.stats().total_blocks(), 0);
        assert_eq!(filter.stats().adopted_pairs, 0);
        assert_eq!(filter.stats().unkeyed_pairs, 0);
    }

    #[test]
    fn stream_adoption_rewrites_an_idless_use_to_the_result_key() {
        // D5 采钥主路径:use 半无 id,result 半带 srvtoolu 键。
        // 期望输出可精确构造:preserve_order 下新插入的 id 追加在
        // content_block 末尾,其余帧字节不变、索引不变。
        let input_frames = vec![
            json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","name":"web_search"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"rust\"}"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_k","content":[{"type":"web_search_result","url":"https://example.test","title":"t"}]}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
            json!({"type":"message_stop"}),
        ];
        let mut expected_frames = input_frames.clone();
        expected_frames[0] = json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","name":"web_search","id":"srvtoolu_k"}});
        let mut filter = SearchNoiseFilter::new();
        let mut out = filter.feed(&sse(&input_frames)).unwrap();
        out.extend_from_slice(&filter.finalize().unwrap());
        assert_eq!(out, sse(&expected_frames));
        assert_eq!(filter.stats().adopted_pairs, 1);
        assert_eq!(filter.stats().unkeyed_pairs, 0);
        assert_eq!(filter.stats().total_blocks(), 0);
    }

    #[test]
    fn stream_adoption_backfills_a_keyless_result_from_the_use_id() {
        // 反向采钥:use 有键、result 无键 → result 补上 use 侧的键。
        let input_frames = vec![
            json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"tool_u","name":"web_search"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","content":[{"type":"web_search_result","url":"https://example.test","title":"t"}]}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"message_stop"}),
        ];
        let mut expected_frames = input_frames.clone();
        expected_frames[2] = json!({"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","content":[{"type":"web_search_result","url":"https://example.test","title":"t"}],"tool_use_id":"tool_u"}});
        let mut filter = SearchNoiseFilter::new();
        let mut out = filter.feed(&sse(&input_frames)).unwrap();
        out.extend_from_slice(&filter.finalize().unwrap());
        assert_eq!(out, sse(&expected_frames));
        assert_eq!(filter.stats().adopted_pairs, 1);
        assert_eq!(filter.stats().total_blocks(), 0);
    }

    #[test]
    fn a_keyed_empty_result_pair_is_adopted_not_stripped() {
        // 收窄回归防线:带 srvtoolu 键的空结果是真实零结果搜索。
        // 旧判据(无 use.id + 空 content,不看 result 键)会把它整对剥掉。
        let frames = vec![
            json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","name":"web_search"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_z","content":[]}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"message_stop"}),
        ];
        let mut filter = SearchNoiseFilter::new();
        let mut out = filter.feed(&sse(&frames)).unwrap();
        out.extend_from_slice(&filter.finalize().unwrap());
        let (content, _) = reconstruct(&out);
        let kinds: Vec<&str> = content
            .iter()
            .map(|block| block["type"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["server_tool_use", "web_search_tool_result"]);
        assert_eq!(content[0]["id"], "srvtoolu_z");
        assert_eq!(content[1]["tool_use_id"], "srvtoolu_z");
        assert_eq!(filter.stats().pair_blocks, 0);
        assert_eq!(filter.stats().adopted_pairs, 1);
    }

    #[test]
    fn an_unkeyed_pair_with_content_passes_byte_identical() {
        // 两半都无键但内容非空:不发明配对键,如实放行,仅记数。
        let frames = vec![
            json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","name":"web_search"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","content":[{"type":"web_search_result","url":"https://example.test","title":"t"}]}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"message_stop"}),
        ];
        let input = sse(&frames);
        let mut filter = SearchNoiseFilter::new();
        let mut out = filter.feed(&input).unwrap();
        out.extend_from_slice(&filter.finalize().unwrap());
        assert_eq!(out, input, "unkeyed pairs must not get invented keys");
        assert_eq!(filter.stats().unkeyed_pairs, 1);
        assert_eq!(filter.stats().adopted_pairs, 0);
        assert_eq!(filter.stats().total_blocks(), 0);
    }

    #[test]
    fn malformed_search_result_content_is_not_a_phantom_stream_pair() {
        // 只有现场出现过的显式 `[]` 才是幻影。旧判据把缺失或错误类型
        // 都当空数组吞掉，会把上游 schema 漂移伪装成成功。
        for (case, result_content) in [
            ("missing", None),
            ("string", Some(json!("invalid"))),
            ("object", Some(json!({"unexpected": true}))),
            ("null", Some(Value::Null)),
        ] {
            let mut result = json!({"type":"web_search_tool_result"});
            if let Some(content) = result_content {
                result["content"] = content;
            }
            let frames = vec![
                json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","name":"web_search"}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"content_block_start","index":1,"content_block":result}),
                json!({"type":"content_block_stop","index":1}),
                json!({"type":"message_stop"}),
            ];
            let input = sse(&frames);
            let mut filter = SearchNoiseFilter::new();
            let mut out = filter.feed(&input).unwrap();
            out.extend_from_slice(&filter.finalize().unwrap());
            assert_eq!(out, input, "malformed {case} content must stay visible");
            assert_eq!(filter.stats().unkeyed_pairs, 1, "{case}");
            assert_eq!(filter.stats().total_blocks(), 0, "{case}");
        }
    }

    #[test]
    fn non_web_search_server_tools_do_not_trigger_stream_pair_rewrites() {
        // 旧逻辑会采纳第一对的 result id，并把第二对误判成幻影删除。
        // `web_search_tool_result` 不能反向把任意 server tool 变成搜索调用。
        let frames = vec![
            json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"tool_compute","name":"code_execution"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srv_search","content":[{"type":"web_search_result","url":"https://example.test"}]}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"server_tool_use","name":"code_execution"}}),
            json!({"type":"content_block_stop","index":2}),
            json!({"type":"content_block_start","index":3,"content_block":{"type":"web_search_tool_result","content":[]}}),
            json!({"type":"content_block_stop","index":3}),
            json!({"type":"message_stop"}),
        ];
        let input = sse(&frames);
        let mut filter = SearchNoiseFilter::new();
        let mut out = filter.feed(&input).unwrap();
        out.extend_from_slice(&filter.finalize().unwrap());
        assert_eq!(out, input);
        assert!(!filter.stats().any_activity());
    }

    #[test]
    fn flushes_a_held_server_tool_before_non_result_blocks() {
        let frames = vec![
            json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","name":"web_search"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"平常回答"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"message_stop"}),
        ];
        let input = sse(&frames);
        let mut filter = SearchNoiseFilter::new();
        let mut out = filter.feed(&input).unwrap();
        out.extend_from_slice(&filter.finalize().unwrap());
        assert_eq!(out, input, "unmatched held pair must flush byte identical");
        assert_eq!(filter.stats().total_blocks(), 0);
    }

    #[test]
    fn composes_noise_and_phantom_pair_strips_with_contiguous_indexes() {
        let frames = vec![
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Search results for query: q"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"server_tool_use","id":"tool_a","name":"web_search"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"web_search_tool_result","tool_use_id":"srv_a","content":[{"type":"web_search_result","url":"https://example.test","title":"t"}]}}),
            json!({"type":"content_block_stop","index":2}),
            json!({"type":"content_block_start","index":3,"content_block":{"type":"server_tool_use","name":"web_search"}}),
            json!({"type":"content_block_stop","index":3}),
            json!({"type":"content_block_start","index":4,"content_block":{"type":"web_search_tool_result","content":[]}}),
            json!({"type":"content_block_stop","index":4}),
            json!({"type":"content_block_start","index":5,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":5,"delta":{"type":"text_delta","text":"答案"}}),
            json!({"type":"content_block_stop","index":5}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
            json!({"type":"message_stop"}),
        ];
        let mut filter = SearchNoiseFilter::new();
        // 按小块喂入,覆盖跨 chunk 组帧。
        let input = sse(&frames);
        let mut out = Vec::new();
        for chunk in input.chunks(11) {
            out.extend_from_slice(&filter.feed(chunk).unwrap());
        }
        out.extend_from_slice(&filter.finalize().unwrap());
        let (content, _) = reconstruct(&out);
        let kinds: Vec<&str> = content
            .iter()
            .map(|block| block["type"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["server_tool_use", "web_search_tool_result", "text"]);
        assert_eq!(content[2]["text"], "答案");
        // 采钥与剥离叠加:被保留的真实对在索引前移的同时完成配对键归一。
        assert_eq!(content[0]["id"], "srv_a");
        assert_eq!(content[1]["tool_use_id"], "srv_a");
        assert_eq!(filter.stats().noise_blocks, 1);
        assert_eq!(filter.stats().pair_blocks, 2);
        assert_eq!(filter.stats().adopted_pairs, 1);
    }

    #[test]
    fn passes_untouched_streams_byte_identical() {
        let frames = vec![
            json!({"type":"message_start","message":{"id":"msg_2","type":"message","role":"assistant","content":[],"model":"k3","stop_reason":null,"usage":{"input_tokens":1,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"…"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Search 结论:平常文本"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"python","input":{}}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
            json!({"type":"content_block_stop","index":2}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}),
            json!({"type":"message_stop"}),
        ];
        let input = sse(&frames);
        let mut filter = SearchNoiseFilter::new();
        let mut out = Vec::new();
        for chunk in input.chunks(7) {
            out.extend_from_slice(&filter.feed(chunk).unwrap());
        }
        out.extend_from_slice(&filter.finalize().unwrap());
        assert_eq!(out, input, "zero-hit streams must stay byte identical");
        assert_eq!(filter.stats().total_blocks(), 0);
    }

    #[test]
    fn holds_prefix_fragments_across_deltas_before_deciding() {
        let noise = vec![
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Search resu"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lts for query: 拆开的前缀"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"答案"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"message_stop"}),
        ];
        let mut filter = SearchNoiseFilter::new();
        let mut out = filter.feed(&sse(&noise)).unwrap();
        out.extend_from_slice(&filter.finalize().unwrap());
        let (content, _) = reconstruct(&out);
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["text"], "答案");

        // 同样的分片走向不同结局:分叉即放行,且零命中保持字节一致。
        let benign = vec![
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Search resu"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lt: 平常内容"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_stop"}),
        ];
        let input = sse(&benign);
        let mut filter = SearchNoiseFilter::new();
        let mut out = filter.feed(&input).unwrap();
        out.extend_from_slice(&filter.finalize().unwrap());
        assert_eq!(out, input);
    }

    #[test]
    fn keeps_text_that_stops_before_completing_the_prefix() {
        let frames = vec![
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Search results for"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_stop"}),
        ];
        let input = sse(&frames);
        let mut filter = SearchNoiseFilter::new();
        let mut out = filter.feed(&input).unwrap();
        out.extend_from_slice(&filter.finalize().unwrap());
        assert_eq!(out, input);
        assert_eq!(filter.stats().total_blocks(), 0);
    }

    #[test]
    fn pings_inside_a_pending_block_survive_a_strip() {
        let frames = vec![
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"ping"}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Search results for query: x"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_stop"}),
        ];
        let mut filter = SearchNoiseFilter::new();
        let mut out = filter.feed(&sse(&frames)).unwrap();
        out.extend_from_slice(&filter.finalize().unwrap());
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"ping\""), "held ping must be re-emitted");
        assert!(!text.contains("Search results for query"));
        assert_eq!(filter.stats().noise_blocks, 1);
    }

    #[test]
    fn oversized_pending_buffer_flushes_without_stripping() {
        let mut filter = SearchNoiseFilter::new();
        let start = render_sse(
            Some("content_block_start"),
            &json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Search"}}),
        );
        filter.feed(&start).unwrap();
        let ping = render_sse(Some("ping"), &json!({"type":"ping"}));
        let mut out = Vec::new();
        for _ in 0..20000 {
            out.extend_from_slice(&filter.feed(&ping).unwrap());
        }
        // 超限后放弃判定并放行全部挂起帧,不吞任何内容。
        assert!(!out.is_empty());
        assert_eq!(filter.stats().total_blocks(), 0);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("content_block_start"));
    }

    #[test]
    fn nonstream_strip_removes_noise_blocks_and_phantom_pairs_only() {
        // 搜索对用已配对的键,让本测试只盯剥离行为。
        let mut response = json!({
            "id": "msg_3",
            "content": [
                {"type": "text", "text": "Search results for query: q"},
                {"type": "server_tool_use", "id": "srvtoolu_a", "name": "web_search", "input": {}},
                {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_a", "content": [{"type": "web_search_result", "url": "https://example.test"}]},
                {"type": "server_tool_use", "name": "web_search"},
                {"type": "web_search_tool_result", "content": []},
                {"type": "text", "text": "真正的回答"},
            ],
            "stop_reason": "end_turn",
        });
        let stats = strip_nonstream_noise(&mut response);
        assert_eq!(stats.noise_blocks, 1);
        assert_eq!(stats.pair_blocks, 2);
        assert_eq!(stats.adopted_pairs, 0);
        let kinds: Vec<&str> = response["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|block| block["type"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["server_tool_use", "web_search_tool_result", "text"]);

        let mut untouched = json!({"content": [
            {"type": "server_tool_use", "id": "srvtoolu_b", "name": "web_search", "input": {}},
            {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_b", "content": []},
            {"type": "text", "text": "平常"},
        ]});
        let before = untouched.clone();
        let untouched_stats = strip_nonstream_noise(&mut untouched);
        assert_eq!(untouched_stats.total_blocks(), 0);
        assert!(!untouched_stats.any_activity());
        assert_eq!(untouched, before);
    }

    #[test]
    fn nonstream_adoption_follows_the_same_matrix() {
        let mut response = json!({
            "id": "msg_4",
            "content": [
                // 采钥:use 无 id + result 带键。
                {"type": "server_tool_use", "name": "web_search", "input": {"query": "a"}},
                {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_a", "content": [{"type": "web_search_result", "url": "https://example.test"}]},
                // 采钥:use 与 result 都有键但不匹配(真实 D5 形态)。
                {"type": "server_tool_use", "id": "tool_e", "name": "web_search", "input": {"query": "e"}},
                {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_e", "content": [{"type": "web_search_result", "url": "https://example.test"}]},
                // 反向采钥:use 有键 + result 无键。
                {"type": "server_tool_use", "id": "tool_b", "name": "web_search", "input": {"query": "b"}},
                {"type": "web_search_tool_result", "content": [{"type": "web_search_result", "url": "https://example.test"}]},
                // 带键空结果:采钥保留,不当幻影剥(收窄回归防线)。
                {"type": "server_tool_use", "name": "web_search", "input": {"query": "c"}},
                {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_c", "content": []},
                // 幻影对:两半都无键且内容为空 → 剥离。
                {"type": "server_tool_use", "name": "web_search"},
                {"type": "web_search_tool_result", "content": []},
                // 两半都无键但内容非空 → 如实放行,仅记数。
                {"type": "server_tool_use", "name": "web_search", "input": {"query": "d"}},
                {"type": "web_search_tool_result", "content": [{"type": "web_search_result", "url": "https://example.test"}]},
                {"type": "text", "text": "回答"},
            ],
            "stop_reason": "end_turn",
        });
        let stats = strip_nonstream_noise(&mut response);
        assert_eq!(stats.adopted_pairs, 4);
        assert_eq!(stats.unkeyed_pairs, 1);
        assert_eq!(stats.pair_blocks, 2);
        assert_eq!(stats.noise_blocks, 0);
        assert!(stats.any_activity());
        assert!(stats.rewrote_body());
        let content = response["content"].as_array().unwrap();
        assert_eq!(content.len(), 11);
        assert_eq!(content[0]["id"], "srvtoolu_a");
        assert_eq!(content[1]["tool_use_id"], "srvtoolu_a");
        assert_eq!(content[2]["id"], "srvtoolu_e");
        assert_eq!(content[3]["tool_use_id"], "srvtoolu_e");
        assert_eq!(content[4]["id"], "tool_b");
        assert_eq!(content[5]["tool_use_id"], "tool_b");
        assert_eq!(content[6]["id"], "srvtoolu_c");
        assert_eq!(content[7]["tool_use_id"], "srvtoolu_c");
        // 无钥对不得被发明键。
        assert!(content[8].get("id").is_none());
        assert!(content[9].get("tool_use_id").is_none());
        assert_eq!(content[10]["text"], "回答");
    }

    #[test]
    fn malformed_search_result_content_is_not_a_phantom_nonstream_pair() {
        // 与流式路径锁同一个信任边界，防止两份 pair matrix 再次漂移。
        for (case, result_content) in [
            ("missing", None),
            ("string", Some(json!("invalid"))),
            ("object", Some(json!({"unexpected": true}))),
            ("null", Some(Value::Null)),
        ] {
            let mut result = json!({"type": "web_search_tool_result"});
            if let Some(content) = result_content {
                result["content"] = content;
            }
            let mut response = json!({"content": [
                {"type": "server_tool_use", "name": "web_search"},
                result,
            ]});
            let before = response.clone();
            let stats = strip_nonstream_noise(&mut response);
            assert_eq!(
                response, before,
                "malformed {case} content must stay visible"
            );
            assert_eq!(stats.unkeyed_pairs, 1, "{case}");
            assert!(!stats.rewrote_body(), "{case}");
        }
    }

    #[test]
    fn non_web_search_server_tools_do_not_trigger_nonstream_pair_rewrites() {
        let mut response = json!({"content": [
            {"type": "server_tool_use", "id": "tool_compute", "name": "code_execution"},
            {"type": "web_search_tool_result", "tool_use_id": "srv_search", "content": [
                {"type": "web_search_result", "url": "https://example.test"}
            ]},
            {"type": "server_tool_use", "name": "code_execution"},
            {"type": "web_search_tool_result", "content": []},
        ]});
        let before = response.clone();
        let stats = strip_nonstream_noise(&mut response);
        assert_eq!(response, before);
        assert!(!stats.any_activity());
    }
}
