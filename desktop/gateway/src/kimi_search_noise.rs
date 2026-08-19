//! Kimi 搜索轮噪声头剥离(响应侧)。
//!
//! 上游对声明了原生 `web_search_20250305` 的轮次会往助手内容里注入独立的
//! text 块,内容恒为 `Search results for query: <query>`,与相邻
//! `server_tool_use.input.query` 完全重复;K2.7 还会在 turn 末尾发一个
//! 悬挂噪声头(宣布下一次搜索却直接 end_turn)。三次隔离探测三发三中,
//! 证据见任务研究记录(2026-08-19-native-probe.md)。
//!
//! 规则:整块剥离,被吞的块不占用输出索引(后续块索引前移补洞);
//! **未命中的流量保持字节级零改写**。规则 ID 记入日志与 metadata。

use serde_json::Value;

use crate::anthropic_compat::{event_and_data, passthrough, render_sse, split_frame};

pub const RULE_PROVIDER_KIMI_SEARCH_NOISE_TEXT_STRIP: &str =
    "provider.kimi.search-noise-text-strip";

/// 上游注入的噪声头前缀(实测恒定,含尾随空格前的冒号)。
pub const NOISE_PREFIX: &str = "Search results for query:";

/// 单帧上限,与旧过滤器一致的有界缓冲。
const MAX_FRAME_BYTES: usize = 1024 * 1024;
/// 判定期挂起帧总字节上限。正常判定在 `NOISE_PREFIX` 长度内完成,
/// 该上限只防御病态流(如无限 ping 夹在未定块中间)。
const MAX_PENDING_BYTES: usize = 256 * 1024;

/// 判定期挂起的一帧:是否属于待判 text 块本体(ping 等穿插帧不是)。
struct HeldFrame {
    bytes: Vec<u8>,
    belongs_to_block: bool,
}

struct PendingText {
    upstream_index: u64,
    accum: String,
    held: Vec<HeldFrame>,
    held_bytes: usize,
}

#[derive(Default)]
pub struct SearchNoiseFilter {
    buf: Vec<u8>,
    /// 已吞块数;为 0 时所有输出保持原字节。
    stripped_blocks: usize,
    stripped_bytes: usize,
    /// 下一个输出索引(吞块不占位)。
    next_output_index: u64,
    /// 当前敞开的直通块:(上游索引, 输出索引)。
    active_output_block: Option<(u64, u64)>,
    /// 正在吞的噪声块的上游索引。
    swallowing: Option<u64>,
    pending: Option<PendingText>,
}

