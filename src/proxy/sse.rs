//! SSE (Server-Sent Events) buffered filter.
//!
//! The OpenAI streaming format emits a series of `data: {json}\n\n` events.
//! Backends and intermediaries may split a single event across multiple TCP
//! chunks, or pack multiple events into one chunk, so we cannot assume a
//! 1:1 mapping between incoming `Bytes` chunks and SSE events.
//!
//! `SseFilter` accumulates bytes, splits on the `\n\n` boundary, applies a
//! per-event JSON transform to `choices[].delta.content`, and re-emits each
//! complete event. Non-data lines (`event: ...`, `: comment`, blank) and the
//! `[DONE]` sentinel pass through untouched.

use bytes::Bytes;
use serde_json::Value;

pub struct SseFilter<F: Fn(&str) -> String> {
    buffer: Vec<u8>,
    transform: F,
}

impl<F: Fn(&str) -> String> SseFilter<F> {
    pub fn new(transform: F) -> Self {
        Self {
            buffer: Vec::new(),
            transform,
        }
    }

    /// Feed an incoming chunk and return any complete events that can now be
    /// emitted. Partial trailing bytes are kept for the next call.
    pub fn push(&mut self, chunk: &[u8]) -> Bytes {
        self.buffer.extend_from_slice(chunk);
        let mut out: Vec<u8> = Vec::new();
        // SSE events are separated by a blank line ("\n\n" or "\r\n\r\n").
        while let Some(end) = find_event_end(&self.buffer) {
            let raw = &self.buffer[..end.split];
            let processed = self.process_event(raw);
            out.extend_from_slice(&processed);
            self.buffer.drain(..end.consumed);
        }
        Bytes::from(out)
    }

    /// Flush any remaining buffered bytes (called when the upstream stream
    /// has ended). Anything not terminated by a blank line is passed through
    /// verbatim — the upstream truncated, so we must not silently drop it.
    pub fn flush(&mut self) -> Bytes {
        if self.buffer.is_empty() {
            return Bytes::new();
        }
        let tail = std::mem::take(&mut self.buffer);
        Bytes::from(tail)
    }

    /// Extract `usage` from a single SSE event's `data:` JSON, if present.
    /// OpenAI emits usage on the final chunk when `stream_options.include_usage`
    /// is set. The buffer is *not* consumed; this is a read-only inspector.
    pub fn try_extract_usage(raw: &[u8]) -> Option<(u64, u64, String)> {
        let text = std::str::from_utf8(raw).ok()?;
        if !text.contains("data:") || text.contains("[DONE]") {
            return None;
        }
        for line in text.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let payload = line.strip_prefix("data:")?.trim_start();
            if payload == "[DONE]" || payload.is_empty() {
                continue;
            }
            let val: Value = serde_json::from_str(payload).ok()?;
            let usage = val.get("usage")?;
            let prompt = usage.get("prompt_tokens").and_then(|v| v.as_u64())?;
            let completion = usage.get("completion_tokens").and_then(|v| v.as_u64())?;
            let model = val
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            return Some((prompt, completion, model));
        }
        None
    }

    fn process_event(&self, raw: &[u8]) -> Vec<u8> {
        // Each event is one or more lines. Locate the (single) `data:` line
        // and try to JSON-parse + transform it. If anything looks unusual,
        // pass the bytes through unchanged.
        let Ok(text) = std::str::from_utf8(raw) else {
            return with_terminator(raw);
        };

        // [DONE] sentinel and non-data events pass through.
        if text.contains("[DONE]") || !text.contains("data:") {
            return with_terminator(raw);
        }

        // Reconstruct line-by-line so prefixes and other fields survive.
        let mut rebuilt = String::with_capacity(text.len());
        let mut changed = false;
        for (i, line) in text.split('\n').enumerate() {
            if i > 0 {
                rebuilt.push('\n');
            }
            let stripped = line.strip_suffix('\r');
            let work = stripped.unwrap_or(line);

            if let Some(payload) = work.strip_prefix("data:") {
                let payload = payload.trim_start();
                if payload == "[DONE]" || payload.is_empty() {
                    rebuilt.push_str(line);
                    continue;
                }
                match serde_json::from_str::<Value>(payload) {
                    Ok(mut val) => {
                        if filter_delta_content(&mut val, &self.transform) {
                            changed = true;
                        }
                        rebuilt.push_str("data: ");
                        rebuilt.push_str(&val.to_string());
                        if stripped.is_some() {
                            rebuilt.push('\r');
                        }
                    }
                    Err(_) => rebuilt.push_str(line),
                }
            } else {
                rebuilt.push_str(line);
            }
        }

        let mut out = if changed {
            rebuilt.into_bytes()
        } else {
            raw.to_vec()
        };
        // Re-attach the blank-line terminator that was stripped by find_event_end.
        out.extend_from_slice(b"\n\n");
        out
    }
}

fn with_terminator(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + 2);
    out.extend_from_slice(raw);
    out.extend_from_slice(b"\n\n");
    out
}

struct EventEnd {
    /// End of the event content (exclusive of blank-line terminator bytes).
    split: usize,
    /// Total bytes to drain from the buffer (includes terminator).
    consumed: usize,
}

