//! Incremental parser for the `data:` lines of an SSE response.
//!
//! Follows the Server-Sent Events framing of the HTML Living Standard: a line
//! ends with LF, CRLF, or CR; `data:` field lines of one event are joined with
//! LF; an empty line dispatches the event; a leading BOM is ignored. Bytes may
//! arrive split anywhere, so a line is only interpreted once its terminator is
//! available, and one event never occupies more than [`MAX_EVENT_LEN`] bytes.
//!
//! The parser is API-format agnostic: it yields each event's `data` payload as
//! a string, and the provider layer decides what the payload means. It also
//! keeps each event's type, the last event id and the reconnection time, which
//! MCP's event streams use.

use crate::ErrorInfo;

/// Upper bound for the bytes of one SSE event, matching the transport's frame
/// payload limit: a larger event could not be forwarded as a single JSON frame
/// anyway, and without a bound a peer could grow the buffer without limit.
pub(crate) const MAX_EVENT_LEN: usize = 16 * 1024 * 1024;

#[derive(Default)]
struct Event {
    /// The data lines collected so far, joined with LF.
    data: String,
    /// At least one data line arrived, so the event dispatches even when
    /// its joined data is empty.
    has_data: bool,
    /// Bytes of the data line values collected so far.
    bytes: usize,
    /// The `event` field; empty means `message`.
    kind: String,
}

/// What `id` and `retry` fields said so far; they outlive their events.
#[derive(Default)]
struct Fields {
    last_id: Option<String>,
    retry: Option<u64>,
}

pub(crate) struct SseParser {
    buffer: Vec<u8>,
    /// Start of the unconsumed bytes in `buffer`.
    start: usize,
    /// Bytes of the pending line already searched for a terminator, so a
    /// long line arriving in pieces is scanned once rather than per piece.
    scanned: usize,
    event: Event,
    ended: bool,
    /// No line has been interpreted yet, so a BOM may still be present.
    at_stream_start: bool,
    fields: Fields,
}

