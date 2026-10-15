use bytes::Bytes;
use serde_json::{Map, Value, json};

pub struct Opts {
    pub openrouter: bool,
    pub include_usage: bool,
    pub include_reasoning: bool,
    pub model: String,
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
        return m.to_string();
    }
    let m = m.split_once('/').map_or(m, |(_, r)| r);
    m.split(':').next().unwrap_or(m).to_string()
}

fn message(role: &str, kind: &str, text: String) -> Value {
    json!({"type":"message","role":role,"content":[{"type":kind,"text":text}]})
}

fn user_content(c: &Value) -> Vec<Value> {
    match c {
        Value::String(s) => vec![json!({"type":"input_text","text":s})],
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| match p["type"].as_str()? {
                "text" => Some(json!({"type":"input_text","text":p["text"]})),
                "image_url" => {
                    let i = &p["image_url"];
                    let (url, detail) = match i {
                        Value::String(_) => (i, &Value::Null),
                        _ => (&i["url"], &i["detail"]),
                    };
                    Some(json!({
                        "type":"input_image",
                        "image_url":url,
                        "detail":detail.as_str().unwrap_or("auto"),
                    }))
                }
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

pub fn to_responses(req: &Value, openrouter: bool) -> Result<(Value, Opts), String> {
    let messages = req["messages"]
        .as_array()
        .filter(|m| !m.is_empty())
        .ok_or("messages is required and must be a non-empty array")?;
    let client_model = req["model"].as_str().ok_or("model is required")?;

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
                    input.push(message("developer", "input_text", t));
                }
            }
            "user" => input.push(json!({"type":"message","role":"user","content":user_content(&m["content"])})),
            "assistant" => {
                let t = text_of(&m["content"]);
                if !t.is_empty() {
                    input.push(message("assistant", "output_text", t));
                }
                if let Some(calls) = m["tool_calls"].as_array() {
                    for c in calls {
                        input.push(json!({
                            "type":"function_call",
                            "call_id":c["id"],
                            "name":c["function"]["name"],
                            "arguments":c["function"]["arguments"],
                        }));
                    }
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

    let mut out = Map::new();
    out.insert("model".into(), clean_model(client_model, openrouter).into());
    out.insert("instructions".into(), instructions.into());
    out.insert("input".into(), input.into());

    if let Some(tools) = req["tools"].as_array() {
        let flat: Vec<Value> = tools
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
            .collect();
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

    let rf = &req["response_format"];
    match rf["type"].as_str() {
        Some("json_schema") => {
            let s = &rf["json_schema"];
            let mut f = Map::new();
            f.insert("type".into(), "json_schema".into());
            for (to, from) in [
                ("name", "name"),
                ("schema", "schema"),
                ("strict", "strict"),
                ("description", "description"),
            ] {
                if let Some(v) = s.get(from) {
                    f.insert(to.into(), v.clone());
                }
            }
            out.insert("text".into(), json!({"format":f}));
        }
        Some("json_object") => {
            out.insert("text".into(), json!({"format":{"type":"json_object"}}));
        }
        _ => {}
    }

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
        model: client_model.to_string(),
    };
    Ok((Value::Object(out), opts))
}

fn id_for(resp_id: &str, openrouter: bool) -> String {
    let prefix = if openrouter { "gen-" } else { "chatcmpl-" };
    format!("{prefix}{}", resp_id.strip_prefix("resp_").unwrap_or(resp_id))
}

fn finish_reason(resp: &Value, has_tool: bool) -> &'static str {
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

fn usage_json(u: &Value, opts: &Opts, cost_model: &str) -> Value {
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
        v["cost"] = crate::app::pricing::cost(cost_model, u).into();
        v["is_byok"] = false.into();
    }
    v
}

struct ToolSlot {
    item_id: String,
    index: usize,
    args_sent: bool,
}

pub struct Stream {
    opts: Opts,
    id: String,
    created: i64,
    started: bool,
    done: bool,
    errored: bool,
    has_tool: bool,
    tools: Vec<ToolSlot>,
    usage: Option<Value>,
    cost_model: String,
}

impl Stream {
    pub fn new(opts: Opts) -> Self {
        Self {
            id: id_for("0", opts.openrouter),
            created: 0,
            started: false,
            done: false,
            errored: false,
            has_tool: false,
            tools: Vec::new(),
            usage: None,
            cost_model: opts.model.clone(),
            opts,
        }
    }

    fn build(&self, delta: Value, finish: Option<&str>) -> Value {
        let mut v = json!({
            "id":self.id,
            "object":"chat.completion.chunk",
            "created":self.created,
            "model":self.opts.model,
            "choices":[{"index":0,"delta":delta,"finish_reason":finish}],
        });
        if self.opts.openrouter {
            v["provider"] = "OpenAI".into();
        }
        if self.opts.include_usage {
            v["usage"] = Value::Null;
        }
        v
    }

    fn push(&self, out: &mut Vec<Bytes>, v: &Value) {
        out.push(crate::sse::frame(None, &v.to_string()));
    }

    fn delta(&self, out: &mut Vec<Bytes>, delta: Value) {
        self.push(out, &self.build(delta, None));
    }

    fn start(&mut self, out: &mut Vec<Bytes>) {
        if !self.started {
            self.started = true;
            self.delta(out, json!({"role":"assistant","content":""}));
        }
    }

    fn args(&mut self, out: &mut Vec<Bytes>, item_id: &str, args: &str, only_if_unsent: bool) {
        let slot = match self.tools.iter_mut().find(|s| s.item_id == item_id) {
            Some(s) => s,
            None => match self.tools.last_mut() {
                Some(s) => s,
                None => return,
            },
        };
        if only_if_unsent && slot.args_sent {
            return;
        }
        slot.args_sent = true;
        let index = slot.index;
        let args = if args.is_empty() && only_if_unsent { "{}" } else { args };
        self.delta(out, json!({"tool_calls":[{"index":index,"function":{"arguments":args}}]}));
    }

    fn fail(&mut self, out: &mut Vec<Bytes>, code: &Value, message: &str) {
        self.start(out);
        if self.opts.openrouter {
            let mut v = self.build(json!({}), Some("error"));
            v["error"] = json!({"code":code,"message":message});
            self.push(out, &v);
        } else {
            let v = json!({"error":{"message":message,"type":"api_error","code":code}});
            self.push(out, &v);
        }
        self.done = true;
        self.errored = true;
    }

    fn complete(&mut self, resp: &Value, out: &mut Vec<Bytes>) {
        if self.done {
            return;
        }
        self.start(out);
        if resp["usage"].is_object() {
            self.usage = Some(resp["usage"].clone());
        }
        if let Some(m) = resp["model"].as_str() {
            self.cost_model = m.to_string();
        }
        let reason = finish_reason(resp, self.has_tool);
        self.push(out, &self.build(json!({}), Some(reason)));
        self.done = true;
    }

    pub fn event(&mut self, ev: &Value, out: &mut Vec<Bytes>) {
        let Some(t) = ev["type"].as_str() else { return };
        match t {
            "response.created" => {
                let r = &ev["response"];
                if let Some(id) = r["id"].as_str() {
                    self.id = id_for(id, self.opts.openrouter);
                }
                if let Some(c) = r["created_at"].as_i64() {
                    self.created = c;
                }
                self.start(out);
            }
            "response.output_text.delta" => {
                self.start(out);
                self.delta(out, json!({"content":ev["delta"]}));
            }
            "response.refusal.delta" => {
                self.start(out);
                self.delta(out, json!({"refusal":ev["delta"]}));
            }
            "response.reasoning_summary_text.delta" if self.opts.include_reasoning => {
                self.start(out);
                self.delta(out, json!({"reasoning":ev["delta"]}));
            }
            "response.reasoning_summary_part.added" if self.opts.include_reasoning && ev["summary_index"].as_u64() > Some(0) => {
                self.start(out);
                self.delta(out, json!({"reasoning":"\n\n"}));
            }
            "response.output_item.added" if ev["item"]["type"] == "function_call" => {
                self.start(out);
                let item = &ev["item"];
                let index = self.tools.len();
                self.tools.push(ToolSlot {
                    item_id: item["id"].as_str().unwrap_or("").to_string(),
                    index,
                    args_sent: false,
                });
                self.has_tool = true;
                self.delta(
                    out,
                    json!({"tool_calls":[{
                        "index":index,
                        "id":item["call_id"],
                        "type":"function",
                        "function":{"name":item["name"],"arguments":""},
                    }]}),
                );
            }
            "response.function_call_arguments.delta" => {
                let id = ev["item_id"].as_str().unwrap_or("");
                self.args(out, id, ev["delta"].as_str().unwrap_or(""), false);
            }
            "response.function_call_arguments.done" => {
                let id = ev["item_id"].as_str().unwrap_or("");
                self.args(out, id, ev["arguments"].as_str().unwrap_or(""), true);
            }
            "response.output_item.done" if ev["item"]["type"] == "function_call" => {
                let item = &ev["item"];
                let id = item["id"].as_str().unwrap_or("");
                self.args(out, id, item["arguments"].as_str().unwrap_or(""), true);
            }
            "response.completed" | "response.incomplete" => self.complete(&ev["response"], out),
            "response.failed" => {
                let e = &ev["response"]["error"];
                self.fail(out, &e["code"], e["message"].as_str().unwrap_or("upstream error"));
            }
            "error" => {
                let msg = ev["message"].as_str().unwrap_or("upstream error");
                self.fail(out, &ev["code"], msg);
            }
            _ => {}
        }
    }

    pub fn finish(&mut self, out: &mut Vec<Bytes>) {
        if !self.done {
            self.fail(out, &json!(502), "upstream stream ended before completion");
        }
        if !self.errored
            && (self.opts.include_usage || self.opts.openrouter)
            && let Some(u) = &self.usage
        {
            let mut v = self.build(json!({}), None);
            v["choices"] = json!([]);
            v["usage"] = usage_json(u, &self.opts, &self.cost_model);
            self.push(out, &v);
        }
        out.push(crate::sse::frame(None, "[DONE]"));
    }
}

pub fn from_response(resp: &Value, opts: &Opts) -> Value {
    let mut content = String::new();
    let mut refusal = String::new();
    let mut reasoning = String::new();
    let mut calls = Vec::new();
    for item in resp["output"].as_array().into_iter().flatten() {
        match item["type"].as_str() {
            Some("message") => {
                for p in item["content"].as_array().into_iter().flatten() {
                    match p["type"].as_str() {
                        Some("output_text") => content.push_str(p["text"].as_str().unwrap_or("")),
                        Some("refusal") => refusal.push_str(p["refusal"].as_str().unwrap_or("")),
                        _ => {}
                    }
                }
            }
            Some("function_call") => {
                let args = item["arguments"].as_str().filter(|a| !a.is_empty()).unwrap_or("{}");
                calls.push(json!({
                    "id":item["call_id"],
                    "type":"function",
                    "function":{"name":item["name"],"arguments":args},
                }));
            }
            Some("reasoning") => {
                for s in item["summary"].as_array().into_iter().flatten() {
                    if !reasoning.is_empty() {
                        reasoning.push_str("\n\n");
                    }
                    reasoning.push_str(s["text"].as_str().unwrap_or(""));
                }
            }
            _ => {}
        }
    }

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
        let m = resp["model"].as_str().unwrap_or(&opts.model);
        v["usage"] = usage_json(&resp["usage"], opts, m);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_maps_to_responses() {
        let req = json!({
            "model":"openai/gpt-5:online",
            "messages":[
                {"role":"system","content":"be brief"},
                {"role":"user","content":"weather?"},
                {"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"wx","arguments":"{\"c\":\"SF\"}"}}]},
                {"role":"tool","tool_call_id":"call_1","content":"sunny"}
            ],
            "tools":[{"type":"function","function":{"name":"wx","description":"d","parameters":{"type":"object"}}}],
            "tool_choice":{"type":"function","function":{"name":"wx"}},
            "reasoning_effort":"low"
        });
        let (r, opts) = to_responses(&req, true).unwrap();
        assert_eq!(r["model"], "gpt-5");
        assert_eq!(r["instructions"], "be brief");
        assert_eq!(r["input"][0]["content"][0], json!({"type":"input_text","text":"weather?"}));
        assert_eq!(
            r["input"][1],
            json!({"type":"function_call","call_id":"call_1","name":"wx","arguments":"{\"c\":\"SF\"}"})
        );
        assert_eq!(
            r["input"][2],
            json!({"type":"function_call_output","call_id":"call_1","output":"sunny"})
        );
        assert_eq!(r["input"].as_array().unwrap().len(), 3);
        assert_eq!(
            r["tools"][0],
            json!({"type":"function","name":"wx","description":"d","parameters":{"type":"object"}})
        );
        assert_eq!(r["tool_choice"], json!({"type":"function","name":"wx"}));
        assert_eq!(r["reasoning"], json!({"effort":"low","summary":"auto"}));
        assert_eq!(opts.model, "openai/gpt-5:online");
        assert!(opts.include_reasoning);
    }

    #[test]
    fn stream_text_then_tool_call() {
        let mut s = Stream::new(Opts {
            openrouter: false,
            include_usage: true,
            include_reasoning: false,
            model: "gpt-5".into(),
        });
        let mut out = Vec::new();
        let evs = [
            json!({"type":"response.created","response":{"id":"resp_abc","created_at":1700000000}}),
            json!({"type":"response.output_text.delta","delta":"Hi"}),
            json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_9","name":"wx"}}),
            json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{}"}),
            json!({"type":"response.completed","response":{"output":[{"type":"function_call"}],"usage":{"input_tokens":5,"output_tokens":7,"total_tokens":12,"input_tokens_details":{"cached_tokens":2},"output_tokens_details":{"reasoning_tokens":3}}}}),
        ];
        for e in &evs {
            s.event(e, &mut out);
        }
        s.finish(&mut out);
        let frames: Vec<String> = out.iter().map(|b| String::from_utf8(b.to_vec()).unwrap()).collect();
        let parse = |i: usize| -> Value { serde_json::from_str(frames[i].strip_prefix("data: ").unwrap().trim()).unwrap() };
        assert_eq!(parse(0)["id"], "chatcmpl-abc");
        assert_eq!(parse(0)["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(parse(1)["choices"][0]["delta"]["content"], "Hi");
        let start = parse(2);
        assert_eq!(start["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
        assert_eq!(start["choices"][0]["delta"]["tool_calls"][0]["id"], "call_9");
        assert_eq!(parse(3)["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"], "{}");
        assert_eq!(parse(4)["choices"][0]["finish_reason"], "tool_calls");
        let u = parse(5);
        assert_eq!(u["choices"], json!([]));
        assert_eq!(
            u["usage"],
            json!({"prompt_tokens":5,"completion_tokens":7,"total_tokens":12,
                "prompt_tokens_details":{"cached_tokens":2},
                "completion_tokens_details":{"reasoning_tokens":3}})
        );
        assert_eq!(frames[6], "data: [DONE]\n\n");
        assert_eq!(frames.len(), 7);
    }
}
