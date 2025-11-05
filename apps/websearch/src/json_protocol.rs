//! JSON replies are schemas, not arbitrary byte buffers. Unknown fields remain
//! forward compatible; required fields, their types and UTF-8 are validated.
use serde::{de::DeserializeOwned, Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct SearchHit {
    pub doc: i64,
    pub score: f64,
    // The index can return a posting before its optional document metadata.
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub snippet: String,
}
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Health {
    pub terms: u64,
    pub postings: u64,
    pub queries: u64,
    pub docs: u64,
    pub segments: u64,
}
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct DocumentPage {
    pub hosts: u64,
    pub total_docs: u64,
    pub offset: u64,
    pub items: Vec<DocumentHost>,
}
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct DocumentHost {
    pub host: String,
    pub pages: u64,
    pub sample: String,
}
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct TermPage {
    pub total: u64,
    pub offset: u64,
    pub items: Vec<Term>,
}
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Term {
    pub term: String,
    pub df: u64,
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    if bytes.len() > parrot_remote::frame::MAX_FRAME_LEN as usize {
        return Err("JSON reply exceeds frame byte budget".into());
    }
    serde_json::from_slice(bytes).map_err(|error| format!("invalid JSON reply: {error}"))
}
