//! Deterministic output truncation.
//!
//! Tool results go back into the prompt, so the same output must always be cut
//! the same way. Over the cap we keep the first and last bytes and say exactly
//! how many were dropped in between. Cuts never split a multibyte character.

/// Output at or under this many bytes is returned unchanged.
pub const MAX_OUTPUT_BYTES: usize = HEAD_BYTES + TAIL_BYTES;
/// Bytes kept from the start of an over-long output.
pub const HEAD_BYTES: usize = 8 * 1024;
/// Bytes kept from the end of an over-long output.
pub const TAIL_BYTES: usize = 8 * 1024;

/// The exact text placed between the kept head and tail.
fn marker(omitted: usize) -> String {
    format!("\n[... {omitted} bytes omitted ...]\n")
}

/// Truncates `text` to head + marker + tail when it is over the cap.
pub fn truncate(text: &str) -> String {
    truncate_with(text, HEAD_BYTES, TAIL_BYTES)
}

fn truncate_with(text: &str, head: usize, tail: usize) -> String {
    if text.len() <= head + tail {
        return text.to_owned();
    }
    // Rust note: indexing a `&str` by byte position panics if it lands in the
    // middle of a multibyte character, so we nudge each cut to a character
    // boundary: the head backwards, the tail forwards. Both loops end because
    // position 0 and the end of the text are always boundaries.
    let mut head_end = head;
    while !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = text.len() - tail;
    while !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let omitted = tail_start - head_end;
    format!(
        "{}{}{}",
        &text[..head_end],
        marker(omitted),
        &text[tail_start..]
    )
}

/// Collects a stream of bytes while remembering only the first `HEAD_BYTES` and
/// the last `TAIL_BYTES`, so a command that prints gigabytes cannot exhaust
/// memory. `finish` gives the same text `truncate` would give for the whole
/// output.
//
// Rust note: `#[derive(Default)]` gives `OutputCapture::default()` with an
// empty `Vec` for each list and `0` for the counter.
#[derive(Default)]
pub(crate) struct OutputCapture {
    head: Vec<u8>,
    tail: Vec<u8>,
    total: usize,
}

impl OutputCapture {
    pub(crate) fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len();
        let mut rest = chunk;
        // Fill the head first; whatever does not fit goes to the tail.
        if self.head.len() < HEAD_BYTES {
            let take = (HEAD_BYTES - self.head.len()).min(rest.len());
            self.head.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
        }
        self.tail.extend_from_slice(rest);
        // Keep the tail from growing without bound: trim to the last
        // TAIL_BYTES whenever it reaches twice that.
        if self.tail.len() > 2 * TAIL_BYTES {
            let cut = self.tail.len() - TAIL_BYTES;
            self.tail.drain(..cut);
        }
    }

    pub(crate) fn finish(self) -> String {
        if self.total <= MAX_OUTPUT_BYTES {
            // Nothing was dropped, and head + tail are contiguous.
            let mut all = self.head;
            all.extend_from_slice(&self.tail);
            return String::from_utf8_lossy(&all).into_owned();
        }
        // The head may end part-way through a character: drop the incomplete
        // trailing bytes (only that exact case; genuinely invalid bytes are
        // left for the lossy conversion below).
        let head_len = match std::str::from_utf8(&self.head) {
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            _ => self.head.len(),
        };
        // The tail may start part-way through a character: skip continuation
        // bytes (those that look like 0b10xxxxxx). A character has at most 3.
        let mut tail_start = self.tail.len().saturating_sub(TAIL_BYTES);
        let tail_keep_from = tail_start;
        while tail_start < tail_keep_from + 3
            && self.tail.get(tail_start).is_some_and(|b| b & 0xC0 == 0x80)
        {
            tail_start += 1;
        }
        let tail = &self.tail[tail_start..];
        let omitted = self.total - head_len - tail.len();
        format!(
            "{}{}{}",
            String::from_utf8_lossy(&self.head[..head_len]),
            marker(omitted),
            String::from_utf8_lossy(tail)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_the_cap_is_unchanged() {
        let text = "a".repeat(MAX_OUTPUT_BYTES);
        assert_eq!(truncate(&text), text);
        assert_eq!(truncate(""), "");
    }

    #[test]
    fn over_the_cap_keeps_head_marker_tail_with_exact_count() {
        let text = format!(
            "{}{}{}",
            "h".repeat(HEAD_BYTES),
            "m".repeat(1000),
            "t".repeat(TAIL_BYTES)
        );
        let expected = format!(
            "{}\n[... 1000 bytes omitted ...]\n{}",
            "h".repeat(HEAD_BYTES),
            "t".repeat(TAIL_BYTES)
        );
        assert_eq!(truncate(&text), expected);
    }

    #[test]
    fn one_byte_over_the_cap_drops_exactly_one_byte() {
        let text = "x".repeat(MAX_OUTPUT_BYTES + 1);
        assert!(truncate(&text).contains("[... 1 bytes omitted ...]"));
    }

    #[test]
    fn multibyte_text_is_never_split() {
        // "é" is 2 bytes, "日" is 3, "🦀" is 4. Shifting the start by 0..4 bytes
        // moves the cut points through every position within each character.
        for shift in 0..4 {
            let text = format!("{}{}", "z".repeat(shift), "éé日日🦀🦀".repeat(3000));
            let out = truncate(&text);
            // If a character had been split, the slice above would have panicked,
            // and the output could not be valid text containing the marker.
            assert!(out.contains("bytes omitted"));
            assert!(out.len() < text.len());
        }
    }

    #[test]
    fn same_input_gives_identical_output() {
        let text = "line\n".repeat(10_000);
        assert_eq!(truncate(&text), truncate(&text));
    }

    #[test]
    fn capture_matches_truncate_for_any_chunking() {
        let text = "héllo 日本語 🦀 ".repeat(5000);
        let expected = truncate(&text);
        for chunk_size in [1, 7, 100, 4096, 100_000] {
            let mut capture = OutputCapture::default();
            for chunk in text.as_bytes().chunks(chunk_size) {
                capture.push(chunk);
            }
            assert_eq!(capture.finish(), expected, "chunk size {chunk_size}");
        }
    }

    #[test]
    fn capture_of_short_output_is_unchanged() {
        let mut capture = OutputCapture::default();
        capture.push(b"hello ");
        capture.push(b"world");
        assert_eq!(capture.finish(), "hello world");
    }
}
