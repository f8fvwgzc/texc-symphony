//! Minimal incremental Server-Sent Events parser (`event:` / `data:` / `id:` fields).

/// One dispatched SSE event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Frame {
    pub event: Option<String>,
    pub data: String,
}

/// Splits a byte stream into [`Frame`]s.
#[derive(Debug, Default)]
pub(crate) struct Parser {
    buffer: String,
    current: Frame,
    has_data: bool,
}

impl Parser {
    /// Feeds bytes; returns the frames completed by them.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<Frame> {
        self.buffer.push_str(&String::from_utf8_lossy(bytes));
        let mut frames = Vec::new();
        while let Some(end) = self.buffer.find('\n') {
            let line: String = self.buffer.drain(..=end).collect();
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if self.has_data {
                    frames.push(std::mem::take(&mut self.current));
                }
                self.current = Frame::default();
                self.has_data = false;
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => self.current.event = Some(value.to_owned()),
                "data" => {
                    if self.has_data {
                        self.current.data.push('\n');
                    }
                    self.current.data.push_str(value);
                    self.has_data = true;
                }
                _ => {}
            }
        }
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_survive_arbitrary_chunking() {
        let input = "retry: 3000\n\n: keep-alive\n\nevent: task\ndata: {\"a\":\ndata: 1}\n\nevent: run_event\r\ndata: x\r\n\r\n";
        for split in 1..input.len() {
            let mut parser = Parser::default();
            let mut frames = parser.push(&input.as_bytes()[..split]);
            frames.extend(parser.push(&input.as_bytes()[split..]));
            assert_eq!(
                frames,
                vec![
                    Frame {
                        event: Some("task".into()),
                        data: "{\"a\":\n1}".into()
                    },
                    Frame {
                        event: Some("run_event".into()),
                        data: "x".into()
                    },
                ],
                "split at {split}"
            );
        }
    }
}
