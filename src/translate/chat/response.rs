use super::Opts;
use serde_json::{Map, Value, json};

pub(super) fn id_for(resp_id: &str, openrouter: bool) -> String {
    let prefix = if openrouter { "gen-" } else { "chatcmpl-" };
    format!("{prefix}{}", resp_id.strip_prefix("resp_").unwrap_or(resp_id))
}

pub(super) fn finish_reason(resp: &Value, has_tool: bool) -> &'static str {
    let tool = has_tool
        || resp["output"]
            .as_array()
            .is_some_and(|o| o.iter().any(|i| i["type"] == "function_call"));
    if tool {
        return "tool_calls";
    }
    match resp["incomplete_details"]["reason"].as_str() {
        Some("max_output_tokens") => "length",
        Some("content_filter") => "content_filter",
        _ => "stop",
    }
}

pub(super) fn usage_json(u: &Value, opts: &Opts) -> Value {
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    let prompt = n(&u["input_tokens"]);
    let completion = n(&u["output_tokens"]);
    let total = u["total_tokens"].as_u64().unwrap_or(prompt + completion);
    let mut v = json!({
        "prompt_tokens":prompt,
        "completion_tokens":completion,
        "total_tokens":total,
        "prompt_tokens_details":{"cached_tokens":n(&u["input_tokens_details"]["cached_tokens"])},
        "completion_tokens_details":{"reasoning_tokens":n(&u["output_tokens_details"]["reasoning_tokens"])},
    });
    if opts.openrouter {
        v["cost"] = opts.rates.map_or(0.0, |r| r.cost(u)).into();
        v["is_byok"] = false.into();
    }
    v
}

#[derive(Default)]
struct Collected {
    content: String,
    refusal: String,
    reasoning: String,
    calls: Vec<Value>,
}

fn collect(resp: &Value) -> Collected {
    let mut c = Collected::default();
    for item in resp["output"].as_array().into_iter().flatten() {
        match item["type"].as_str() {
            Some("message") => {
                for p in item["content"].as_array().into_iter().flatten() {
                    match p["type"].as_str() {
                        Some("output_text") => c.content.push_str(p["text"].as_str().unwrap_or("")),
                        Some("refusal") => c.refusal.push_str(p["refusal"].as_str().unwrap_or("")),
                        _ => {}
                    }
                }
            }
            Some("function_call") => {
                let args = item["arguments"].as_str().filter(|a| !a.is_empty()).unwrap_or("{}");
                c.calls.push(json!({
                    "id":item["call_id"],
                    "type":"function",
                    "function":{"name":item["name"],"arguments":args},
                }));
            }
            Some("reasoning") => {
                for s in item["summary"].as_array().into_iter().flatten() {
                    if !c.reasoning.is_empty() {
                        c.reasoning.push_str("\n\n");
                    }
                    c.reasoning.push_str(s["text"].as_str().unwrap_or(""));
                }
            }
            _ => {}
        }
    }
    c
}

pub(crate) fn from_response(resp: &Value, opts: &Opts) -> Value {
    let Collected {
        content,
        refusal,
        reasoning,
        calls,
    } = collect(resp);

    let reason = finish_reason(resp, !calls.is_empty());
    let mut msg = Map::new();
    msg.insert("role".into(), "assistant".into());
    if content.is_empty() && (!calls.is_empty() || !refusal.is_empty()) {
        msg.insert("content".into(), Value::Null);
    } else {
        msg.insert("content".into(), content.into());
    }
    if !refusal.is_empty() {
        msg.insert("refusal".into(), refusal.into());
    }
    if !calls.is_empty() {
        msg.insert("tool_calls".into(), calls.into());
    }
    if opts.include_reasoning && !reasoning.is_empty() {
        msg.insert("reasoning".into(), reasoning.into());
    }

    let mut v = json!({
        "id":id_for(resp["id"].as_str().unwrap_or("0"), opts.openrouter),
        "object":"chat.completion",
        "created":resp["created_at"].as_i64().unwrap_or(0),
        "model":opts.model,
        "choices":[{"index":0,"message":msg,"finish_reason":reason}],
    });
    if opts.openrouter {
        v["provider"] = "OpenAI".into();
    }
    if resp["usage"].is_object() {
        v["usage"] = usage_json(&resp["usage"], opts);
    }
    v
}
