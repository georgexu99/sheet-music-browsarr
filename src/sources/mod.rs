use serde::Serialize;

pub mod imslp;

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub source: String,
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub external_url: String,
}
