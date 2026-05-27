use async_trait::async_trait;
use serde::Serialize;

pub mod imslp;
pub mod mutopia;

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    /// Stable identifier of the source this result came from (e.g., "imslp",
    /// "mutopia"). Matches the value returned by `Source::id`.
    pub source: String,
    /// Source-specific id for the work. URL-safe (no slashes); each source
    /// is responsible for emitting ids that round-trip through path params.
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    /// Public viewer URL on the source (e.g., the IMSLP wiki page). Used in
    /// the search results UI as a "View on <source>" link and as a graceful
    /// fallback when proxy-streaming the PDF fails.
    pub external_url: String,
}

/// A backend that knows how to search for sheet music and fetch PDFs for a
/// specific catalog (IMSLP, Mutopia, MuseScore, etc.). Implementations are
/// stored as `Arc<dyn Source>` in `AppState::sources` and registered at
/// startup in `main.rs`.
#[async_trait]
pub trait Source: Send + Sync {
    /// Stable URL-safe identifier (e.g., "imslp"). Appears in routes like
    /// `/pdf/{id}/{...}` and in `SearchResult::source`.
    fn id(&self) -> &'static str;

    /// Human-readable name for the UI (e.g., "IMSLP", "Mutopia Project").
    fn display_name(&self) -> &'static str;

    /// Public viewer URL on the source for a given work id. Used as a
    /// fallback when `fetch_pdf_bytes` cannot resolve a real PDF.
    fn external_url(&self, id: &str) -> String;

    async fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<SearchResult>>;

    /// Download a work's PDF bytes, refusing anything larger than
    /// `max_bytes`. The trait does buffered fetch (not streaming) so the
    /// caller can both serve to clients and pass to lettre as an attachment
    /// from a single code path. Sources that have to do multi-step
    /// resolution (token mint, disclaimer follow, etc.) hide that here.
    async fn fetch_pdf_bytes(&self, id: &str, max_bytes: usize) -> anyhow::Result<Vec<u8>>;
}
