mod data;

use super::Opts;
use super::response::{finish_reason, id_for, usage_json};
use bytes::Bytes;
use serde_json::{Map, Value, json};

struct ToolSlot {
    item_id: String,
    index: usize,
    args_sent: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Fresh,
    Started,
    Done,
    Failed,
}

pub(crate) struct Stream {
    opts: Opts,
    id: String,
    created: i64,
    phase: Phase,
    has_tool: bool,
    tools: Vec<ToolSlot>,
    usage: Option<Value>,
    tail: Vec<u8>,
}

impl Stream {
    pub(crate) fn new(opts: Opts) -> Self {
        let mut s = Self {
            id: id_for("0", opts.openrouter),
            created: 0,
            phase: Phase::Fresh,
            has_tool: false,
            tools: Vec::new(),
            usage: None,
            tail: Vec::new(),
            opts,
        };
        s.set_tail();
        s
    }

    fn finished(&self) -> bool {
        matches!(self.phase, Phase::Done | Phase::Failed)
    }

    fn build(&self, delta: Value, finish: Option<&str>) -> Value {
        let mut choice = Map::with_capacity(3);
        choice.insert("index".into(), 0.into());
        choice.insert("delta".into(), delta);
        choice.insert("finish_reason".into(), finish.into());
        let mut v = json!({
            "id":self.id,
            "object":"chat.completion.chunk",
            "created":self.created,
            "model":self.opts.model,
            "choices":[Value::Object(choice)],
        });
        if self.opts.openrouter {
            v["provider"] = "OpenAI".into();
        }
        if self.opts.include_usage {
            v["usage"] = Value::Null;
        }
        v
    }

    fn push(out: &mut Vec<Bytes>, v: &Value) {
        out.push(crate::sse::frame(None, &v.to_string()));
    }

    fn delta(&self, out: &mut Vec<Bytes>, delta: Value) {
        Self::push(out, &self.build(delta, None));
    }

    fn start(&mut self, out: &mut Vec<Bytes>) {
        if self.phase == Phase::Fresh {
            self.phase = Phase::Started;
            self.delta(out, json!({"role":"assistant","content":""}));
        }
    }

    fn slot(&mut self, item_id: &str, only_if_unsent: bool) -> Option<usize> {
        let pos = self
            .tools
            .iter()
            .position(|s| s.item_id == item_id)
            .or_else(|| self.tools.len().checked_sub(1));
        let slot = pos.and_then(|p| self.tools.get_mut(p))?;
        if only_if_unsent && slot.args_sent {
            return None;
        }
        slot.args_sent = true;
        Some(slot.index)
    }

    fn args(&mut self, out: &mut Vec<Bytes>, item_id: &str, args: &str, only_if_unsent: bool) {
        let Some(index) = self.slot(item_id, only_if_unsent) else { return };
        let args = if args.is_empty() && only_if_unsent { "{}" } else { args };
        self.delta(out, json!({"tool_calls":[{"index":index,"function":{"arguments":args}}]}));
    }

    fn fail(&mut self, out: &mut Vec<Bytes>, code: &Value, message: &str) {
        self.start(out);
        if self.opts.openrouter {
            let mut v = self.build(json!({}), Some("error"));
            v["error"] = json!({"code":code,"message":message});
            Self::push(out, &v);
        } else {
            let v = json!({"error":{"message":message,"type":"api_error","code":code}});
            Self::push(out, &v);
        }
        self.phase = Phase::Failed;
    }

    fn complete(&mut self, resp: &Value, out: &mut Vec<Bytes>) {
        if self.finished() {
            return;
        }
        self.start(out);
        if resp["usage"].is_object() {
            self.usage = Some(resp["usage"].clone());
        }
        let reason = finish_reason(resp, self.has_tool);
        Self::push(out, &self.build(json!({}), Some(reason)));
        self.phase = Phase::Done;
    }

    fn tool_started(&mut self, item: &Value, out: &mut Vec<Bytes>) {
        self.start(out);
        let index = self.tools.len();
        self.tools.push(ToolSlot {
            item_id: item["id"].as_str().unwrap_or("").to_owned(),
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

    pub(crate) fn event(&mut self, ev: &Value, out: &mut Vec<Bytes>) {
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
                self.set_tail();
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
                self.tool_started(&ev["item"], out);
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

    pub(crate) fn finish(&mut self, out: &mut Vec<Bytes>) {
        if !self.finished() {
            self.fail(out, &json!(502), "upstream stream ended before completion");
        }
        if self.phase != Phase::Failed
            && (self.opts.include_usage || self.opts.openrouter)
            && let Some(u) = &self.usage
        {
            let mut v = self.build(json!({}), None);
            v["choices"] = json!([]);
            v["usage"] = usage_json(u, &self.opts);
            Self::push(out, &v);
        }
        out.push(crate::sse::frame(None, "[DONE]"));
    }
}
