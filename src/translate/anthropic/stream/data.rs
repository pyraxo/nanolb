use super::{Open, Phase, Stream};
use crate::translate::{Event, write_json};
use bytes::Bytes;
use serde_json::Value;

impl Stream {
    fn raw_delta(&self, key: &str, kind: &str, text: &str, out: &mut Vec<Bytes>) {
        let mut b = Vec::with_capacity(128 + text.len());
        b.extend_from_slice(b"event: content_block_delta\ndata: {\"delta\":{\"");
        b.extend_from_slice(key.as_bytes());
        b.extend_from_slice(b"\":");
        write_json(&mut b, &text);
        b.extend_from_slice(b",\"type\":\"");
        b.extend_from_slice(kind.as_bytes());
        b.extend_from_slice(b"\"},\"index\":");
        write_json(&mut b, &self.index);
        b.extend_from_slice(b",\"type\":\"content_block_delta\"}\n\n");
        out.push(Bytes::from(b));
    }

    fn raw_text(&mut self, text: &str, out: &mut Vec<Bytes>) {
        self.start("", out);
        if text.is_empty() {
            return;
        }
        if self.open != Open::Text {
            self.open_text(out);
        }
        self.raw_delta("text", "text_delta", text, out);
    }

    pub(crate) fn event_data(&mut self, data: &str, out: &mut Vec<Bytes>) {
        if self.phase == Phase::Done {
            return;
        }
        if let Ok(ev) = serde_json::from_str::<Event<'_>>(data) {
            let d = ev.delta().unwrap_or("");
            match &*ev.kind {
                "response.output_text.delta" => return self.raw_text(d, out),
                "response.refusal.delta" => {
                    self.refused = true;
                    return self.raw_text(d, out);
                }
                "response.reasoning_summary_text.delta" => {
                    if self.open == Open::Thinking {
                        self.raw_delta("thinking", "thinking_delta", d, out);
                    }
                    return;
                }
                "response.function_call_arguments.delta" => {
                    if self.open == Open::Tool && !d.is_empty() {
                        self.args_seen = true;
                        self.raw_delta("partial_json", "input_json_delta", d, out);
                    }
                    return;
                }
                "response.content_part.done" | "response.output_text.done" | "response.refusal.done" => {
                    if self.open == Open::Text {
                        self.close(out);
                    }
                    return;
                }
                "response.created"
                | "response.output_item.added"
                | "response.reasoning_summary_part.added"
                | "response.content_part.added"
                | "response.output_item.done"
                | "response.completed"
                | "response.incomplete"
                | "response.failed"
                | "error" => {}
                _ => return,
            }
        }
        if let Ok(v) = serde_json::from_str::<Value>(data) {
            self.event(&v, out);
        }
    }
}
