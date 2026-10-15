mod response;
mod stream;
#[cfg(test)]
mod tests;

use super::TranslateError;
use crate::pricing::Rates;
use serde_json::{Map, Value, json};

pub(crate) use response::from_response;
pub(crate) use stream::Stream;

pub(crate) struct Opts {
    openrouter: bool,
    include_usage: bool,
    include_reasoning: bool,
    model: String,
    pub(crate) rates: Option<Rates>,
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().concat(),
        _ => String::new(),
    }
}

fn clean_model(m: &str, openrouter: bool) -> String {
    if !openrouter {
        return m.to_owned();
    }
    let m = m.split_once('/').map_or(m, |(_, r)| r);
    m.split(':').next().unwrap_or(m).to_owned()
}

fn message(role: &str, kind: &str, text: &str) -> Value {
    json!({"type":"message","role":role,"content":[{"type":kind,"text":text}]})
}

fn image_part(p: &Value) -> Value {
    let i = &p["image_url"];
    let (url, detail) = match i {
        Value::String(_) => (i, &Value::Null),
        _ => (&i["url"], &i["detail"]),
    };
    json!({
        "type":"input_image",
        "image_url":url,
        "detail":detail.as_str().unwrap_or("auto"),
    })
}

fn user_content(c: &Value) -> Vec<Value> {
    match c {
        Value::String(s) => vec![json!({"type":"input_text","text":s})],
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| match p["type"].as_str()? {
                "text" => Some(json!({"type":"input_text","text":p["text"]})),
                "image_url" => Some(image_part(p)),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn effort_for(tokens: u64) -> &'static str {
    match tokens {
        0..=2000 => "low",
        2001..=8000 => "medium",
        _ => "high",
    }
}

fn convert_messages(messages: &[Value]) -> (String, Vec<Value>) {
    let mut instructions = String::new();
    let mut input: Vec<Value> = Vec::with_capacity(messages.len());
    for m in messages {
        match m["role"].as_str().unwrap_or("") {
            "system" | "developer" => {
                let t = text_of(&m["content"]);
                if input.is_empty() {
                    if !instructions.is_empty() {
                        instructions.push_str("\n\n");
                    }
                    instructions.push_str(&t);
                } else {
                    input.push(message("developer", "input_text", &t));
                }
            }
            "user" => input.push(json!({"type":"message","role":"user","content":user_content(&m["content"])})),
            "assistant" => {
                let t = text_of(&m["content"]);
                if !t.is_empty() {
                    input.push(message("assistant", "output_text", &t));
                }
                for c in m["tool_calls"].as_array().into_iter().flatten() {
                    input.push(json!({
                        "type":"function_call",
                        "call_id":c["id"],
                        "name":c["function"]["name"],
                        "arguments":c["function"]["arguments"],
                    }));
                }
            }
            "tool" => input.push(json!({
                "type":"function_call_output",
                "call_id":m["tool_call_id"],
                "output":text_of(&m["content"]),
            })),
            _ => {}
        }
    }
    (instructions, input)
}

fn flatten_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter(|t| t["type"] == "function")
        .map(|t| {
            let f = &t["function"];
            let mut o = Map::new();
            o.insert("type".into(), "function".into());
            for k in ["name", "description", "parameters", "strict"] {
                if let Some(v) = f.get(k) {
                    o.insert(k.into(), v.clone());
                }
            }
            Value::Object(o)
        })
        .collect()
}

fn insert_tools(out: &mut Map<String, Value>, req: &Value) {
    if let Some(tools) = req["tools"].as_array() {
        let flat = flatten_tools(tools);
        if !flat.is_empty() {
            out.insert("tools".into(), flat.into());
        }
    }
    match &req["tool_choice"] {
        Value::String(s) => {
            out.insert("tool_choice".into(), s.clone().into());
        }
        Value::Object(o) => {
            if let Some(name) = o.get("function").and_then(|f| f.get("name")) {
                out.insert("tool_choice".into(), json!({"type":"function","name":name}));
            }
        }
        _ => {}
    }
    if let Some(p) = req.get("parallel_tool_calls").filter(|p| p.is_boolean()) {
        out.insert("parallel_tool_calls".into(), p.clone());
    }
}

fn insert_format(out: &mut Map<String, Value>, rf: &Value) {
    match rf["type"].as_str() {
        Some("json_schema") => {
            let s = &rf["json_schema"];
            let mut f = Map::new();
            f.insert("type".into(), "json_schema".into());
            for k in ["name", "schema", "strict", "description"] {
                if let Some(v) = s.get(k) {
                    f.insert(k.into(), v.clone());
                }
            }
            out.insert("text".into(), json!({"format":f}));
        }
        Some("json_object") => {
            out.insert("text".into(), json!({"format":{"type":"json_object"}}));
        }
        _ => {}
    }
}

pub(crate) fn to_responses(req: &Value, openrouter: bool) -> Result<(Value, Opts), TranslateError> {
    let messages = req["messages"]
        .as_array()
        .filter(|m| !m.is_empty())
        .ok_or(TranslateError::MissingMessages)?;
    let client_model = req["model"].as_str().ok_or(TranslateError::MissingModel)?;

    let (instructions, input) = convert_messages(messages);
    let mut out = Map::new();
    out.insert("model".into(), clean_model(client_model, openrouter).into());
    out.insert("instructions".into(), instructions.into());
    out.insert("input".into(), input.into());
    insert_tools(&mut out, req);
    insert_format(&mut out, &req["response_format"]);

    let r = &req["reasoning"];
    let effort = req["reasoning_effort"]
        .as_str()
        .or_else(|| r["effort"].as_str())
        .or_else(|| r["max_tokens"].as_u64().map(effort_for));
    if let Some(e) = effort {
        out.insert("reasoning".into(), json!({"effort":e,"summary":"auto"}));
    }
    if let Some(k) = req.get("prompt_cache_key").filter(|k| k.is_string()) {
        out.insert("prompt_cache_key".into(), k.clone());
    }

    let include_reasoning = if openrouter {
        r["exclude"] != true && req["include_reasoning"] != false
    } else {
        req["include_reasoning"] == true
    };
    let opts = Opts {
        openrouter,
        include_usage: req["stream_options"]["include_usage"] == true,
        include_reasoning,
        model: client_model.to_owned(),
        rates: None,
    };
    Ok((Value::Object(out), opts))
}
