//! `cct blame <path>` — which sessions discussed a file.
//!
//! The index has no notion of "this session touched that file": a transcript
//! mentions paths as ordinary text, in whatever form the agent typed them.
//! So blame searches for the basename — the one form every mention shares —
//! and then verifies each hit, because `search.rs` also occurs inside
//! `research.rsync` and inside prose that never meant a file at all.

use std::path::{Path, PathBuf};

/// The forms of one path worth recognising in a transcript, longest first.
#[derive(Clone, Debug)]
pub struct PathQuery {
    /// `search.rs` — what the index is queried for.
    pub basename: String,
    /// `index/search.rs`, `src/index/search.rs`, the absolute path — the forms
    /// that prove the mention is about *this* file and not a namesake.
    pub qualified: Vec<String>,
}

/// How strongly a piece of text points at the file.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Mention {
    /// The basename appears, but only as part of a longer word.
    None,
    /// `search.rs` on its own — could be any file with that name.
    Bare,
    /// `index/search.rs` or longer — this file.
    Qualified,
    /// The session changed the file: an Edit/Write call on it, or the
    /// `edited_text_file` record a Bash or python edit leaves behind.
    Edited,
}

impl PathQuery {
    /// Build the recognisable forms of `raw`. An existing path is resolved
    /// against the filesystem so `./search.rs` and a relative fragment both
    /// yield the absolute form; anything else is used as typed, which is what
    /// makes `cct blame src/index/search.rs` work for a file already deleted.
    pub fn new(raw: &str) -> Option<Self> {
        let trimmed = raw.trim().trim_end_matches('/');
        if trimmed.is_empty() {
            return None;
        }
        let as_path = Path::new(trimmed);
        let basename = as_path.file_name()?.to_string_lossy().into_owned();
        if basename.is_empty() {
            return None;
        }

        let absolute: Option<PathBuf> = std::fs::canonicalize(as_path).ok();
        let mut qualified: Vec<String> = Vec::new();
        if let Some(abs) = &absolute {
            collect_suffixes(abs, &mut qualified);
        }
        collect_suffixes(as_path, &mut qualified);

        // Longest first: the first form that matches is the most specific one.
        qualified.sort_by_key(|s| std::cmp::Reverse(s.len()));
        qualified.dedup();
        Some(Self {
            basename,
            qualified,
        })
    }

    /// True when `path` — the target of an edit — is this file rather than a
    /// namesake. With a qualified form in hand the edited path must end with
    /// it; for a bare filename the basename has to match exactly.
    pub fn is_same_file(&self, path: &str) -> bool {
        let lower = path.to_lowercase();
        if self.qualified.is_empty() {
            let name = lower.rsplit('/').next().unwrap_or(&lower);
            return name == self.basename.to_lowercase();
        }
        self.qualified.iter().any(|form| {
            let form = form.to_lowercase();
            lower == form || lower.ends_with(&format!("/{form}"))
        })
    }

    /// The strongest mention of the file anywhere in `text`.
    pub fn classify(&self, text: &str) -> Mention {
        let lower = text.to_lowercase();
        for form in &self.qualified {
            if lower.contains(&form.to_lowercase()) {
                return Mention::Qualified;
            }
        }
        if mentions_standalone(&lower, &self.basename.to_lowercase()) {
            return Mention::Bare;
        }
        Mention::None
    }
}

/// Every `dir/.../basename` tail of `path` with at least one directory, so a
/// mention can be recognised however deeply the transcript qualified it.
fn collect_suffixes(path: &Path, out: &mut Vec<String>) {
    let parts: Vec<String> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .filter(|s| !s.is_empty() && s != "/" && s != "." && s != "..")
        .collect();
    if parts.len() < 2 {
        return;
    }
    for start in 0..parts.len() - 1 {
        out.push(parts[start..].join("/"));
    }
}

/// True when `needle` appears in `haystack` as its own token — bounded by
/// anything that is not a word character, so `search.rs` matches in
/// `src/search.rs:12` and in `"search.rs"` but not inside `research.rsync`.
/// Both arguments must already be lowercase.
fn mentions_standalone(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let bytes = haystack.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = haystack[from..].find(needle) {
        let start = from + rel;
        let end = start + needle.len();
        let before_ok = start == 0 || !is_word_byte(bytes[start - 1]);
        // A trailing dot is punctuation ("see search.rs.") but a trailing
        // letter is a different name ("search.rsync").
        let after_ok = end == bytes.len() || !is_word_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = start + needle.len().max(1);
        if from >= haystack.len() {
            break;
        }
    }
    false
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || !b.is_ascii()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(raw: &str) -> PathQuery {
        PathQuery::new(raw).expect("path query")
    }

    #[test]
    fn a_bare_basename_is_weaker_than_a_qualified_one() {
        let q = query("src/index/search.rs");
        assert_eq!(q.basename, "search.rs");
        assert_eq!(
            q.classify("rewrote src/index/search.rs to score sessions"),
            Mention::Qualified
        );
        assert_eq!(q.classify("touched index/search.rs"), Mention::Qualified);
        assert_eq!(q.classify("search.rs got slower"), Mention::Bare);
    }

    #[test]
    fn an_edit_counts_only_for_this_file() {
        let q = query("src/index/search.rs");
        assert!(q.is_same_file("/Users/x/cct/src/index/search.rs"));
        assert!(!q.is_same_file("/Users/x/cct/src/other/search.rs"), "a namesake elsewhere");
        assert!(!q.is_same_file("/Users/x/cct/src/index/research.rs"));

        // A filename that does not exist here stays bare: any file of that
        // name counts. (An existing one resolves to its absolute path, and
        // only edits to that very file count.)
        let bare = query("NoSuchFile.toml");
        assert!(bare.is_same_file("/Users/x/any/NoSuchFile.toml"));
        assert!(!bare.is_same_file("/Users/x/any/NoSuchFile.toml.bak"));
        let here = query("Cargo.toml");
        assert!(!here.is_same_file("/Users/x/elsewhere/Cargo.toml"));
        assert!(Mention::Edited > Mention::Qualified);
    }

    #[test]
    fn a_longer_name_is_not_a_mention() {
        let q = query("search.rs");
        assert_eq!(q.classify("ran research.rsync overnight"), Mention::None);
        assert_eq!(q.classify("mysearch.rs is unrelated"), Mention::None);
        // Punctuation around the name is fine.
        assert_eq!(q.classify("see \"search.rs\":12 for it"), Mention::Bare);
        assert_eq!(q.classify("edited search.rs."), Mention::Bare);
    }

    #[test]
    fn suffixes_are_longest_first_and_deduped() {
        let q = query("a/b/c/file.txt");
        assert_eq!(
            q.qualified.first().map(String::as_str),
            Some("a/b/c/file.txt")
        );
        assert!(q.qualified.iter().any(|s| s == "c/file.txt"));
        let mut sorted = q.qualified.clone();
        sorted.dedup();
        assert_eq!(sorted.len(), q.qualified.len(), "no duplicate forms");
    }

    #[test]
    fn a_bare_filename_has_no_qualified_form() {
        let q = query("Cargo.toml");
        assert!(q.qualified.is_empty() || q.qualified.iter().all(|s| s.contains('/')));
        assert_eq!(q.classify("bumped Cargo.toml"), Mention::Bare);
    }
}
