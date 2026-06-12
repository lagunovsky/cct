//! ANSI colors, spinner, and small terminal helpers.

use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const GRAY: &str = "\x1b[90m";
pub const CYAN: &str = "\x1b[36m";
pub const GREEN: &str = "\x1b[32m";
pub const RED: &str = "\x1b[31m";

pub fn stderr_is_tty() -> bool {
    io::stderr().is_terminal()
}

pub fn stdout_is_tty() -> bool {
    io::stdout().is_terminal()
}

pub fn stdin_is_tty() -> bool {
    io::stdin().is_terminal()
}

/// Lightweight spinner that writes to stderr. Active only on TTYs; otherwise a no-op.
pub struct Spinner {
    msg: Arc<Mutex<String>>,
    done: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Spinner {
    pub fn start(initial: &str) -> Self {
        let msg = Arc::new(Mutex::new(initial.to_string()));
        let done = Arc::new(AtomicBool::new(false));
        let handle = if stderr_is_tty() {
            let msg = Arc::clone(&msg);
            let done = Arc::clone(&done);
            Some(thread::spawn(move || {
                let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
                let mut i = 0usize;
                let mut stderr = io::stderr().lock();
                while !done.load(Ordering::Relaxed) {
                    let m = msg.lock().unwrap().clone();
                    let _ = write!(
                        stderr,
                        "\r\x1b[K{DIM}{frame} {m}{RESET}",
                        frame = frames[i % frames.len()],
                    );
                    let _ = stderr.flush();
                    i = i.wrapping_add(1);
                    thread::sleep(Duration::from_millis(80));
                }
                let _ = write!(stderr, "\r\x1b[K");
                let _ = stderr.flush();
            }))
        } else {
            None
        };
        Self { msg, done, handle }
    }

    pub fn set_message(&self, m: &str) {
        if let Ok(mut guard) = self.msg.lock() {
            *guard = m.to_string();
        }
    }

    pub fn stop(self) {
        // Drop does the actual cleanup; this name just marks intent.
    }
}

/// Stops the spinner thread even when the owner bails with `?` between
/// `start` and `stop` — otherwise it keeps drawing frames over the error.
impl Drop for Spinner {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Format bytes the way the Go tool used to (`B`, `K`, `M`, `G`).
pub fn fmt_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1}G", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1}M", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{}K", bytes / KB)
    } else {
        format!("{bytes}B")
    }
}

/// Highlight every case-insensitive occurrence of any of `queries` in `text`.
/// Empty queries are skipped. Used by fuzzy search to light up each token.
pub fn highlight_any(text: &str, queries: &[&str]) -> String {
    let (lower_text, spans) = lowercase_with_spans(text);
    let mut ranges = Vec::new();
    for q in queries {
        if q.is_empty() {
            continue;
        }
        let lower_q = q.to_lowercase();
        if lower_q.is_empty() {
            continue;
        }

        let mut lower_from = 0;
        while lower_from < lower_text.len() {
            let Some(idx) = lower_text[lower_from..].find(&lower_q) else {
                break;
            };
            let lower_start = lower_from + idx;
            let lower_end = lower_start + lower_q.len();
            if let Some(range) = original_span_for_lower_match(&spans, lower_start, lower_end) {
                ranges.push(range);
            }
            lower_from = lower_end;
        }
    }

    if ranges.is_empty() {
        return text.to_string();
    }

    ranges.sort_unstable_by_key(|(start, end)| (*start, *end));
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        if start >= end {
            continue;
        }
        if let Some((_, last_end)) = merged.last_mut() {
            if start <= *last_end {
                *last_end = (*last_end).max(end);
                continue;
            }
        }
        merged.push((start, end));
    }

    let mut out =
        String::with_capacity(text.len() + merged.len() * (RED.len() + BOLD.len() + RESET.len()));
    let mut original_from = 0;
    for (start, end) in merged {
        out.push_str(&text[original_from..start]);
        out.push_str(RED);
        out.push_str(BOLD);
        out.push_str(&text[start..end]);
        out.push_str(RESET);
        original_from = end;
    }
    out.push_str(&text[original_from..]);
    out
}

/// Highlight every case-insensitive occurrence of `query` inside `text` with bold red.
pub fn highlight(text: &str, query: &str) -> String {
    if query.is_empty() {
        return text.to_string();
    }
    let (lower_text, spans) = lowercase_with_spans(text);
    let lower_q = query.to_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut lower_from = 0;
    let mut original_from = 0;
    while lower_from < lower_text.len() {
        if let Some(idx) = lower_text[lower_from..].find(&lower_q) {
            let lower_start = lower_from + idx;
            let lower_end = lower_start + lower_q.len();
            let Some((original_start, original_end)) =
                original_span_for_lower_match(&spans, lower_start, lower_end)
            else {
                lower_from = lower_end;
                continue;
            };
            out.push_str(&text[original_from..original_start]);
            out.push_str(RED);
            out.push_str(BOLD);
            out.push_str(&text[original_start..original_end]);
            out.push_str(RESET);
            lower_from = lower_end;
            original_from = original_end;
        } else {
            break;
        }
    }
    out.push_str(&text[original_from..]);
    out
}

