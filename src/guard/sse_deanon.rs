//! Streaming deanonymizer that walks `delta.content` text across SSE event
//! boundaries. The placeholder format (`[REDACTED_<ENTITY>_<N>]` or
//! `[<ENTITY>_<N>]`) can be split across events; this state machine buffers
//! from `[` until the matching `]` is seen, then attempts a Vault lookup.
//!
//! The rationale: SSE chunk boundaries are independent of placeholder
//! boundaries, so a placeholder text like `[REDACTED_EMAIL_1]` can be
//! split across two `data:` events. The state machine buffers from `[`
//! to `]` (with a max length safety bail-out) and only consults the
//! Vault when the window closes.

use crate::guard::deanonymize::MatchingStrategy;

/// Maximum bytes to buffer while inside a `[...]` window before giving up
/// and treating the buffered content as ordinary text.
const MAX_PLACEHOLDER_LEN: usize = 64;

enum State {
    Normal,
    InPlaceholder,
}

pub struct DeanonymizeStream {
    state: State,
    pending: String,
    /// Cached snapshot of the vault — the strategies we ship today
    /// (Exact, CaseInsensitive) only ever consult vault entries, so passing
    /// a snapshot in once at construction time avoids needing a `&Vault`
    /// reference inside the async stream where lifetimes are awkward.
    entries: Vec<(String, String)>,
    strategy: Box<dyn MatchingStrategy>,
}

impl DeanonymizeStream {
    pub fn new(entries: Vec<(String, String)>, strategy: Box<dyn MatchingStrategy>) -> Self {
        Self {
            state: State::Normal,
            pending: String::new(),
            entries,
            strategy,
        }
    }

    /// Feed the next slice of `delta.content` from the upstream LLM. Returns
    /// the bytes that are now safe to emit to the downstream client. Bytes
    /// that *might* belong to an unfinished placeholder are kept in `pending`
    /// for the next call.
    pub fn push(&mut self, text: &str) -> String {
        if self.entries.is_empty() {
            return text.to_string();
        }
        let mut emit = String::with_capacity(text.len());
        for ch in text.chars() {
            match self.state {
                State::Normal => {
                    if ch == '[' {
                        // Begin tentative placeholder window.
                        self.pending.clear();
                        self.pending.push(ch);
                        self.state = State::InPlaceholder;
                    } else {
                        emit.push(ch);
                    }
                }
                State::InPlaceholder => {
                    self.pending.push(ch);
                    if ch == ']' {
                        // Window closed. Try a vault lookup over the entire
                        // buffered placeholder.
                        let restored = self.strategy.restore(&self.pending, &self.entries);
                        emit.push_str(&restored);
                        self.pending.clear();
                        self.state = State::Normal;
                    } else if self.pending.len() >= MAX_PLACEHOLDER_LEN {
                        // Bail: this `[` was probably ordinary text. Flush
                        // the buffer verbatim and resume Normal.
                        emit.push_str(&self.pending);
                        self.pending.clear();
                        self.state = State::Normal;
                    }
                }
            }
        }
        emit
    }

    /// Flush any pending bytes. Called when the upstream stream has ended.
    /// Anything still inside an unclosed `[...]` is emitted verbatim.
    pub fn flush(&mut self) -> String {
        let out = std::mem::take(&mut self.pending);
        self.state = State::Normal;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::deanonymize::Exact;

    fn vault_entries() -> Vec<(String, String)> {
        vec![
            ("[REDACTED_EMAIL_1]".to_string(), "alice@x.com".to_string()),
            ("[REDACTED_SSN_1]".to_string(), "123-45-6789".to_string()),
        ]
    }

    fn stream() -> DeanonymizeStream {
        DeanonymizeStream::new(vault_entries(), Box::new(Exact))
    }

    #[test]
    fn restores_single_chunk_placeholder() {
        let mut s = stream();
        let out = s.push("your email is [REDACTED_EMAIL_1] thanks");
        let tail = s.flush();
        assert_eq!(out + &tail, "your email is alice@x.com thanks");
    }

    #[test]
    fn restores_placeholder_split_across_chunks() {
        let mut s = stream();
        let a = s.push("your email is [REDACT");
        let b = s.push("ED_EMAIL_1] please ");
        let c = s.push("respond");
        let tail = s.flush();
        let total = a + &b + &c + &tail;
        assert_eq!(total, "your email is alice@x.com please respond");
    }

    #[test]
    fn unclosed_window_at_eof_flushes_verbatim() {
        let mut s = stream();
        let a = s.push("trailing [REDACT");
        let tail = s.flush();
        assert_eq!(a + &tail, "trailing [REDACT");
    }

    #[test]
    fn bracket_followed_by_normal_text_does_not_break() {
        // Long-enough run of non-`]` bytes inside a window forces the buffer
        // to bail and emit verbatim once MAX_PLACEHOLDER_LEN is reached.
        let mut s = stream();
        let long = "[".to_string() + &"x".repeat(MAX_PLACEHOLDER_LEN);
        let out = s.push(&long);
        let tail = s.flush();
        assert!(
            (out.clone() + &tail).contains('['),
            "verbatim emission expected, got `{}{}`",
            out,
            tail
        );
    }

    #[test]
    fn unknown_placeholder_passes_through() {
        let mut s = stream();
        let out = s.push("found [REDACTED_PERSON_99] mystery");
        let tail = s.flush();
        assert_eq!(out + &tail, "found [REDACTED_PERSON_99] mystery");
    }

    #[test]
    fn empty_vault_returns_input_unchanged() {
        let mut s = DeanonymizeStream::new(vec![], Box::new(Exact));
        let out = s.push("anything [REDACTED_EMAIL_1] anywhere");
        let tail = s.flush();
        assert_eq!(out + &tail, "anything [REDACTED_EMAIL_1] anywhere");
    }

    #[test]
    fn multiple_placeholders_in_one_chunk() {
        let mut s = stream();
        let out = s.push("[REDACTED_EMAIL_1] and [REDACTED_SSN_1]");
        let tail = s.flush();
        assert_eq!(out + &tail, "alice@x.com and 123-45-6789");
    }

    #[test]
    fn placeholder_at_chunk_boundary_each_byte() {
        let mut s = stream();
        // Pathological case: deliver one byte at a time.
        let input = "x [REDACTED_EMAIL_1] y";
        let mut total = String::new();
        for ch in input.chars() {
            total.push_str(&s.push(&ch.to_string()));
        }
        total.push_str(&s.flush());
        assert_eq!(total, "x alice@x.com y");
    }
}
