use bytes::Bytes;
use serde_json::{Map, Value, json};

use crate::sse::frame;

pub struct Opts {
    pub model: String,
    pub thinking: bool,
}

fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn text_part(assistant: bool, text: &str) -> Value {
    let kind = if assistant { "output_text" } else { "input_text" };
    json!({"type": kind, "text": text})
}

fn flush(input: &mut Vec<Value>, role: &str, parts: &mut Vec<Value>) {
    if !parts.is_empty() {
        input.push(json!({"type": "message", "role": role, "content": std::mem::take(parts)}));
    }
}

fn flatten_tool_result(b: &Value) -> String {
    let mut out = String::new();
    if matches!(b.get("is_error"), Some(Value::Bool(true))) {
        out.push_str("Error: ");
    }
    match b.get("content") {
        Some(Value::String(s)) => out.push_str(s),
        Some(Value::Array(blocks)) => {
            let mut first = true;
            for blk in blocks {
                if str_of(blk, "type") == "text" {
                    if !first {
                        out.push('\n');
                    }
                    first = false;
                    out.push_str(str_of(blk, "text"));
                }
            }
        }
        _ => {}
    }
    out
}

fn image_url(b: &Value) -> Option<String> {
    let src = b.get("source")?;
    match str_of(src, "type") {
        "base64" => Some(format!("data:{};base64,{}", str_of(src, "media_type"), str_of(src, "data"))),
        "url" => Some(str_of(src, "url").to_owned()),
        _ => None,
    }
}

fn convert_blocks(input: &mut Vec<Value>, role: &str, blocks: &[Value]) {
    let assistant = role == "assistant";
    let mut parts = Vec::new();
    for b in blocks {
        match str_of(b, "type") {
            "text" => {
                let t = str_of(b, "text");
                if !t.is_empty() {
                    parts.push(text_part(assistant, t));
                }
            }
            "image" if !assistant => {
                if let Some(url) = image_url(b) {
                    parts.push(json!({"type": "input_image", "image_url": url, "detail": "auto"}));
                }
            }
            "tool_use" if assistant => {
                flush(input, role, &mut parts);
                let args = match b.get("input") {
                    Some(v) if !v.is_null() => v.to_string(),
                    _ => "{}".to_owned(),
                };
                input.push(json!({
                    "type": "function_call",
                    "call_id": str_of(b, "id"),
                    "name": str_of(b, "name"),
                    "arguments": args,
                }));
            }
            "tool_result" if !assistant => {
                flush(input, role, &mut parts);
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": str_of(b, "tool_use_id"),
                    "output": flatten_tool_result(b),
                }));
            }
            _ => {}
        }
    }
    flush(input, role, &mut parts);
}

fn instructions(req: &Value) -> String {
    match req.get("system") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => {
            let mut out = String::new();
            for b in blocks {
                let t = str_of(b, "text");
                if t.is_empty() {
                    continue;
                }
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                out.push_str(t);
            }
            out
        }
        _ => String::new(),
    }
}

fn convert_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter(|t| t.get("input_schema").is_some())
        .map(|t| {
            let mut m = Map::new();
            m.insert("type".into(), "function".into());
            m.insert("name".into(), str_of(t, "name").into());
            if let Some(d) = t.get("description") {
                m.insert("description".into(), d.clone());
            }
            m.insert("parameters".into(), t["input_schema"].clone());
            m.insert("strict".into(), false.into());
            Value::Object(m)
        })
        .collect()
}

fn effort(budget: Option<u64>) -> &'static str {
    match budget {
        Some(b) if b < 2048 => "low",
        Some(b) if b < 8192 => "medium",
        Some(_) => "high",
        None => "medium",
    }
}

