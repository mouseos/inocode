//! Incremental parser for the `data:` lines of an SSE response.
//!
//! Follows the Server-Sent Events framing of the HTML Living Standard: a line
//! ends with LF, CRLF, or CR; `data:` field lines of one event are joined with
//! LF; an empty line dispatches the event; a leading BOM is ignored. Bytes may
//! arrive split anywhere, so a line is only interpreted once its terminator is
//! available, and one event never occupies more than [`MAX_EVENT_LEN`] bytes.
//!
//! The parser is API-format agnostic: it yields each event's `data` payload as
//! a string, and the provider layer decides what the payload means.

use crate::ErrorInfo;

/// Upper bound for the bytes of one SSE event, matching the transport's frame
/// payload limit: a larger event could not be forwarded as a single JSON frame
/// anyway, and without a bound a peer could grow the buffer without limit.
pub(crate) const MAX_EVENT_LEN: usize = 16 * 1024 * 1024;

struct Event {
    data_lines: Vec<String>,
    /// Bytes of the data lines collected so far.
    bytes: usize,
}

impl Event {
    fn new() -> Self {
        Self {
            data_lines: Vec::new(),
            bytes: 0,
        }
    }
}

pub(crate) struct SseParser {
    buffer: Vec<u8>,
    event: Event,
    ended: bool,
    /// No line has been interpreted yet, so a BOM may still be present.
    at_stream_start: bool,
}

