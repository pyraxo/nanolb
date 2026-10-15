use super::Stream;
use crate::translate::{Event, write_json};
use bytes::Bytes;
use serde_json::Value;

impl Stream {
    pub(super) fn set_tail(&mut self) {
        let t = &mut self.tail;
        t.clear();
        t.extend_from_slice(b",\"created\":");
        write_json(t, &self.created);
        t.extend_from_slice(b",\"id\":");
        write_json(t, &self.id);
        t.extend_from_slice(b",\"model\":");
        write_json(t, &self.opts.model);
        t.extend_from_slice(b",\"object\":\"chat.completion.chunk\"");
        if self.opts.openrouter {
            t.extend_from_slice(b",\"provider\":\"OpenAI\"");
        }
        if self.opts.include_usage {
            t.extend_from_slice(b",\"usage\":null");
        }
        t.push(b'}');
    }

    fn raw(&self, out: &mut Vec<Bytes>, delta: impl FnOnce(&mut Vec<u8>)) {
        let mut b = Vec::with_capacity(96 + self.tail.len());
        b.extend_from_slice(b"data: {\"choices\":[{\"delta\":");
        delta(&mut b);
        b.extend_from_slice(b",\"finish_reason\":null,\"index\":0}]");
        b.extend_from_slice(&self.tail);
        b.extend_from_slice(b"\n\n");
        out.push(Bytes::from(b));
    }

    fn raw_text(&mut self, out: &mut Vec<Bytes>, key: &str, text: Option<&str>) {
        self.start(out);
        self.raw(out, |b| {
            b.extend_from_slice(b"{\"");
            b.extend_from_slice(key.as_bytes());
            b.extend_from_slice(b"\":");
            write_json(b, &text);
            b.push(b'}');
        });
    }

    fn raw_args(&mut self, out: &mut Vec<Bytes>, item_id: &str, args: &str) {
        let Some(index) = self.slot(item_id, false) else { return };
        self.raw(out, |b| {
            b.extend_from_slice(b"{\"tool_calls\":[{\"function\":{\"arguments\":");
            write_json(b, &args);
            b.extend_from_slice(b"},\"index\":");
            write_json(b, &index);
            b.extend_from_slice(b"}]}");
        });
    }

    pub(crate) fn event_data(&mut self, data: &str, out: &mut Vec<Bytes>) {
        if let Ok(ev) = serde_json::from_str::<Event<'_>>(data) {
            match &*ev.kind {
                "response.output_text.delta" => return self.raw_text(out, "content", ev.delta()),
                "response.refusal.delta" => return self.raw_text(out, "refusal", ev.delta()),
                "response.reasoning_summary_text.delta" if self.opts.include_reasoning => {
                    return self.raw_text(out, "reasoning", ev.delta());
                }
                "response.function_call_arguments.delta" => return self.raw_args(out, ev.item_id(), ev.delta().unwrap_or("")),
                "response.created"
                | "response.reasoning_summary_part.added"
                | "response.output_item.added"
                | "response.function_call_arguments.done"
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