pub fn to_responses(req: &Value, upstream_model: &str) -> Result<(Value, Opts), String> {
    let msgs = req.get("messages").and_then(Value::as_array).ok_or("messages must be an array")?;
    let mut input = Vec::with_capacity(msgs.len() + 2);
    for m in msgs {
        let role = str_of(m, "role");
        if role != "user" && role != "assistant" {
            return Err(format!("invalid message role: {role:?}"));
        }
        match m.get("content") {
            Some(Value::String(s)) => {
                if !s.is_empty() {
                    let mut parts = vec![text_part(role == "assistant", s)];
                    flush(&mut input, role, &mut parts);
                }
            }
            Some(Value::Array(blocks)) => convert_blocks(&mut input, role, blocks),
            _ => return Err("message content must be a string or an array".into()),
        }
    }

    let mut out = Map::new();
    out.insert("model".into(), upstream_model.into());
    out.insert("instructions".into(), instructions(req).into());
    out.insert("input".into(), Value::Array(input));

    let tools = req
        .get("tools")
        .and_then(Value::as_array)
        .map(|t| convert_tools(t))
        .unwrap_or_default();
    if !tools.is_empty() {
        out.insert("tools".into(), Value::Array(tools));
        let tc = req.get("tool_choice");
        let choice = match tc.map(|c| str_of(c, "type")) {
            Some("any") => Some(json!("required")),
            Some("none") => Some(json!("none")),
            Some("auto") => Some(json!("auto")),
            Some("tool") => Some(json!({"type": "function", "name": str_of(tc.unwrap(), "name")})),
            _ => None,
        };
        if let Some(c) = choice {
            out.insert("tool_choice".into(), c);
        }
        let parallel = !matches!(tc.and_then(|c| c.get("disable_parallel_tool_use")), Some(Value::Bool(true)));
        out.insert("parallel_tool_calls".into(), parallel.into());
    }

    let mut thinking = false;
    if let Some(t) = req.get("thinking") {
        let kind = str_of(t, "type");
        if kind == "enabled" || kind == "adaptive" {
            thinking = true;
            let e = effort(t.get("budget_tokens").and_then(Value::as_u64));
            out.insert("reasoning".into(), json!({"effort": e, "summary": "auto"}));
        }
    }

    let opts = Opts {
        model: str_of(req, "model").to_owned(),
        thinking,
    };
    Ok((Value::Object(out), opts))
}

fn msg_id(resp_id: &str) -> String {
    format!("msg_{}", resp_id.strip_prefix("resp_").unwrap_or(resp_id))
}

fn usage(resp: &Value) -> Value {
    let u = &resp["usage"];
    let input = u["input_tokens"].as_u64().unwrap_or(0);
    let cached = u["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
    json!({
        "input_tokens": input.saturating_sub(cached),
        "output_tokens": u["output_tokens"].as_u64().unwrap_or(0),
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": cached,
    })
}

fn scan(resp: &Value) -> (bool, bool) {
    let (mut tool, mut refusal) = (false, false);
    for item in resp["output"].as_array().into_iter().flatten() {
        match str_of(item, "type") {
            "function_call" => tool = true,
            "message" => {
                refusal |= item["content"]
                    .as_array()
                    .is_some_and(|c| c.iter().any(|p| str_of(p, "type") == "refusal"));
            }
            _ => {}
        }
    }
    (tool, refusal)
}

fn stop_reason(resp: &Value, tool: bool, refusal: bool) -> &'static str {
    if tool {
        return "tool_use";
    }
    match str_of(&resp["incomplete_details"], "reason") {
        "max_output_tokens" => "max_tokens",
        "content_filter" => "refusal",
        _ if refusal => "refusal",
        _ => "end_turn",
    }
}

fn error_kind(code: &str) -> &'static str {
    if code.contains("rate_limit") {
        "rate_limit_error"
    } else if code.contains("overloaded") || code.contains("slow_down") {
        "overloaded_error"
    } else {
        "api_error"
    }
}

fn send(out: &mut Vec<Bytes>, event: &str, v: Value) {
    out.push(frame(Some(event), &v.to_string()));
}

#[derive(Clone, Copy, PartialEq)]
enum Open {
    None,
    Text,
    Thinking,
    Tool,
}

