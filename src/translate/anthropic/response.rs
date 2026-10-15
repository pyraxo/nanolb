use serde_json::{Value, json};

use super::{Opts, str_of};

pub(super) fn msg_id(resp_id: &str) -> String {
    format!("msg_{}", resp_id.strip_prefix("resp_").unwrap_or(resp_id))
}

pub(super) fn usage(resp: &Value) -> Value {
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

pub(super) fn scan(resp: &Value) -> (bool, bool) {
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

pub(super) fn stop_reason(resp: &Value, tool: bool, refusal: bool) -> &'static str {
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

pub(super) fn error_kind(code: &str) -> &'static str {
    if code.contains("rate_limit") {
        "rate_limit_error"
    } else if code.contains("overloaded") || code.contains("slow_down") {
        "overloaded_error"
    } else {
        "api_error"
    }
}

fn thinking_block(item: &Value) -> Value {
    let mut text = String::new();
    for s in item["summary"].as_array().into_iter().flatten() {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(str_of(s, "text"));
    }
    json!({
        "type": "thinking",
        "thinking": text,
        "signature": str_of(item, "encrypted_content"),
    })
}

pub(crate) fn from_response(resp: &Value, opts: &Opts) -> Value {
    let mut content = Vec::new();
    for item in resp["output"].as_array().into_iter().flatten() {
        match str_of(item, "type") {
            "reasoning" if opts.thinking => content.push(thinking_block(item)),
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

pub(crate) fn count_tokens(req: &Value) -> Value {
    let mut c = Counter(0);
    for k in ["system", "messages", "tools"] {
        if let Some(v) = req.get(k) {
            let _ = serde_json::to_writer(&mut c, v);
        }
    }
    json!({"input_tokens": c.0 / 4})
}

pub(crate) fn error_body(status: u16, msg: &str) -> Value {
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
