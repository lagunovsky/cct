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

    pub fn stop(mut self) {
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

/// Truncate to at most `n` display columns, passing ANSI escapes through unmodified.
pub fn trunc_to_width(s: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    let mut out = String::with_capacity(s.len());
    let mut count = 0usize;
    let mut in_esc = false;
    for ch in s.chars() {
        if in_esc {
            out.push(ch);
            if ch == 'm' {
                in_esc = false;
            }
            continue;
        }
        if ch == '\x1b' {
            out.push(ch);
            in_esc = true;
            continue;
        }
        if count >= n {
            out.push('…');
            return out;
        }
        out.push(ch);
        count += 1;
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
}