pub struct Stream {
    opts: Opts,
    started: bool,
    done: bool,
    open: Open,
    index: u32,
    has_tool: bool,
    refused: bool,
    args_seen: bool,
}

impl Stream {
    pub fn new(opts: Opts) -> Self {
        Self {
            opts,
            started: false,
            done: false,
            open: Open::None,
            index: 0,
            has_tool: false,
            refused: false,
            args_seen: false,
        }
    }

    fn start(&mut self, resp_id: &str, out: &mut Vec<Bytes>) {
        if self.started {
            return;
        }
        self.started = true;
        send(
            out,
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": msg_id(resp_id),
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": self.opts.model,
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {
                        "input_tokens": 0,
                        "output_tokens": 0,
                        "cache_creation_input_tokens": 0,
                        "cache_read_input_tokens": 0,
                    },
                },
            }),
        );
        send(out, "ping", json!({"type": "ping"}));
    }

    fn close(&mut self, out: &mut Vec<Bytes>) {
        if self.open != Open::None {
            send(
                out,
                "content_block_stop",
                json!({"type": "content_block_stop", "index": self.index}),
            );
            self.index += 1;
            self.open = Open::None;
        }
    }

    fn open_block(&mut self, kind: Open, block: Value, out: &mut Vec<Bytes>) {
        self.close(out);
        send(
            out,
            "content_block_start",
            json!({"type": "content_block_start", "index": self.index, "content_block": block}),
        );
        self.open = kind;
    }

    fn open_text(&mut self, out: &mut Vec<Bytes>) {
        self.open_block(Open::Text, json!({"type": "text", "text": ""}), out);
    }

    fn delta(&mut self, delta: Value, out: &mut Vec<Bytes>) {
        send(
            out,
            "content_block_delta",
            json!({"type": "content_block_delta", "index": self.index, "delta": delta}),
        );
    }

    fn text_delta(&mut self, text: &str, out: &mut Vec<Bytes>) {
        if text.is_empty() {
            return;
        }
        if self.open != Open::Text {
            self.open_text(out);
        }
        self.delta(json!({"type": "text_delta", "text": text}), out);
    }

    fn fail(&mut self, code: &str, message: &str, out: &mut Vec<Bytes>) {
        self.done = true;
        send(
            out,
            "error",
            json!({"type": "error", "error": {"type": error_kind(code), "message": message}}),
        );
    }

    fn complete(&mut self, resp: &Value, out: &mut Vec<Bytes>) {
        self.start(str_of(resp, "id"), out);
        self.close(out);
        let (tool, refusal) = scan(resp);
        let reason = stop_reason(resp, tool || self.has_tool, refusal || self.refused);
        send(
            out,
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": reason, "stop_sequence": null},
                "usage": usage(resp),
            }),
        );
        send(out, "message_stop", json!({"type": "message_stop"}));
        self.done = true;
    }

    pub fn event(&mut self, ev: &Value, out: &mut Vec<Bytes>) {
        if self.done {
            return;
        }
        let t = str_of(ev, "type");
        match t {
            "response.created" => self.start(str_of(&ev["response"], "id"), out),
            "response.output_item.added" => {
                self.start("", out);
                let item = &ev["item"];
                match str_of(item, "type") {
                    "reasoning" if self.opts.thinking => {
                        self.open_block(Open::Thinking, json!({"type": "thinking", "thinking": ""}), out);
                    }
                    "function_call" => {
                        self.has_tool = true;
                        self.args_seen = false;
                        self.open_block(
                            Open::Tool,
                            json!({
                                "type": "tool_use",
                                "id": str_of(item, "call_id"),
                                "name": str_of(item, "name"),
                                "input": {},
                            }),
                            out,
                        );
                    }
                    _ => {}
                }
            }
            "response.reasoning_summary_part.added" => {
                if self.open == Open::Thinking && ev["summary_index"].as_u64().unwrap_or(0) > 0 {
                    self.delta(json!({"type": "thinking_delta", "thinking": "\n\n"}), out);
                }
            }
            "response.reasoning_summary_text.delta" => {
                if self.open == Open::Thinking {
                    self.delta(json!({"type": "thinking_delta", "thinking": str_of(ev, "delta")}), out);
                }
            }
            "response.content_part.added" => {
                self.start("", out);
                let kind = str_of(&ev["part"], "type");
                if kind == "output_text" || kind == "refusal" {
                    self.open_text(out);
                }
            }
            "response.output_text.delta" => {
                self.start("", out);
                self.text_delta(str_of(ev, "delta"), out);
            }
            "response.refusal.delta" => {
                self.start("", out);
                self.refused = true;
                self.text_delta(str_of(ev, "delta"), out);
            }
            "response.content_part.done" | "response.output_text.done" | "response.refusal.done" => {
                if self.open == Open::Text {
                    self.close(out);
                }
            }
            "response.function_call_arguments.delta" => {
                if self.open == Open::Tool {
                    let d = str_of(ev, "delta");
                    if !d.is_empty() {
                        self.args_seen = true;
                        self.delta(json!({"type": "input_json_delta", "partial_json": d}), out);
                    }
                }
            }
            "response.output_item.done" => {
                let item = &ev["item"];
                match (str_of(item, "type"), self.open) {
                    ("reasoning", Open::Thinking) => {
                        self.delta(
                            json!({"type": "signature_delta", "signature": str_of(item, "encrypted_content")}),
                            out,
                        );
                        self.close(out);
                    }
                    ("function_call", Open::Tool) => {
                        let args = str_of(item, "arguments");
                        if !self.args_seen && !args.is_empty() {
                            self.delta(json!({"type": "input_json_delta", "partial_json": args}), out);
                        }
                        self.close(out);
                    }
                    ("message", Open::Text) => self.close(out),
                    _ => {}
                }
            }
            "response.completed" | "response.incomplete" => self.complete(&ev["response"], out),
            "response.failed" => {
                let e = &ev["response"]["error"];
                self.fail(str_of(e, "code"), str_of(e, "message"), out);
            }
            "error" => {
                let e = if ev.get("error").is_some() { &ev["error"] } else { ev };
                self.fail(str_of(e, "code"), str_of(e, "message"), out);
            }
            _ => {}
        }
    }

    pub fn finish(&mut self, out: &mut Vec<Bytes>) {
        if self.done {
            return;
        }
        self.close(out);
        self.fail("", "upstream stream ended before completion", out);
    }
}

