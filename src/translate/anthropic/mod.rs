mod response;
mod stream;
#[cfg(test)]
mod tests;

use serde_json::{Map, Value, json};

use super::TranslateError;

pub(crate) use response::{count_tokens, error_body, from_response};
pub(crate) use stream::Stream;

pub(crate) struct Opts {
    pub(crate) model: String,
    thinking: bool,
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

fn tool_choice(tc: Option<&Value>) -> Option<Value> {
    let tc = tc?;
    match str_of(tc, "type") {
        "any" => Some(json!("required")),
        "none" => Some(json!("none")),
        "auto" => Some(json!("auto")),
        "tool" => Some(json!({"type": "function", "name": str_of(tc, "name")})),
        _ => None,
    }
}

fn convert_input(msgs: &[Value]) -> Result<Vec<Value>, TranslateError> {
    let mut input = Vec::with_capacity(msgs.len() + 2);
    for m in msgs {
        let role = str_of(m, "role");
        if role != "user" && role != "assistant" {
            return Err(TranslateError::InvalidRole(role.to_owned()));
        }
        match m.get("content") {
            Some(Value::String(s)) => {
                if !s.is_empty() {
                    let mut parts = vec![text_part(role == "assistant", s)];
                    flush(&mut input, role, &mut parts);
                }
            }
            Some(Value::Array(blocks)) => convert_blocks(&mut input, role, blocks),
            _ => return Err(TranslateError::InvalidContent),
        }
    }
    Ok(input)
}

pub(crate) fn to_responses(req: &Value, upstream_model: &str) -> Result<(Value, Opts), TranslateError> {
    let msgs = req
        .get("messages")
        .and_then(Value::as_array)
        .ok_or(TranslateError::MessagesNotArray)?;
    let input = convert_input(msgs)?;

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
        if let Some(c) = tool_choice(tc) {
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