impl SearchNoiseFilter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn stripped_blocks(&self) -> usize {
        self.stripped_blocks
    }

    pub fn stripped_bytes(&self) -> usize {
        self.stripped_bytes
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
            "content_block_start" => {
                let index = block_index(&obj)?;
                if self.pending.is_some() || self.swallowing.is_some() {
                    return Err("Kimi noise strip saw overlapping content blocks".into());
                }
                let block_type = obj
                    .get("content_block")
                    .and_then(Value::as_object)
                    .and_then(|block| block.get("type"))
                    .and_then(Value::as_str);
                if block_type == Some("text") {
                    let seed = obj
                        .get("content_block")
                        .and_then(Value::as_object)
                        .and_then(|block| block.get("text"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    self.pending = Some(PendingText {
                        upstream_index: index,
                        accum: seed,
                        held: Vec::new(),
                        held_bytes: 0,
                    });
                    self.hold_frame(frame, sep, true)?;
                    return Ok(self.decide());
                }
                Ok(self.pass_block_start(index, event.as_deref(), obj, frame, sep))
            }
            "content_block_delta" => {
                let index = block_index(&obj)?;
                if self.swallowing == Some(index) {
                    self.stripped_bytes += frame.len() + sep.len();
                    return Ok(Vec::new());
                }
                if let Some(pending) = self.pending.as_mut() {
                    if pending.upstream_index == index {
                        if let Some(text) = obj
                            .get("delta")
                            .and_then(Value::as_object)
                            .and_then(|delta| delta.get("text"))
                            .and_then(Value::as_str)
                        {
                            pending.accum.push_str(text);
                        }
                        self.hold_frame(frame, sep, true)?;
                        return Ok(self.decide());
                    }
                }
                Ok(self.pass_indexed(index, event.as_deref(), obj, frame, sep))
            }
            "content_block_stop" => {
                let index = block_index(&obj)?;
                if self.swallowing == Some(index) {
                    self.stripped_bytes += frame.len() + sep.len();
                    self.swallowing = None;
                    self.stripped_blocks += 1;
                    return Ok(Vec::new());
                }
                if self
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.upstream_index == index)
                {
                    // 块整体结束仍未凑满前缀:不是噪声,原样放行。
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
                let out = self.pass_indexed(index, event.as_deref(), obj, frame, sep);
                self.active_output_block = None;
                Ok(out)
            }
            // message_* / ping / error 等无索引帧:未定块挂起期间保持顺序,其余直通。
            _ => Ok(self.emit_or_hold_interleaved(frame, sep)),
        }
    }

    /// 挂起判定:凑满前缀长度即拍板;提前分叉立即放行。
    fn decide(&mut self) -> Vec<u8> {
        let Some(pending) = self.pending.as_ref() else {
            return Vec::new();
        };
        if pending.accum.starts_with(NOISE_PREFIX) {
            let pending = self.pending.take().expect("checked above");
            let mut out = Vec::new();
            for held in pending.held {
                if held.belongs_to_block {
                    self.stripped_bytes += held.bytes.len();
                } else {
                    out.extend_from_slice(&held.bytes);
                }
            }
            self.swallowing = Some(pending.upstream_index);
            return out;
        }
        let still_possible = pending.accum.len() < NOISE_PREFIX.len()
            && NOISE_PREFIX
                .as_bytes()
                .starts_with(pending.accum.as_bytes());
        if still_possible {
            return Vec::new();
        }
        self.flush_pending()
    }

    /// 把未定块按"非噪声"放行:块首帧走 pass_block_start 以登记索引映射。
    fn flush_pending(&mut self) -> Vec<u8> {
        let Some(pending) = self.pending.take() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut block_started = false;
        for held in pending.held {
            if !held.belongs_to_block {
                out.extend_from_slice(&held.bytes);
                continue;
            }
            if self.stripped_blocks == 0 {
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
            let Ok(obj) = serde_json::from_slice::<Value>(&data) else {
                out.extend_from_slice(&held.bytes);
                continue;
            };
            if !block_started {
                block_started = true;
                out.extend_from_slice(&self.pass_block_start(
                    pending.upstream_index,
                    event.as_deref(),
                    obj,
                    &frame,
                    &sep,
                ));
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
            // 挂起期间的穿插帧(ping 等)同样保序;失败仅在超限时发生,
            // 由调用方在下一次 feed 的错误里统一暴露。
            if self.hold_frame(frame, sep, false).is_err() {
                // 超限即刻放弃判定,按非噪声放行,不吞任何内容。
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

/// 非流式响应的同一规则:整块删除噪声头 text 块。
/// 返回 (删除块数, 删除字节数);内容数组不存在时原样不动。
pub fn strip_nonstream_noise(response: &mut Value) -> (usize, usize) {
    let Some(content) = response.get_mut("content").and_then(Value::as_array_mut) else {
        return (0, 0);
    };
    let mut blocks = 0;
    let mut bytes = 0;
    content.retain(|block| {
        let is_noise = block.get("type").and_then(Value::as_str) == Some("text")
            && block
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| text.starts_with(NOISE_PREFIX));
        if is_noise {
            blocks += 1;
            bytes += block
                .get("text")
                .and_then(Value::as_str)
                .map(str::len)
                .unwrap_or(0);
        }
        !is_noise
    });
    (blocks, bytes)
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
        assert_eq!(filter.stripped_blocks(), 1);
        assert!(filter.stripped_bytes() > 0);
    }

    #[test]
    fn strips_the_dangling_trailing_noise_header() {
        // 探测 4(K2.7)形态:结尾悬挂噪声头,没有答案文本。
        let frames = vec![
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Search results for query: q1"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"server_tool_use","id":"tool_a","name":"web_search"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"web_search_tool_result","tool_use_id":"srv_a","content":[]}}),
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
        assert_eq!(filter.stripped_blocks(), 2);
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
        // 按小块喂入,覆盖跨 chunk 组帧。
        let mut out = Vec::new();
        for chunk in input.chunks(7) {
            out.extend_from_slice(&filter.feed(chunk).unwrap());
        }
        out.extend_from_slice(&filter.finalize().unwrap());
        assert_eq!(out, input, "zero-hit streams must stay byte identical");
        assert_eq!(filter.stripped_blocks(), 0);
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
        assert_eq!(filter.stripped_blocks(), 0);
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
        assert_eq!(filter.stripped_blocks(), 1);
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
        assert_eq!(filter.stripped_blocks(), 0);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("content_block_start"));
    }

    #[test]
    fn nonstream_strip_removes_noise_blocks_only() {
        let mut response = json!({
            "id": "msg_3",
            "content": [
                {"type": "text", "text": "Search results for query: q"},
                {"type": "server_tool_use", "id": "tool_a", "name": "web_search", "input": {}},
                {"type": "web_search_tool_result", "tool_use_id": "srv_a", "content": []},
                {"type": "text", "text": "真正的回答"},
            ],
            "stop_reason": "end_turn",
        });
        let (blocks, bytes) = strip_nonstream_noise(&mut response);
        assert_eq!(blocks, 1);
        assert!(bytes > 0);
        let kinds: Vec<&str> = response["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|block| block["type"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["server_tool_use", "web_search_tool_result", "text"]);

        let mut untouched = json!({"content": [{"type": "text", "text": "平常"}]});
        let before = untouched.clone();
        assert_eq!(strip_nonstream_noise(&mut untouched), (0, 0));
        assert_eq!(untouched, before);
    }
}