impl SseParser {
    pub(crate) fn new() -> Self {
        Self {
            buffer: Vec::new(),
            start: 0,
            scanned: 0,
            event: Event::default(),
            ended: false,
            at_stream_start: true,
            fields: Fields::default(),
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) {
        // Dropping consumed bytes once they outweigh the pending ones keeps
        // the copying amortized constant per byte.
        if self.start > 0 && self.start >= self.buffer.len() - self.start {
            self.buffer.drain(..self.start);
            self.start = 0;
        }
        self.buffer.extend_from_slice(bytes);
    }

    /// Marks the end of the body so a last line without a terminator is still parsed.
    pub(crate) fn end(&mut self) {
        self.ended = true;
    }

    /// Returns the next event's `data` payload, or `None` while more bytes are
    /// needed.
    pub(crate) fn next_item(&mut self) -> Option<Result<String, ErrorInfo>> {
        self.next_event().map(|event| event.map(|(_, data)| data))
    }

    /// The last event id the stream gave, for resuming it.
    pub(crate) fn last_id(&self) -> Option<&str> {
        self.fields.last_id.as_deref()
    }

    /// The reconnection time in milliseconds the stream asked for.
    pub(crate) fn retry(&self) -> Option<u64> {
        self.fields.retry
    }

    /// Returns the next event's type (`message` when it named none) and
    /// `data` payload, or `None` while more bytes are needed.
    pub(crate) fn next_event(&mut self) -> Option<Result<(String, String), ErrorInfo>> {
        loop {
            // The collected data plus the unterminated rest is what the
            // current event can still grow to.
            if self.event.bytes + (self.buffer.len() - self.start) > MAX_EVENT_LEN {
                return Some(Err(ErrorInfo::new(
                    "provider_protocol",
                    format!("SSE event exceeds the limit of {MAX_EVENT_LEN} bytes"),
                )));
            }
            if let Some((line_start, line_end)) = self.next_line() {
                match line_item(
                    &mut self.event,
                    &mut self.at_stream_start,
                    &mut self.fields,
                    &self.buffer[line_start..line_end],
                ) {
                    Ok(Some(event)) => return Some(Ok(event)),
                    Ok(None) => continue,
                    Err(error) => return Some(Err(error)),
                }
            }
            if self.ended {
                // The body is over: a final line without a terminator and an
                // event without its dispatching empty line are still events.
                if self.start < self.buffer.len() {
                    let (line_start, line_end) = (self.start, self.buffer.len());
                    self.start = line_end;
                    self.scanned = 0;
                    match line_item(
                        &mut self.event,
                        &mut self.at_stream_start,
                        &mut self.fields,
                        &self.buffer[line_start..line_end],
                    ) {
                        Ok(Some(event)) => return Some(Ok(event)),
                        Ok(None) => {}
                        Err(error) => return Some(Err(error)),
                    }
                }
                if self.event.has_data {
                    let event = std::mem::take(&mut self.event);
                    if !event.data.is_empty() {
                        return Some(Ok((kind_of(event.kind), event.data)));
                    }
                }
            }
            return None;
        }
    }

    /// Consumes the next complete line and returns its range in `buffer`
    /// without the terminator, or `None` while more bytes are needed.
    ///
    /// A trailing CR at the end of the buffer waits for the next byte, because
    /// it may be the first half of a CRLF pair.
    fn next_line(&mut self) -> Option<(usize, usize)> {
        let from = self.start + self.scanned;
        let Some(offset) = self.buffer[from..]
            .iter()
            .position(|byte| *byte == b'\n' || *byte == b'\r')
        else {
            self.scanned = self.buffer.len() - self.start;
            return None;
        };
        let end = from + offset;
        let is_lf = self.buffer[end] == b'\n';
        let consume = if !is_lf && end + 1 < self.buffer.len() && self.buffer[end + 1] == b'\n' {
            2
        } else if !is_lf && end + 1 == self.buffer.len() && !self.ended {
            // Rescan the CR once the next byte arrives.
            self.scanned = end - self.start;
            return None;
        } else {
            1
        };
        let line_start = self.start;
        self.start = end + consume;
        self.scanned = 0;
        Some((line_start, end))
    }
}

/// Interprets one line of the current event.
///
/// `Ok(None)` means the line produced no item yet; an empty line dispatches
/// the accumulated event.
fn line_item(
    event: &mut Event,
    at_stream_start: &mut bool,
    fields: &mut Fields,
    line: &[u8],
) -> Result<Option<(String, String)>, ErrorInfo> {
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
    let text = if std::mem::take(at_stream_start) {
        text.strip_prefix('\u{feff}').unwrap_or(text)
    } else {
        text
    };
    if text.is_empty() {
        if !event.has_data {
            event.kind.clear();
            return Ok(None);
        }
        // An event whose only data is an empty string carries nothing for
        // either API format.
        let event = std::mem::take(event);
        return Ok(if event.data.is_empty() {
            None
        } else {
            Some((kind_of(event.kind), event.data))
        });
    }
    let (field, value) = match text.split_once(':') {
        Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
        None => (text, ""),
    };
    match field {
        "data" => {}
        "event" => {
            event.kind = value.to_string();
            return Ok(None);
        }
        "id" => {
            if !value.contains(' ') {
                fields.last_id = Some(value.to_string());
            }
            return Ok(None);
        }
        "retry" => {
            if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
                fields.retry = value.parse().ok();
            }
            return Ok(None);
        }
        // Other fields and `:` comments carry nothing.
        _ => return Ok(None),
    }
    if event.has_data {
        event.data.push('\n');
    }
    event.data.push_str(value);
    event.has_data = true;
    event.bytes += value.len();
    Ok(None)
}

/// An event's type: `message` when the stream named none.
fn kind_of(kind: String) -> String {
    if kind.is_empty() {
        "message".to_string()
    } else {
        kind
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
    fn mixed_terminators_parse_the_same_when_fed_byte_by_byte() {
        let stream = "data: a\r\n\r\ndata: b\r\rdata: c\ndata: d\n\n: note\r\ndata: e\r\n\r\n";
        let expected = ["a", "b", "c\nd", "e"];
        let mut whole = SseParser::new();
        whole.push(stream.as_bytes());
        assert_eq!(drain(&mut whole), expected);
        let mut parser = SseParser::new();
        let mut items = Vec::new();
        for byte in stream.as_bytes() {
            parser.push(std::slice::from_ref(byte));
            items.extend(drain(&mut parser));
        }
        assert_eq!(items, expected);
    }

    #[test]
    fn event_types_ids_and_the_retry_time_are_kept() {
        let mut parser = SseParser::new();
        parser.push(
            b"retry: 250
id: 7
data:

event: endpoint
data: /messages

id: 8
data: x

",
        );
        assert_eq!(
            parser.next_event().expect("event").expect("parsed"),
            ("endpoint".to_string(), "/messages".to_string())
        );
        assert_eq!(
            parser.next_event().expect("event").expect("parsed"),
            ("message".to_string(), "x".to_string())
        );
        assert_eq!(parser.last_id(), Some("8"));
        assert_eq!(parser.retry(), Some(250));
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