pub fn from_response(resp: &Value, opts: &Opts) -> Value {
    let mut content = Vec::new();
    for item in resp["output"].as_array().into_iter().flatten() {
        match str_of(item, "type") {
            "reasoning" if opts.thinking => {
                let mut text = String::new();
                for s in item["summary"].as_array().into_iter().flatten() {
                    if !text.is_empty() {
                        text.push_str("\n\n");
                    }
                    text.push_str(str_of(s, "text"));
                }
                content.push(json!({
                    "type": "thinking",
                    "thinking": text,
                    "signature": str_of(item, "encrypted_content"),
                }));
            }
            "message" => {
                for p in item["content"].as_array().into_iter().flatten() {
                    let text = match str_of(p, "type") {
                        "output_text" => str_of(p, "text"),
                        "refusal" => str_of(p, "refusal"),
                        _ => continue,
                    };
                    content.push(json!({"type": "text", "text": text}));
                }
            }
            "function_call" => {
                let input = serde_json::from_str::<Value>(str_of(item, "arguments"))
                    .ok()
                    .filter(Value::is_object)
                    .unwrap_or_else(|| json!({}));
                content.push(json!({
                    "type": "tool_use",
                    "id": str_of(item, "call_id"),
                    "name": str_of(item, "name"),
                    "input": input,
                }));
            }
            _ => {}
        }
    }
    let (tool, refusal) = scan(resp);
    json!({
        "id": msg_id(str_of(resp, "id")),
        "type": "message",
        "role": "assistant",
        "model": opts.model,
        "content": content,
        "stop_reason": stop_reason(resp, tool, refusal),
        "stop_sequence": null,
        "usage": usage(resp),
    })
}

