use async_trait::async_trait;
use serde::Serialize;

pub mod health;
pub mod imslp;
pub mod musescore;
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
    /// Optional source-native preview image URL. Rendered as a card
    /// thumbnail. `None` falls back to a generic placeholder. Sources that
    /// can derive a thumbnail URL cheaply should populate this; sources
    /// that would need an extra HTTP roundtrip per result should leave it
    /// `None` to keep search latency bounded.
    pub thumbnail_url: Option<String>,
    /// Small, source-extracted metadata pills (pages count, instrumentation,
    /// key, etc.) rendered as gray context badges under the title. Sources
    /// populate whatever they can pull cheaply from the search response;
    /// missing values stay out of the Vec rather than rendering as "—".
    /// Per-result HTTP enrichment is explicitly not done here — search
    /// latency budget is tight.
    #[serde(default)]
    pub metadata: Vec<MetadataBadge>,
}

/// A small contextual metadata pill rendered below the title on a search
/// result card. `kind` carries the visual style; `label` is the rendered
/// text (already formatted, e.g. "12 pages", "C minor", "1823").
#[derive(Debug, Clone, Serialize)]
pub struct MetadataBadge {
    pub label: String,
    pub kind: BadgeKind,
}

/// Visual style for a `MetadataBadge`. Kept small on purpose — the badges
/// are context, not focus, so they all share a neutral gray palette in the
/// template today. The `kind` is preserved on the struct anyway so we can
/// re-skin per kind later (e.g. tint Difficulty by level) without changing
/// the source extractors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BadgeKind {
    Pages,
    Key,
    Year,
    Instrument,
    Difficulty,
    Generic,
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

    /// Optional lazy thumbnail resolver. Sources whose search response
    /// already carries a thumbnail URL (`SearchResult::thumbnail_url`)
    /// should leave this as the default Err — the route never reaches them.
    /// Sources that need an extra HTTP call to discover the thumbnail
    /// (e.g., IMSLP scrapes the wiki page) override this, and the
    /// `/thumbnail/{source}/{id}` route caches the result.
    async fn thumbnail_url(&self, _id: &str) -> anyhow::Result<String> {
        anyhow::bail!("source does not provide lazy thumbnails")
    }
}
