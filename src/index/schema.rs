use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::Result;
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, INDEXED, STORED, STRING,
};
use tantivy::tokenizer::{
    LowerCaser, NgramTokenizer, SimpleTokenizer, TextAnalyzer, Token, TokenFilter, TokenStream,
    Tokenizer,
};
use tantivy::Index;

pub const WORD_TOKENIZER: &str = "cct_word";
pub const NGRAM_TOKENIZER: &str = "cct_ngram3";

/// Word-index tokens are truncated to this many characters at index time.
///
/// Measured over a 300-file sample of real logs, tokens of 32+ chars are 30% of
/// the vocabulary and 68% of its bytes, and 88% of them occur exactly once:
/// base64 blobs, hashes and ids, not words anyone types. Keeping only their
/// first 32 chars shrinks the term dictionary without making them unfindable —
/// a query for the full string is truncated the same way, matches the same
/// term, and the snippet pass then verifies the whole string against the file,
/// so two blobs sharing a prefix cannot pass as each other.
///
/// Dropping them outright was the first attempt, on the theory that the trigram
/// index would answer such queries as substrings. It does not: a 512 KB chunk
/// contains nearly every trigram, so an AND over them barely narrows anything
/// and the one chunk that holds the string never reaches the candidate limit.
///
/// Counted in chars, not bytes: `RemoveLongFilter` measures UTF-8 length, which
/// would halve the budget for Cyrillic and cut real words in half.
pub const WORD_TOKEN_MAX_CHARS: usize = 32;

/// True when `token` reaches the word index only as its truncated prefix.
pub fn is_oversized_word_token(token: &str) -> bool {
    token.chars().count() > WORD_TOKEN_MAX_CHARS
}

/// The form an oversized token takes in the word index — and the form a query
/// for it must take to match.
pub fn truncate_word_token(token: &str) -> &str {
    match token.char_indices().nth(WORD_TOKEN_MAX_CHARS) {
        Some((byte_idx, _)) => &token[..byte_idx],
        None => token,
    }
}

/// Truncates tokens above `max_chars`. See [`WORD_TOKEN_MAX_CHARS`].
#[derive(Clone)]
pub struct TruncateFilter {
    max_chars: usize,
}

impl TruncateFilter {
    pub fn limit(max_chars: usize) -> Self {
        Self { max_chars }
    }
}

impl TokenFilter for TruncateFilter {
    type Tokenizer<T: Tokenizer> = TruncateWrapper<T>;

    fn transform<T: Tokenizer>(self, inner: T) -> Self::Tokenizer<T> {
        TruncateWrapper {
            max_chars: self.max_chars,
            inner,
        }
    }
}

#[derive(Clone)]
pub struct TruncateWrapper<T: Tokenizer> {
    max_chars: usize,
    inner: T,
}

impl<T: Tokenizer> Tokenizer for TruncateWrapper<T> {
    type TokenStream<'a> = TruncateStream<T::TokenStream<'a>>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        TruncateStream {
            max_chars: self.max_chars,
            tail: self.inner.token_stream(text),
        }
    }
}

pub struct TruncateStream<T> {
    max_chars: usize,
    tail: T,
}

impl<T: TokenStream> TokenStream for TruncateStream<T> {
    fn advance(&mut self) -> bool {
        if !self.tail.advance() {
            return false;
        }
        let token = self.tail.token_mut();
        if let Some((byte_idx, _)) = token.text.char_indices().nth(self.max_chars) {
            token.text.truncate(byte_idx);
        }
        true
    }

    fn token(&self) -> &Token {
        self.tail.token()
    }

    fn token_mut(&mut self) -> &mut Token {
        self.tail.token_mut()
    }
}

/// The analyzer behind [`WORD_TOKENIZER`]. Shared by index writes and query
/// parsing so a query is tokenized exactly like the text it searches — in
/// particular, an oversized token disappears from both sides.
pub fn word_analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(LowerCaser)
        .filter(TruncateFilter::limit(WORD_TOKEN_MAX_CHARS))
        .build()
}

