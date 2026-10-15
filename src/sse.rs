use bytes::Bytes;
use eventsource_stream::Event;
use std::collections::VecDeque;
use std::mem;

pub(crate) fn frame(event: Option<&str>, data: &str) -> Bytes {
    let mut s = String::with_capacity(data.len() + 32);
    if let Some(e) = event {
        s.push_str("event: ");
        s.push_str(e);
        s.push('\n');
    }
    s.push_str("data: ");
    s.push_str(data);
    s.push_str("\n\n");
    Bytes::from(s)
}

const BOM: &[u8] = "\u{feff}".as_bytes();

#[derive(Clone, Copy, PartialEq, Eq)]
enum Edge {
    Start,
    Line,
    Cr,
}

pub(crate) struct Splitter {
    keep: fn(&str) -> bool,
    retain: bool,
    first: bool,
    edge: Edge,
    line: Vec<u8>,
    event: String,
    data: Vec<u8>,
    has_data: bool,
    dispatched: usize,
    events: VecDeque<Event>,
    held: Vec<Bytes>,
    released: Vec<Bytes>,
}

impl Splitter {
    pub(crate) fn new(keep: fn(&str) -> bool, retain: bool) -> Self {
        Self {
            keep,
            retain,
            first: true,
            edge: Edge::Start,
            line: Vec::new(),
            event: String::new(),
            data: Vec::new(),
            has_data: false,
            dispatched: 0,
            events: VecDeque::new(),
            held: Vec::new(),
            released: Vec::new(),
        }
    }

    pub(crate) fn feed(&mut self, chunk: Bytes) {
        if chunk.is_empty() {
            return;
        }
        let mut i = match self.edge {
            Edge::Start if chunk.starts_with(BOM) => BOM.len(),
            Edge::Cr if chunk[0] == b'\n' => 1,
            _ => 0,
        };
        self.edge = Edge::Line;
        let mut boundary = None;
        while let Some(n) = memchr::memchr2(b'\n', b'\r', &chunk[i..]) {
            let end = i + n;
            let mut next = end + 1;
            if chunk[end] == b'\r' {
                match chunk.get(next) {
                    Some(b'\n') => next += 1,
                    Some(_) => {}
                    None => self.edge = Edge::Cr,
                }
            }
            let blank = if self.line.is_empty() {
                self.on_line(&chunk[i..end])
            } else {
                let mut line = mem::take(&mut self.line);
                line.extend_from_slice(&chunk[i..end]);
                let blank = self.on_line(&line);
                line.clear();
                self.line = line;
                blank
            };
            if blank {
                boundary = Some(next);
            }
            i = next;
        }
        self.line.extend_from_slice(&chunk[i..]);
        if self.retain {
            match boundary {
                Some(b) => {
                    self.released.append(&mut self.held);
                    self.released.push(chunk.slice(..b));
                    if b < chunk.len() {
                        self.held.push(chunk.slice(b..));
                    }
                }
                None => self.held.push(chunk),
            }
        }
    }

    fn wanted(&self) -> bool {
        self.first || (self.keep)(if self.event.is_empty() { "message" } else { &self.event })
    }

    fn on_line(&mut self, line: &[u8]) -> bool {
        if line.is_empty() {
            self.dispatch();
            return true;
        }
        if line[0] == b':' {
            return false;
        }
        let (name, value) = match memchr::memchr(b':', line) {
            Some(p) => {
                let v = &line[p + 1..];
                (&line[..p], v.strip_prefix(b" ").unwrap_or(v))
            }
            None => (line, &b""[..]),
        };
        match name {
            b"event" => {
                self.event.clear();
                self.event.push_str(&String::from_utf8_lossy(value));
            }
            b"data" => {
                if self.wanted() {
                    if self.has_data {
                        self.data.push(b'\n');
                    }
                    self.data.extend_from_slice(value);
                }
                self.has_data = true;
            }
            _ => {}
        }
        false
    }

    fn dispatch(&mut self) {
        if mem::take(&mut self.has_data) {
            self.dispatched += 1;
            if self.wanted()
                && let Ok(data) = String::from_utf8(mem::take(&mut self.data))
            {
                let event = if self.event.is_empty() {
                    "message".to_owned()
                } else {
                    mem::take(&mut self.event)
                };
                self.events.push_back(Event {
                    event,
                    data,
                    ..Event::default()
                });
            }
            self.first = false;
        }
        self.event.clear();
        self.data.clear();
    }

    pub(crate) fn pop(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    pub(crate) fn take_dispatched(&mut self) -> bool {
        mem::take(&mut self.dispatched) > 0
    }

    pub(crate) fn released(&mut self, out: &mut Vec<Bytes>) {
        out.append(&mut self.released);
    }

    pub(crate) fn rest(&mut self, out: &mut Vec<Bytes>) {
        out.append(&mut self.released);
        out.append(&mut self.held);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eventsource_stream::Eventsource;
    use futures_util::StreamExt;

    const INPUT: &str = ": hi\r\nevent: a\r\ndata: 1\r\ndata:2\r\n\r\nid: 7\nretry: 5\ndata\n\nevent: b\n\n\
        data: {\"x\":\"\u{e9}\"}\revent:c\r\rdata: \n\n: tail\n\nevent: d\ndata: cut";

    #[tokio::test]
    async fn matches_eventsource_at_every_split() {
        let raw = INPUT.as_bytes();
        let want: Vec<(String, String)> = futures_util::stream::iter([Ok::<_, std::convert::Infallible>(raw)])
            .eventsource()
            .map(|e| e.map(|e| (e.event, e.data)).unwrap())
            .collect()
            .await;
        assert_eq!(want.len(), 4);
        for a in 1..raw.len() {
            for b in a..raw.len() {
                let mut s = Splitter::new(|_| true, true);
                for part in [&raw[..a], &raw[a..b], &raw[b..]] {
                    s.feed(Bytes::copy_from_slice(part));
                }
                let got: Vec<(String, String)> = std::iter::from_fn(|| s.pop()).map(|e| (e.event, e.data)).collect();
                assert_eq!(got, want, "split at {a},{b}");
                let mut out = Vec::new();
                s.rest(&mut out);
                assert_eq!(out.concat(), raw);
            }
        }
    }
}
