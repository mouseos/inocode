//! Incremental parser for the `data:` lines of an SSE response.
//!
//! Follows the Server-Sent Events framing of the HTML Living Standard: a line
//! ends with LF, CRLF, or CR; `data:` field lines of one event are joined with
//! LF; an empty line dispatches the event; a leading BOM is ignored. Bytes may
//! arrive split anywhere, so a line is only interpreted once its terminator is
//! available, and one event never occupies more than [`MAX_EVENT_LEN`] bytes.

use serde_json::Value;

use crate::ErrorInfo;

/// Upper bound for the bytes of one SSE event, matching the transport's frame
/// payload limit: a larger event could not be forwarded as a single JSON frame
/// anyway, and without a bound a peer could grow the buffer without limit.
pub(crate) const MAX_EVENT_LEN: usize = 16 * 1024 * 1024;

/// One item parsed from a chat-completions SSE stream.
#[derive(Debug)]
pub(crate) enum SseItem {
    Delta(String),
    Finished,
}

struct Event {
    data_lines: Vec<String>,
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
    finished: bool,
}

impl SseParser {
    pub(crate) fn new() -> Self {
        Self {
            buffer: Vec::new(),
            event: Event::new(),
            ended: false,
            finished: false,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
        self.event.bytes += bytes.len();
    }

    /// Marks the end of the body so a last line without a terminator is still parsed.
    pub(crate) fn end(&mut self) {
        self.ended = true;
    }

    /// Whether the provider signalled the end of the completion.
    pub(crate) fn is_finished(&self) -> bool {
        self.finished
    }

    /// Returns the next item, or `None` while more bytes are needed.
    pub(crate) fn next_item(&mut self) -> Option<Result<SseItem, ErrorInfo>> {
        if self.finished {
            return Some(Ok(SseItem::Finished));
        }
        loop {
            if self.event.bytes > MAX_EVENT_LEN || self.buffer.len() > MAX_EVENT_LEN {
                return Some(Err(ErrorInfo::new(
                    "provider_protocol",
                    format!("SSE event exceeds the limit of {MAX_EVENT_LEN} bytes"),
                )));
            }
            if let Some(line) = self.next_line() {
                match self.line_item(&line) {
                    Some(Ok(item)) => return Some(Ok(item)),
                    Some(Err(error)) => return Some(Err(error)),
                    None => continue,
                }
            }
            if self.ended {
                // The body is over: a final line without a terminator and an
                // event without its dispatching empty line are still events.
                if !self.buffer.is_empty() {
                    let line = std::mem::take(&mut self.buffer);
                    match self.line_item(&line) {
                        Some(Ok(item)) => return Some(Ok(item)),
                        Some(Err(error)) => return Some(Err(error)),
                        None => {}
                    }
                }
                if !self.event.data_lines.is_empty() {
                    return self.dispatch();
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
    /// the accumulated event, which may still yield no item (no `data` field,
    /// or an event the protocol does not carry text for).
    fn line_item(&mut self, line: &[u8]) -> Option<Result<SseItem, ErrorInfo>> {
        let text = match std::str::from_utf8(line) {
            Ok(text) => text,
            Err(_) => {
                return Some(Err(ErrorInfo::new(
                    "provider_protocol",
                    "SSE line is not valid UTF-8",
                )))
            }
        };
        // The standard allows one BOM at the very start of the stream.
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        if text.is_empty() {
            return self.dispatch();
        }
        let (field, value) = match text.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (text, ""),
        };
        if field != "data" {
            // Other fields and `:` comments carry nothing for this protocol.
            return None;
        }
        self.event.data_lines.push(value.to_string());
        None
    }

    fn dispatch(&mut self) -> Option<Result<SseItem, ErrorInfo>> {
        let data = self.event.data_lines.join("\n");
        self.event = Event::new();
        if data.is_empty() {
            return None;
        }
        if data == "[DONE]" {
            self.finished = true;
            return Some(Ok(SseItem::Finished));
        }
        match parse_chunk(&data) {
            Ok(Some(SseItem::Finished)) => {
                self.finished = true;
                Some(Ok(SseItem::Finished))
            }
            Ok(Some(item)) => Some(Ok(item)),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        }
    }
}

fn parse_chunk(data: &str) -> Result<Option<SseItem>, ErrorInfo> {
    let value: Value = serde_json::from_str(data).map_err(|error| {
        ErrorInfo::new("provider_protocol", format!("invalid SSE payload: {error}"))
    })?;
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("provider reported an error in the stream");
        return Err(ErrorInfo::new("provider_stream", message.to_string()));
    }
    let choice = value.get("choices").and_then(|choices| choices.get(0));
    let finished = choice
        .and_then(|choice| choice.get("finish_reason"))
        .is_some_and(|reason| !reason.is_null());
    let content = choice
        .and_then(|choice| choice.get("delta"))
        .and_then(|delta| delta.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("");
    Ok(match (content.is_empty(), finished) {
        (false, _) => Some(SseItem::Delta(content.to_string())),
        (true, true) => Some(SseItem::Finished),
        (true, false) => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const STREAM: &str =
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"こん\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"にちは\"}}]}\n\ndata: [DONE]\n\n";

    fn drain(parser: &mut SseParser, items: &mut Vec<String>, finished: &mut bool) {
        if *finished {
            return;
        }
        while let Some(item) = parser.next_item() {
            match item.expect("no parse error") {
                SseItem::Delta(text) => items.push(text),
                SseItem::Finished => {
                    items.push("<done>".to_string());
                    *finished = true;
                    break;
                }
            }
        }
    }

    fn collect(parser: &mut SseParser) -> Vec<String> {
        let mut items = Vec::new();
        let mut finished = false;
        drain(parser, &mut items, &mut finished);
        items
    }

    #[test]
    fn parses_when_every_byte_arrives_separately() {
        for split in 0..STREAM.len() {
            let mut parser = SseParser::new();
            let mut items = Vec::new();
            let mut finished = false;
            parser.push(&STREAM.as_bytes()[..split]);
            drain(&mut parser, &mut items, &mut finished);
            parser.push(&STREAM.as_bytes()[split..]);
            drain(&mut parser, &mut items, &mut finished);
            assert_eq!(items, ["こん", "にちは", "<done>"], "split at {split}");
        }
    }

    #[test]
    fn parses_several_events_from_one_push() {
        let mut parser = SseParser::new();
        parser.push(STREAM.as_bytes());
        assert_eq!(collect(&mut parser), ["こん", "にちは", "<done>"]);
    }

    #[test]
    fn accepts_crlf_line_endings() {
        let mut parser = SseParser::new();
        parser.push(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\r\n\r\ndata: [DONE]\r\n\r\n",
        );
        assert_eq!(collect(&mut parser), ["a", "<done>"]);
    }

    #[test]
    fn accepts_bare_cr_line_endings() {
        let mut parser = SseParser::new();
        parser.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\r\rdata: [DONE]\r\r");
        parser.end();
        assert_eq!(collect(&mut parser), ["a", "<done>"]);
    }

    #[test]
    fn joins_multi_line_data_events() {
        let mut parser = SseParser::new();
        parser.push(
            b"data: {\"choices\":[\ndata: {\"delta\":{\"content\":\"a\"}}]}\n\ndata: [DONE]\n\n",
        );
        assert_eq!(collect(&mut parser), ["a", "<done>"]);
    }

    #[test]
    fn ignores_a_leading_byte_order_mark() {
        let mut parser = SseParser::new();
        parser.push(
            "\u{feff}data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\ndata: [DONE]\n\n"
                .as_bytes(),
        );
        assert_eq!(collect(&mut parser), ["a", "<done>"]);
    }

    #[test]
    fn ignores_lines_that_are_not_data_events() {
        let mut parser = SseParser::new();
        parser.push(
            b": keep-alive\n\nevent: message\nretry: 1000\ndata:\n\ndata: {\"choices\":[]}\n\n",
        );
        assert!(parser.next_item().is_none());
    }

    #[test]
    fn finishes_the_last_line_without_a_terminator() {
        let mut parser = SseParser::new();
        parser.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"tail\"}}]}");
        assert!(parser.next_item().is_none());
        parser.end();
        assert_eq!(collect(&mut parser), ["tail"]);
    }

    #[test]
    fn reports_a_body_that_ends_without_done() {
        let mut parser = SseParser::new();
        parser.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"half\"}}]}\n\n");
        assert_eq!(collect(&mut parser), ["half"]);
        assert!(parser.next_item().is_none());
        assert!(!parser.is_finished());
    }

    #[test]
    fn treats_a_finish_reason_as_completion() {
        let mut parser = SseParser::new();
        parser.push(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        );
        assert_eq!(collect(&mut parser), ["a", "<done>"]);
        assert!(parser.is_finished());
    }

    #[test]
    fn rejects_invalid_json_and_stream_errors() {
        let mut parser = SseParser::new();
        parser.push(b"data: {not json}\n\n");
        assert!(
            parser
                .next_item()
                .expect("an item")
                .expect_err("must fail")
                .code
                == "provider_protocol"
        );

        let mut parser = SseParser::new();
        parser.push(b"data: {\"error\":{\"message\":\"rate limited\"}}\n\n");
        let error = parser.next_item().expect("an item").expect_err("must fail");
        assert_eq!(error.code, "provider_stream");
        assert_eq!(error.message, "rate limited");
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