fn find_event_end(buf: &[u8]) -> Option<EventEnd> {
    // Search for "\n\n" or "\r\n\r\n".
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some(EventEnd {
                split: i,
                consumed: i + 2,
            });
        }
        if i + 3 < buf.len() && &buf[i..i + 4] == b"\r\n\r\n" {
            return Some(EventEnd {
                split: i,
                consumed: i + 4,
            });
        }
        i += 1;
    }
    None
}

fn filter_delta_content<F: Fn(&str) -> String>(val: &mut Value, transform: &F) -> bool {
    let Some(choices) = val.get_mut("choices").and_then(|c| c.as_array_mut()) else {
        return false;
    };
    let mut changed = false;
    for choice in choices {
        let Some(delta) = choice.get_mut("delta") else {
            continue;
        };
        let Some(content) = delta.get_mut("content") else {
            continue;
        };
        let Some(text) = content.as_str() else {
            continue;
        };
        let filtered = transform(text);
        if filtered != text {
            *content = Value::String(filtered);
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn star_filter(s: &str) -> String {
        s.replace("ssn", "***")
    }

    #[test]
    fn passes_through_clean_event() {
        let mut f = SseFilter::new(star_filter);
        let out = f.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n");
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.starts_with("data:"));
        assert!(s.contains("\"content\":\"hello\""));
        assert!(s.ends_with("\n\n"));
    }

    #[test]
    fn filters_matched_content() {
        let mut f = SseFilter::new(star_filter);
        let out = f.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"your ssn is here\"}}]}\n\n");
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("your *** is here"));
        assert!(!s.contains("ssn"));
    }

    #[test]
    fn handles_split_event_across_two_chunks() {
        let mut f = SseFilter::new(star_filter);
        // First half: no terminator yet.
        let a = f.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"your ");
        assert!(a.is_empty(), "no event boundary yet, must buffer");

        // Second half: completes the event.
        let b = f.push(b"ssn is here\"}}]}\n\n");
        let s = std::str::from_utf8(&b).unwrap();
        assert!(s.contains("your *** is here"), "got `{s}`");
    }

    #[test]
    fn handles_multiple_events_in_one_chunk() {
        let mut f = SseFilter::new(star_filter);
        let chunk = b"data: {\"choices\":[{\"delta\":{\"content\":\"a ssn\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"b ok\"}}]}\n\n";
        let out = f.push(chunk);
        let s = std::str::from_utf8(&out).unwrap();
        let events: Vec<&str> = s.trim_end_matches("\n\n").split("\n\n").collect();
        assert_eq!(events.len(), 2, "expected two events, got `{s}`");
        assert!(events[0].contains("a ***"));
        assert!(events[1].contains("b ok"));
    }

    #[test]
    fn done_sentinel_passes_through() {
        let mut f = SseFilter::new(star_filter);
        let out = f.push(b"data: [DONE]\n\n");
        let s = std::str::from_utf8(&out).unwrap();
        assert_eq!(s, "data: [DONE]\n\n");
    }

    #[test]
    fn comment_lines_pass_through() {
        let mut f = SseFilter::new(star_filter);
        let out = f.push(b": keep-alive\n\n");
        let s = std::str::from_utf8(&out).unwrap();
        assert_eq!(s, ": keep-alive\n\n");
    }

    #[test]
    fn crlf_line_endings_supported() {
        let mut f = SseFilter::new(star_filter);
        let out = f.push(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"ssn here\"}}]}\r\n\r\n",
        );
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("*** here"), "got `{s}`");
    }

    #[test]
    fn flush_returns_partial_trailing_bytes() {
        let mut f = SseFilter::new(star_filter);
        f.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"partial");
        let tail = f.flush();
        assert!(!tail.is_empty(), "flush should release buffered bytes");
    }

    #[test]
    fn invalid_json_passes_through() {
        let mut f = SseFilter::new(star_filter);
        let out = f.push(b"data: not json\n\n");
        let s = std::str::from_utf8(&out).unwrap();
        assert_eq!(s, "data: not json\n\n");
    }

    #[test]
    fn event_without_choices_passes_through() {
        let mut f = SseFilter::new(star_filter);
        let out = f.push(b"data: {\"object\":\"chat.completion.chunk\"}\n\n");
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("\"object\":\"chat.completion.chunk\""));
    }

    #[test]
    fn extract_usage_from_final_chunk() {
        let chunk = b"data: {\"id\":\"x\",\"choices\":[],\"model\":\"gpt-4o-mini\",\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":34,\"total_tokens\":46}}\n\n";
        let usage = SseFilter::<fn(&str) -> String>::try_extract_usage(chunk).unwrap();
        assert_eq!(usage, (12, 34, "gpt-4o-mini".to_string()));
    }

    #[test]
    fn extract_usage_returns_none_when_absent() {
        let chunk = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";
        assert!(SseFilter::<fn(&str) -> String>::try_extract_usage(chunk).is_none());
    }

    #[test]
    fn extract_usage_returns_none_for_done() {
        assert!(
            SseFilter::<fn(&str) -> String>::try_extract_usage(b"data: [DONE]\n\n").is_none()
        );
    }
}