struct Counter(usize);

impl std::io::Write for Counter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn count_tokens(req: &Value) -> Value {
    let mut c = Counter(0);
    for k in ["system", "messages", "tools"] {
        if let Some(v) = req.get(k) {
            let _ = serde_json::to_writer(&mut c, v);
        }
    }
    json!({"input_tokens": c.0 / 4})
}

pub fn error_body(status: u16, msg: &str) -> Value {
    let kind = match status {
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        529 => "overloaded_error",
        400..=499 => "invalid_request_error",
        _ => "api_error",
    };
    json!({"type": "error", "error": {"type": kind, "message": msg}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_maps_to_responses() {
        let req = json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 100,
            "system": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}],
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "ok"},
                    {"type": "tool_use", "id": "toolu_1", "name": "f", "input": {"x": 1}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "is_error": true,
                     "content": [{"type": "text", "text": "bad"}]}
                ]}
            ],
            "tools": [{"name": "f", "description": "d", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "any"},
            "thinking": {"type": "enabled", "budget_tokens": 4000}
        });
        let (r, opts) = to_responses(&req, "gpt-5").unwrap();
        assert_eq!(opts.model, "claude-sonnet-4-5");
        assert!(opts.thinking);
        assert_eq!(
            r,
            json!({
                "model": "gpt-5",
                "instructions": "a\n\nb",
                "input": [
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                    {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "ok"}]},
                    {"type": "function_call", "call_id": "toolu_1", "name": "f", "arguments": "{\"x\":1}"},
                    {"type": "function_call_output", "call_id": "toolu_1", "output": "Error: bad"}
                ],
                "tools": [{"type": "function", "name": "f", "description": "d",
                           "parameters": {"type": "object"}, "strict": false}],
                "tool_choice": "required",
                "parallel_tool_calls": true,
                "reasoning": {"effort": "medium", "summary": "auto"}
            })
        );
    }

    #[test]
    fn stream_text_then_tool_call() {
        let mut s = Stream::new(Opts {
            model: "claude-x".into(),
            thinking: false,
        });
        let evs = [
            json!({"type": "response.created", "response": {"id": "resp_abc"}}),
            json!({"type": "response.output_item.added", "item": {"type": "message"}}),
            json!({"type": "response.content_part.added", "part": {"type": "output_text"}}),
            json!({"type": "response.output_text.delta", "delta": "Hi"}),
            json!({"type": "response.output_text.done", "text": "Hi"}),
            json!({"type": "response.output_item.added",
                   "item": {"type": "function_call", "call_id": "call_1", "name": "f"}}),
            json!({"type": "response.function_call_arguments.delta", "delta": "{}"}),
            json!({"type": "response.output_item.done",
                   "item": {"type": "function_call", "call_id": "call_1", "arguments": "{}"}}),
            json!({"type": "response.completed", "response": {"id": "resp_abc", "output": [
                {"type": "function_call"}],
                "usage": {"input_tokens": 10, "output_tokens": 5,
                          "input_tokens_details": {"cached_tokens": 4}}}}),
        ];
        let mut out = Vec::new();
        for e in &evs {
            s.event(e, &mut out);
        }
        s.finish(&mut out);
        let names: Vec<String> = out
            .iter()
            .map(|b| {
                let t = std::str::from_utf8(b).unwrap();
                t.lines().next().unwrap().trim_start_matches("event: ").to_owned()
            })
            .collect();
        assert_eq!(
            names,
            [
                "message_start",
                "ping",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        let md = std::str::from_utf8(&out[8]).unwrap();
        assert!(md.contains("\"stop_reason\":\"tool_use\""));
        assert!(md.contains("\"input_tokens\":6"));
        assert!(md.contains("\"cache_read_input_tokens\":4"));
    }
}
