//! Tantivy-backed full-text index split across word/trigram/path physical indexes.

pub mod build;
pub mod catalog;
pub mod compact;
pub mod schema;
pub mod search;
pub mod update;
pub mod writer;

pub use schema::IndexKind;
