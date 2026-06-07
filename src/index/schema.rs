use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, INDEXED, STORED, STRING,
};
use tantivy::tokenizer::{LowerCaser, NgramTokenizer, SimpleTokenizer, TextAnalyzer};
use tantivy::Index;

pub const WORD_TOKENIZER: &str = "cct_word";
pub const NGRAM_TOKENIZER: &str = "cct_ngram3";

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

    pub fn all() -> BTreeSet<IndexKind> {
        [IndexKind::Word, IndexKind::Trigram, IndexKind::Path]
            .into_iter()
            .collect()
    }

    pub fn from_str(raw: &str) -> Option<Self> {
        match raw.trim() {
            "word" => Some(IndexKind::Word),
            "trigram" => Some(IndexKind::Trigram),
            "path" => Some(IndexKind::Path),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
pub struct Fields {
    pub file_id: Field,
    pub chunk_id: Field,
    pub project_id: Option<Field>,
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
        .set_index_option(IndexRecordOption::Basic);
    let text = builder.add_text_field(
        "text",
        TextOptions::default().set_indexing_options(text_indexing),
    );
    (
        builder.build(),
        Fields {
            file_id,
            chunk_id,
            project_id: Some(project_id),
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
        project_id: schema.get_field("project_id").ok(),
        path: schema.get_field("path")?,
        start_offset: schema.get_field("start_offset")?,
        end_offset: schema.get_field("end_offset")?,
        text: schema.get_field("text")?,
    })
}

pub fn register_tokenizers(index: &Index) -> Result<()> {
    let word = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(LowerCaser)
        .build();
    let trigram = TextAnalyzer::builder(NgramTokenizer::new(3, 3, false)?)
        .filter(LowerCaser)
        .build();
    index.tokenizers().register(WORD_TOKENIZER, word);
    index.tokenizers().register(NGRAM_TOKENIZER, trigram);
    Ok(())
}

pub fn subindex_dir(root: &Path, kind: IndexKind) -> PathBuf {
    root.join(kind.as_str())
}

pub fn delta_dir(root: &Path) -> PathBuf {
    root.join("delta")
}