/// The analyzer behind [`NGRAM_TOKENIZER`], used by the trigram and path indexes.
pub fn ngram_analyzer() -> Result<TextAnalyzer> {
    Ok(TextAnalyzer::builder(NgramTokenizer::new(3, 3, false)?)
        .filter(LowerCaser)
        .build())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum IndexKind {
    Word,
    Trigram,
    Path,
}

impl IndexKind {
    pub fn as_str(self) -> &'static str {
        match self {
            IndexKind::Word => "word",
            IndexKind::Trigram => "trigram",
            IndexKind::Path => "path",
        }
    }

    pub fn tokenizer(self) -> &'static str {
        match self {
            IndexKind::Word => WORD_TOKENIZER,
            IndexKind::Trigram | IndexKind::Path => NGRAM_TOKENIZER,
        }
    }

    /// What the postings carry. `Basic` stores doc ids only, which leaves BM25
    /// with nothing but document length — a one-line session then outranks the
    /// long investigation that actually discussed the term. Word postings
    /// therefore keep term frequencies; the ngram indexes do not, because a
    /// trigram's count says little and their postings are the bulky ones.
    ///
    /// Word postings also keep positions, so a multi-word query runs as a
    /// phrase. Without them it is an AND of words inside a 512 KB chunk:
    /// "code only adds" then matches most chunks of the corpus, and the five
    /// that hold the phrase drown among candidates the snippet pass rejects.
    pub fn record_option(self) -> IndexRecordOption {
        match self {
            IndexKind::Word => IndexRecordOption::WithFreqsAndPositions,
            IndexKind::Trigram | IndexKind::Path => IndexRecordOption::Basic,
        }
    }

    pub fn all() -> BTreeSet<IndexKind> {
        [IndexKind::Word, IndexKind::Trigram, IndexKind::Path]
            .into_iter()
            .collect()
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "word" => Some(IndexKind::Word),
            "trigram" => Some(IndexKind::Trigram),
            "path" => Some(IndexKind::Path),
            _ => None,
        }
    }
}

impl FromStr for IndexKind {
    type Err = ();

    fn from_str(raw: &str) -> std::result::Result<Self, Self::Err> {
        Self::parse(raw).ok_or(())
    }
}

#[derive(Clone, Copy)]
pub struct Fields {
    pub file_id: Field,
    pub chunk_id: Field,
    pub project_id: Field,
    pub path: Field,
    pub start_offset: Field,
    pub end_offset: Field,
    pub text: Field,
}

pub fn schema_and_fields(kind: IndexKind) -> (Schema, Fields) {
    let mut builder = Schema::builder();
    let file_id = builder.add_u64_field("file_id", INDEXED | STORED);
    let chunk_id = builder.add_u64_field("chunk_id", STORED);
    let project_id = builder.add_text_field("project_id", STRING | STORED);
    let path = builder.add_text_field("path", TextOptions::default().set_stored());
    let start_offset = builder.add_u64_field("start_offset", STORED);
    let end_offset = builder.add_u64_field("end_offset", STORED);
    let text_indexing = TextFieldIndexing::default()
        .set_tokenizer(kind.tokenizer())
        .set_index_option(kind.record_option());
    let text = builder.add_text_field(
        "text",
        TextOptions::default().set_indexing_options(text_indexing),
    );
    (
        builder.build(),
        Fields {
            file_id,
            chunk_id,
            project_id,
            path,
            start_offset,
            end_offset,
            text,
        },
    )
}

pub fn fields_from_schema(schema: &Schema) -> Result<Fields> {
    Ok(Fields {
        file_id: schema.get_field("file_id")?,
        chunk_id: schema.get_field("chunk_id")?,
        project_id: schema.get_field("project_id")?,
        path: schema.get_field("path")?,
        start_offset: schema.get_field("start_offset")?,
        end_offset: schema.get_field("end_offset")?,
        text: schema.get_field("text")?,
    })
}

pub fn register_tokenizers(index: &Index) -> Result<()> {
    index.tokenizers().register(WORD_TOKENIZER, word_analyzer());
    index
        .tokenizers()
        .register(NGRAM_TOKENIZER, ngram_analyzer()?);
    Ok(())
}

pub fn subindex_dir(root: &Path, kind: IndexKind) -> PathBuf {
    root.join(kind.as_str())
}

pub fn delta_dir(root: &Path) -> PathBuf {
    root.join("delta")
}
