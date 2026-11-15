use serde_json::Value;

use crate::ErrorInfo;

/// One item parsed from a chat-completions SSE stream.
#[derive(Debug)]
pub(crate) enum SseItem {
    Delta(String),
    Finished,
}

/// Incremental parser for the `data:` lines of an SSE response.
///
/// Bytes may arrive split anywhere, so a line is only interpreted once its
/// terminating newline (or the end of the body) is available.
pub(crate) struct SseParser {
    buffer: Vec<u8>,
    ended: bool,
    finished: bool,
}

impl SseParser {
    pub(crate) fn new() -> Self {
        Self {
            buffer: Vec::new(),
            ended: false,
            finished: false,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// Marks the end of the body so a last line without a newline is still parsed.
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
            if let Some(position) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=position).collect();
                if let Some(item) = self.line_item(&line[..line.len() - 1]) {
                    return Some(item);
                }
                continue;
            }
            if self.ended && !self.buffer.is_empty() {
                let line = std::mem::take(&mut self.buffer);
                return self.line_item(&line);
            }
            return None;
        }
    }

    /// `None` means the line carried no item; other SSE fields are ignored.
    fn line_item(&mut self, line: &[u8]) -> Option<Result<SseItem, ErrorInfo>> {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let text = match std::str::from_utf8(line) {
            Ok(text) => text,
            Err(_) => {
                return Some(Err(ErrorInfo::new(
                    "provider_protocol",
                    "SSE line is not valid UTF-8",
                )))
            }
        };
        let data = text.strip_prefix("data:")?.trim_start();
        if data.is_empty() {
            return None;
        }
        if data == "[DONE]" {
            self.finished = true;
            return Some(Ok(SseItem::Finished));
        }
        match parse_chunk(data) {
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
    let content = value
        .get("choices")
        .and_then(|choices| choices.get(0))
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
    fn ignores_lines_that_are_not_data_events() {
        let mut parser = SseParser::new();
        parser.push(b": keep-alive\n\nevent: message\n\ndata:\n\ndata: {\"choices\":[]}\n\n");
        assert!(parser.next_item().is_none());
    }

    #[test]
    fn finishes_the_last_line_without_a_newline() {
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
            b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}

data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}

",
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
}