impl SseParser {
    pub(crate) fn new() -> Self {
        Self {
            buffer: Vec::new(),
            event: Event::new(),
            ended: false,
            at_stream_start: true,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// Marks the end of the body so a last line without a terminator is still parsed.
    pub(crate) fn end(&mut self) {
        self.ended = true;
    }

    /// Returns the next event's `data` payload, or `None` while more bytes are
    /// needed.
    pub(crate) fn next_item(&mut self) -> Option<Result<String, ErrorInfo>> {
        loop {
            // The collected data plus the unterminated rest is what the
            // current event can still grow to.
            if self.event.bytes + self.buffer.len() > MAX_EVENT_LEN {
                return Some(Err(ErrorInfo::new(
                    "provider_protocol",
                    format!("SSE event exceeds the limit of {MAX_EVENT_LEN} bytes"),
                )));
            }
            if let Some(line) = self.next_line() {
                match self.line_item(&line) {
                    Ok(Some(event)) => return Some(Ok(event)),
                    Ok(None) => continue,
                    Err(error) => return Some(Err(error)),
                }
            }
            if self.ended {
                // The body is over: a final line without a terminator and an
                // event without its dispatching empty line are still events.
                if !self.buffer.is_empty() {
                    let line = std::mem::take(&mut self.buffer);
                    match self.line_item(&line) {
                        Ok(Some(event)) => return Some(Ok(event)),
                        Ok(None) => {}
                        Err(error) => return Some(Err(error)),
                    }
                }
                if !self.event.data_lines.is_empty() {
                    let data = self.take_event();
                    if !data.is_empty() {
                        return Some(Ok(data));
                    }
                }
            }
            return None;
        }
    }

    /// Extracts the next complete line, or `None` while more bytes are needed.
    ///
    /// A trailing CR at the end of the buffer waits for the next byte, because
    /// it may be the first half of a CRLF pair.
    fn next_line(&mut self) -> Option<Vec<u8>> {
        let end = self
            .buffer
            .iter()
            .position(|byte| *byte == b'\n' || *byte == b'\r')?;
        let is_lf = self.buffer[end] == b'\n';
        let consume = if !is_lf && end + 1 < self.buffer.len() && self.buffer[end + 1] == b'\n' {
            2
        } else if !is_lf && end + 1 == self.buffer.len() && !self.ended {
            return None;
        } else {
            1
        };
        let mut line: Vec<u8> = self.buffer.drain(..end + consume).collect();
        line.truncate(end);
        Some(line)
    }

    /// Interprets one line of the current event.
    ///
    /// `Ok(None)` means the line produced no item yet; an empty line dispatches
    /// the accumulated event.
    fn line_item(&mut self, line: &[u8]) -> Result<Option<String>, ErrorInfo> {
        let text = match std::str::from_utf8(line) {
            Ok(text) => text,
            Err(_) => {
                return Err(ErrorInfo::new(
                    "provider_protocol",
                    "SSE line is not valid UTF-8",
                ))
            }
        };
        // The standard allows one BOM at the very start of the stream.
        let text = if std::mem::take(&mut self.at_stream_start) {
            text.strip_prefix('\u{feff}').unwrap_or(text)
        } else {
            text
        };
        if text.is_empty() {
            if self.event.data_lines.is_empty() {
                return Ok(None);
            }
            // An event whose only data is an empty string carries nothing for
            // either API format.
            let data = self.take_event();
            return Ok(if data.is_empty() { None } else { Some(data) });
        }
        let (field, value) = match text.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (text, ""),
        };
        if field != "data" {
            // Other fields and `:` comments carry nothing for this protocol.
            return Ok(None);
        }
        self.event.bytes += value.len();
        self.event.data_lines.push(value.to_string());
        Ok(None)
    }

    fn take_event(&mut self) -> String {
        let data = self.event.data_lines.join("\n");
        self.event = Event::new();
        data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STREAM: &str = "data: one\n\ndata: こん\n\ndata: にちは\n\n";

    fn drain(parser: &mut SseParser) -> Vec<String> {
        let mut items = Vec::new();
        while let Some(item) = parser.next_item() {
            items.push(item.expect("no parse error"));
        }
        items
    }

    #[test]
    fn parses_when_every_byte_arrives_separately() {
        for split in 0..STREAM.len() {
            let mut parser = SseParser::new();
            parser.push(&STREAM.as_bytes()[..split]);
            let mut items = drain(&mut parser);
            parser.push(&STREAM.as_bytes()[split..]);
            items.extend(drain(&mut parser));
            assert_eq!(items, ["one", "こん", "にちは"], "split at {split}");
        }
    }

    #[test]
    fn parses_several_events_from_one_push() {
        let mut parser = SseParser::new();
        parser.push(STREAM.as_bytes());
        assert_eq!(drain(&mut parser), ["one", "こん", "にちは"]);
    }

    #[test]
    fn accepts_crlf_line_endings() {
        let mut parser = SseParser::new();
        parser.push(b"data: a\r\n\r\ndata: b\r\n\r\n");
        assert_eq!(drain(&mut parser), ["a", "b"]);
    }

    #[test]
    fn accepts_bare_cr_line_endings() {
        let mut parser = SseParser::new();
        parser.push(b"data: a\r\rdata: b\r\r");
        parser.end();
        assert_eq!(drain(&mut parser), ["a", "b"]);
    }

    #[test]
    fn joins_multi_line_data_events() {
        let mut parser = SseParser::new();
        parser.push(b"data: {\"a\":\ndata: 1}\n\ndata: done\n\n");
        assert_eq!(drain(&mut parser), ["{\"a\":\n1}", "done"]);
    }

    #[test]
    fn ignores_a_leading_byte_order_mark() {
        let mut parser = SseParser::new();
        parser.push("\u{feff}data: a\n\ndata: b\n\n".as_bytes());
        assert_eq!(drain(&mut parser), ["a", "b"]);
    }

    #[test]
    fn keeps_a_byte_order_mark_after_the_first_line() {
        let mut parser = SseParser::new();
        parser.push("data: a\n\n\u{feff}data: b\n\n".as_bytes());
        parser.end();
        // Only the stream's first BOM is dropped; a later one makes the line
        // an unknown field, which carries nothing.
        assert_eq!(drain(&mut parser), vec!["a".to_string()]);
    }

    #[test]
    fn heartbeat_comments_do_not_count_toward_the_event_limit() {
        let heartbeat = format!(": {}\n\n", "p".repeat(1024));
        let mut parser = SseParser::new();
        for _ in 0..(MAX_EVENT_LEN / 1024 + 16) {
            parser.push(heartbeat.as_bytes());
            assert!(parser.next_item().is_none(), "comments carry no event");
        }
        parser.push(b"data: after\n\n");
        assert_eq!(drain(&mut parser), vec!["after".to_string()]);
    }

    #[test]
    fn ignores_lines_that_are_not_data_events() {
        let mut parser = SseParser::new();
        parser.push(b": keep-alive\n\nevent: message\nretry: 1000\ndata:\n\n");
        assert!(parser.next_item().is_none());
    }

    #[test]
    fn finishes_the_last_line_without_a_terminator() {
        let mut parser = SseParser::new();
        parser.push(b"data: tail");
        assert!(parser.next_item().is_none());
        parser.end();
        assert_eq!(drain(&mut parser), ["tail"]);
    }

    #[test]
    fn dispatches_an_event_without_its_final_empty_line_at_body_end() {
        let mut parser = SseParser::new();
        parser.push(b"data: one\n\ndata: two\n");
        assert_eq!(drain(&mut parser), ["one"]);
        parser.end();
        assert_eq!(drain(&mut parser), ["two"]);
    }

    #[test]
    fn rejects_invalid_utf8() {
        let mut parser = SseParser::new();
        parser.push(b"data: \xff\xfe\n\n");
        let error = parser.next_item().expect("an item").expect_err("must fail");
        assert_eq!(error.code, "provider_protocol");
    }

    #[test]
    fn bounds_an_event_that_never_terminates() {
        let mut parser = SseParser::new();
        parser.push(&vec![b'a'; MAX_EVENT_LEN + 1]);
        let error = parser.next_item().expect("an item").expect_err("must fail");
        assert_eq!(error.code, "provider_protocol");
        assert!(error.message.contains("exceeds the limit"));
    }
}
