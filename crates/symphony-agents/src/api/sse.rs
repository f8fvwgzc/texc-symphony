//! Incremental Server-Sent Events parser for streamed model responses.
//!
//! Bytes arrive in arbitrary chunks ([`reqwest::Response::chunk`]); [`SseParser::feed`] buffers
//! them and returns every complete event (`event:` name and joined `data:` lines). Comments
//! (`:` lines), `id:` and `retry:` are ignored; `\r\n` and `\n` line endings are both accepted.

/// One parsed event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    /// The `event:` field, if any.
    pub event: Option<String>,
    /// The `data:` lines joined with `\n`.
    pub data: String,
}

/// Buffers partial input between chunks.
#[derive(Debug, Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
    has_data: bool,
}

impl SseParser {
    /// A parser with an empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Consumes `chunk` and returns the events it completed.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(end) = self.buffer.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line).into_owned();
            if let Some(event) = self.line(&line) {
                events.push(event);
            }
        }
        events
    }

    /// The event still pending at end of stream (a server that omits the final blank line).
    pub fn finish(&mut self) -> Option<SseEvent> {
        if !self.buffer.is_empty() {
            let rest = String::from_utf8_lossy(&std::mem::take(&mut self.buffer)).into_owned();
            let rest = rest.trim_end_matches('\r').to_owned();
            if let Some(event) = self.line(&rest) {
                return Some(event);
            }
        }
        self.dispatch()
    }

    fn line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => {
                self.data.push(value.to_owned());
                self.has_data = true;
            }
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        let event = self.event.take();
        if !self.has_data {
            return None;
        }
        self.has_data = false;
        Some(SseEvent {
            event,
            data: std::mem::take(&mut self.data).join("\n"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_split_across_chunks_are_reassembled() {
        let mut parser = SseParser::new();
        assert!(parser.feed(b"event: ping\nda").is_empty());
        let events = parser.feed(b"ta: {\"a\":1}\r\n\r\n: comment\ndata: x\ndata: y\n\n");
        assert_eq!(
            events,
            vec![
                SseEvent {
                    event: Some("ping".into()),
                    data: "{\"a\":1}".into()
                },
                SseEvent {
                    event: None,
                    data: "x\ny".into()
                },
            ]
        );
        assert!(parser.feed(b"event: only-name\n\n").is_empty());
    }

    #[test]
    fn a_trailing_event_without_blank_line_is_flushed() {
        let mut parser = SseParser::new();
        assert!(parser.feed(b"data: [DONE]").is_empty());
        assert_eq!(
            parser.finish(),
            Some(SseEvent {
                event: None,
                data: "[DONE]".into()
            })
        );
        assert_eq!(parser.finish(), None);
    }
}