#[derive(Clone, Copy)]
struct LowerSpan {
    lower_start: usize,
    lower_end: usize,
    original_start: usize,
    original_end: usize,
}

fn lowercase_with_spans(text: &str) -> (String, Vec<LowerSpan>) {
    let mut lower = String::with_capacity(text.len());
    let mut spans = Vec::new();
    for (original_start, ch) in text.char_indices() {
        let original_end = original_start + ch.len_utf8();
        let lower_start = lower.len();
        for folded in ch.to_lowercase() {
            lower.push(folded);
        }
        spans.push(LowerSpan {
            lower_start,
            lower_end: lower.len(),
            original_start,
            original_end,
        });
    }
    (lower, spans)
}

fn original_span_for_lower_match(
    spans: &[LowerSpan],
    lower_start: usize,
    lower_end: usize,
) -> Option<(usize, usize)> {
    let mut original_start = None;
    let mut original_end = None;
    for span in spans {
        if span.lower_end <= lower_start {
            continue;
        }
        if span.lower_start >= lower_end {
            break;
        }
        if original_start.is_none() {
            original_start = Some(span.original_start);
        }
        original_end = Some(span.original_end);
    }
    Some((original_start?, original_end?))
}

/// Truncate to at most `n` display columns (wide chars count as 2), keeping
/// SGR style sequences, dropping all other escape/control bytes, and closing
/// any open style with RESET when the text is cut. The ellipsis is included
/// in the budget, so the result never exceeds `n` columns.
pub fn trunc_to_width(s: &str, n: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if n == 0 {
        return String::new();
    }
    let mut out = String::with_capacity(s.len());
    let mut width = 0usize;
    let mut styled = false;
    let mut truncated = false;
    // Widest prefix of `out` (bytes, styled-at-that-point) where '…' still fits.
    let mut cut = (0usize, false);
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            if chars.peek() == Some(&'[') {
                let mut seq = String::from("\x1b");
                seq.push(chars.next().unwrap());
                while matches!(chars.peek(), Some(c) if c.is_ascii_digit() || *c == ';') {
                    seq.push(chars.next().unwrap());
                }
                match chars.peek() {
                    // SGR (style) sequences pass through.
                    Some('m') => {
                        seq.push(chars.next().unwrap());
                        out.push_str(&seq);
                        styled = true;
                    }
                    // Any other CSI sequence is dropped wholesale.
                    Some(c) if c.is_ascii_alphabetic() => {
                        chars.next();
                    }
                    _ => {}
                }
            }
            // A bare ESC (or unfinished sequence) is dropped.
            continue;
        }
        if (ch as u32) < 32 || ch == '\u{7f}' {
            continue;
        }
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > n {
            truncated = true;
            break;
        }
        out.push(ch);
        width += ch_width;
        if width + 1 <= n {
            cut = (out.len(), styled);
        }
    }
    if truncated {
        let (bytes, cut_styled) = cut;
        out.truncate(bytes);
        if cut_styled {
            out.push_str(RESET);
        }
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_handles_unicode_lowercase_expansion() {
        let rendered = highlight("İstanbul pressure note", "İstanbul");
        assert!(rendered.contains("İstanbul"));
        assert!(rendered.contains(RED));
    }

    #[test]
    fn highlight_any_does_not_match_ansi_from_previous_tokens() {
        let rendered = highlight_any("foo 31", &["foo", "31"]);
        assert_eq!(
            rendered,
            format!("{RED}{BOLD}foo{RESET} {RED}{BOLD}31{RESET}")
        );
    }

    #[test]
    fn highlight_any_merges_overlapping_tokens() {
        let rendered = highlight_any("foobar", &["foo", "foobar"]);
        assert_eq!(rendered, format!("{RED}{BOLD}foobar{RESET}"));
    }

    #[test]
    fn trunc_counts_display_columns_not_chars() {
        // Four CJK chars are eight columns; with n=5 only two fit (4 cols)
        // plus the ellipsis.
        assert_eq!(trunc_to_width("漢字漢字", 5), "漢字…");
        // Exact fit is not truncated.
        assert_eq!(trunc_to_width("漢字", 4), "漢字");
    }

    #[test]
    fn trunc_result_never_exceeds_limit() {
        // "abcdef" at n=4 → 3 chars + ellipsis = exactly 4 columns.
        assert_eq!(trunc_to_width("abcdef", 4), "abc…");
        assert_eq!(trunc_to_width("abcd", 4), "abcd");
    }

    #[test]
    fn trunc_closes_open_style_on_cut() {
        let s = format!("{GREEN}{BOLD}long green title here{RESET}");
        let cut = trunc_to_width(&s, 6);
        assert!(cut.ends_with(&format!("{RESET}…")), "got {cut:?}");
    }

    #[test]
    fn trunc_drops_non_sgr_escapes_and_controls() {
        // A clear-screen CSI and a bare ESC must not leak through or eat text.
        assert_eq!(trunc_to_width("a\x1b[2Jb\x1bc\x07d", 10), "abcd");
    }
}
