use bytes::Bytes;
use serde_json::{Value, json};

use super::response::{error_kind, msg_id, scan, stop_reason, usage};
use super::{Opts, str_of};
use crate::sse::frame;

fn send(out: &mut Vec<Bytes>, event: &str, v: &Value) {
    out.push(frame(Some(event), &v.to_string()));
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Open {
    None,
    Text,
    Thinking,
    Tool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Fresh,
    Started,
    Done,
}

pub(crate) struct Stream {
    opts: Opts,
    phase: Phase,
    open: Open,
    index: u32,
    has_tool: bool,
    refused: bool,
    args_seen: bool,
}

impl Stream {
    pub(crate) fn new(opts: Opts) -> Self {
        Self {
            opts,
            phase: Phase::Fresh,
            open: Open::None,
            index: 0,
            has_tool: false,
            refused: false,
            args_seen: false,
        }
    }

    fn start(&mut self, resp_id: &str, out: &mut Vec<Bytes>) {
        if self.phase != Phase::Fresh {
            return;
        }
        self.phase = Phase::Started;
        send(
            out,
            "message_start",
            &json!({
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
        send(out, "ping", &json!({"type": "ping"}));
    }

    fn close(&mut self, out: &mut Vec<Bytes>) {
        if self.open != Open::None {
            send(
                out,
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": self.index}),
            );
            self.index += 1;
            self.open = Open::None;
        }
    }

    fn open_block(&mut self, kind: Open, block: &Value, out: &mut Vec<Bytes>) {
        self.close(out);
        send(
            out,
            "content_block_start",
            &json!({"type": "content_block_start", "index": self.index, "content_block": block}),
        );
        self.open = kind;
    }

    fn open_text(&mut self, out: &mut Vec<Bytes>) {
        self.open_block(Open::Text, &json!({"type": "text", "text": ""}), out);
    }

    fn delta(&self, delta: &Value, out: &mut Vec<Bytes>) {
        send(
            out,
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": self.index, "delta": delta}),
        );
    }

    fn text_delta(&mut self, text: &str, out: &mut Vec<Bytes>) {
        if text.is_empty() {
            return;
        }
        if self.open != Open::Text {
            self.open_text(out);
        }
        self.delta(&json!({"type": "text_delta", "text": text}), out);
    }

    fn fail(&mut self, code: &str, message: &str, out: &mut Vec<Bytes>) {
        self.phase = Phase::Done;
        send(
            out,
            "error",
            &json!({"type": "error", "error": {"type": error_kind(code), "message": message}}),
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
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": reason, "stop_sequence": null},
                "usage": usage(resp),
            }),
        );
        send(out, "message_stop", &json!({"type": "message_stop"}));
        self.phase = Phase::Done;
    }

    fn item_added(&mut self, item: &Value, out: &mut Vec<Bytes>) {
        match str_of(item, "type") {
            "reasoning" if self.opts.thinking => {
                self.open_block(Open::Thinking, &json!({"type": "thinking", "thinking": ""}), out);
            }
            "function_call" => {
                self.has_tool = true;
                self.args_seen = false;
                self.open_block(
                    Open::Tool,
                    &json!({
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

    fn item_done(&mut self, item: &Value, out: &mut Vec<Bytes>) {
        match (str_of(item, "type"), self.open) {
            ("reasoning", Open::Thinking) => {
                self.delta(
                    &json!({"type": "signature_delta", "signature": str_of(item, "encrypted_content")}),
                    out,
                );
                self.close(out);
            }
            ("function_call", Open::Tool) => {
                let args = str_of(item, "arguments");
                if !self.args_seen && !args.is_empty() {
                    self.delta(&json!({"type": "input_json_delta", "partial_json": args}), out);
                }
                self.close(out);
            }
            ("message", Open::Text) => self.close(out),
            _ => {}
        }
    }

    pub(crate) fn event(&mut self, ev: &Value, out: &mut Vec<Bytes>) {
        if self.phase == Phase::Done {
            return;
        }
        let t = str_of(ev, "type");
        match t {
            "response.created" => self.start(str_of(&ev["response"], "id"), out),
            "response.output_item.added" => {
                self.start("", out);
                self.item_added(&ev["item"], out);
            }
            "response.reasoning_summary_part.added" => {
                if self.open == Open::Thinking && ev["summary_index"].as_u64().unwrap_or(0) > 0 {
                    self.delta(&json!({"type": "thinking_delta", "thinking": "\n\n"}), out);
                }
            }
            "response.reasoning_summary_text.delta" => {
                if self.open == Open::Thinking {
                    self.delta(&json!({"type": "thinking_delta", "thinking": str_of(ev, "delta")}), out);
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
                        self.delta(&json!({"type": "input_json_delta", "partial_json": d}), out);
                    }
                }
            }
            "response.output_item.done" => self.item_done(&ev["item"], out),
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

    pub(crate) fn finish(&mut self, out: &mut Vec<Bytes>) {
        if self.phase == Phase::Done {
            return;
        }
        self.close(out);
        self.fail("", "upstream stream ended before completion", out);
    }
}
