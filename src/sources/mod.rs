use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub mod flaresolverr;
pub mod health;
pub mod imslp;
pub mod musescore;
pub mod mutopia;

/// User-selectable instrument filter. Each source decides how to apply it:
/// Mutopia maps to its native `Instrument` CGI facet, MuseScore to its
/// `&instrument=` URL param, IMSLP filters post-hoc by checking title and
/// description text against per-instrument keywords (the OpenSearch API has
/// no facet).
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub enum Instrument {
    Piano,
    Guitar,
    Violin,
    Viola,
    Cello,
    Flute,
    Clarinet,
    Voice,
    Choral,
    Organ,
}

impl Instrument {
    /// URL-safe lowercase identifier. Matches MuseScore's `&instrument=`
    /// values where applicable.
    pub fn slug(&self) -> &'static str {
        match self {
            Instrument::Piano => "piano",
            Instrument::Guitar => "guitar",
            Instrument::Violin => "violin",
            Instrument::Viola => "viola",
            Instrument::Cello => "cello",
            Instrument::Flute => "flute",
            Instrument::Clarinet => "clarinet",
            Instrument::Voice => "voice",
            Instrument::Choral => "choral",
            Instrument::Organ => "organ",
        }
    }

    /// Title-case label for the dropdown.
    pub fn display(&self) -> &'static str {
        match self {
            Instrument::Piano => "Piano",
            Instrument::Guitar => "Guitar",
            Instrument::Violin => "Violin",
            Instrument::Viola => "Viola",
            Instrument::Cello => "Cello",
            Instrument::Flute => "Flute",
            Instrument::Clarinet => "Clarinet",
            Instrument::Voice => "Voice",
            Instrument::Choral => "Choral",
            Instrument::Organ => "Organ",
        }
    }

    /// Mutopia's CGI `Instrument` field is case-sensitive and expects the
    /// title-case form (e.g. "Piano", "Guitar"). `Choral` maps to Mutopia's
    /// "Choir" entry.
    pub fn mutopia_value(&self) -> &'static str {
        match self {
            Instrument::Choral => "Choir",
            _ => self.display(),
        }
    }

    pub fn from_slug(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|i| i.slug() == s)
    }

    /// Fixed display order for the UI dropdown.
    pub const ALL: &'static [Instrument] = &[
        Instrument::Piano,
        Instrument::Guitar,
        Instrument::Violin,
        Instrument::Viola,
        Instrument::Cello,
        Instrument::Flute,
        Instrument::Clarinet,
        Instrument::Voice,
        Instrument::Choral,
        Instrument::Organ,
    ];
}

/// User-selectable difficulty filter. MuseScore is the only catalog that
/// exposes a complexity signal (1=Beginner, 2=Intermediate, 3=Advanced
/// on the per-score JSON). Filtering is post-hoc in the route layer
/// rather than pushed down to each source — there's no upstream facet
/// to map to and the data is already on `SearchResult`.
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub enum Difficulty {
    Beginner,
    Intermediate,
    Advanced,
}

impl Difficulty {
    pub fn slug(&self) -> &'static str {
        match self {
            Difficulty::Beginner => "beginner",
            Difficulty::Intermediate => "intermediate",
            Difficulty::Advanced => "advanced",
        }
    }
    pub fn display(&self) -> &'static str {
        match self {
            Difficulty::Beginner => "Beginner",
            Difficulty::Intermediate => "Intermediate",
            Difficulty::Advanced => "Advanced",
        }
    }
    pub fn from_slug(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|d| d.slug() == s)
    }
    /// Inverse of MuseScore's `complexity` field (1/2/3 → enum).
    pub fn from_complexity(c: u8) -> Option<Self> {
        match c {
            1 => Some(Difficulty::Beginner),
            2 => Some(Difficulty::Intermediate),
            3 => Some(Difficulty::Advanced),
            _ => None,
        }
    }
    pub const ALL: &'static [Difficulty] = &[
        Difficulty::Beginner,
        Difficulty::Intermediate,
        Difficulty::Advanced,
    ];
}

/// Distinguishes publisher-engraved "official" content (Hal Leonard,
/// ArrangeMe) from community uploads on MuseScore. IMSLP and Mutopia
/// are inherently community engravings of PD works — they pass
/// "Community" but are excluded from "Official".
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub enum ScoreType {
    Official,
    Community,
}

impl ScoreType {
    pub fn slug(&self) -> &'static str {
        match self {
            ScoreType::Official => "official",
            ScoreType::Community => "community",
        }
    }
    pub fn display(&self) -> &'static str {
        match self {
            ScoreType::Official => "Official",
            ScoreType::Community => "Community",
        }
    }
    pub fn from_slug(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|t| t.slug() == s)
    }
    pub const ALL: &'static [ScoreType] = &[ScoreType::Official, ScoreType::Community];
}

/// Optional facets the search route forwards to each source. `instrument`
/// is pushed down to upstream sources (Mutopia/MuseScore have native
/// facets, IMSLP filters post-hoc on title/desc); the other three are
/// applied centrally in the route handler on the assembled, deduped
/// result set since the data is already on every `SearchResult`.
#[derive(Debug, Clone, Default, Hash, Eq, PartialEq)]
pub struct SearchFilters {
    pub instrument: Option<Instrument>,
    pub difficulty: Option<Difficulty>,
    /// True when the user wants only known-PD content. Conservative: a
    /// result with `is_public_domain == None` (i.e., MuseScore DOM-
    /// scrape fallback where we can't tell) does NOT pass when set.
    pub public_domain_only: bool,
    pub score_type: Option<ScoreType>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// 1 = Beginner, 2 = Intermediate, 3 = Advanced. MuseScore exposes
    /// this on community uploads via the `complexity` JSON field. IMSLP /
    /// Mutopia have no equivalent, so it's None for them.
    #[serde(default)]
    pub complexity: Option<u8>,
    /// True when we know the underlying work is in the public domain.
    /// IMSLP and Mutopia are catalog-wide PD so we hard-code Some(true);
    /// MuseScore exposes a per-score `is_public_domain` flag. None means
    /// "we don't know" (DOM-scrape fallback path on MuseScore can't tell).
    #[serde(default)]
    pub is_public_domain: Option<bool>,
    /// True when the score is an "official" publisher engraving on
    /// MuseScore (Hal Leonard, ArrangeMe, etc.) rather than a community
    /// upload. The community-vs-official distinction is MuseScore-only —
    /// IMSLP and Mutopia are inherently community engravings of PD works,
    /// so this stays None there.
    #[serde(default)]
    pub is_official: Option<bool>,
}

/// A small contextual metadata pill rendered below the title on a search
/// result card. `kind` carries the visual style; `label` is the rendered
/// text (already formatted, e.g. "12 pages", "C minor", "1823").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataBadge {
    pub label: String,
    pub kind: BadgeKind,
}

/// Visual style for a `MetadataBadge`. Kept small on purpose — the badges
/// are context, not focus, so they all share a neutral gray palette in the
/// template today. The `kind` is preserved on the struct anyway so we can
/// re-skin per kind later (e.g. tint Difficulty by level) without changing
/// the source extractors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)] // Key/Difficulty kept for sources that can extract them later.
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

    async fn search(
        &self,
        query: &str,
        filters: &SearchFilters,
        limit: usize,
    ) -> anyhow::Result<Vec<SearchResult>>;

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

    /// Inline-bytes alternative to `thumbnail_url`. Used by sources whose
    /// thumbnails are generated on demand (e.g., Mutopia rasterizing the
    /// first PDF page) rather than served by a third party CDN. Default
    /// implementation errors so the route can choose between redirect and
    /// inline-bytes paths.
    async fn thumbnail_bytes(&self, _id: &str) -> anyhow::Result<(Vec<u8>, &'static str)> {
        anyhow::bail!("source does not provide inline thumbnails")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instrument_slug_roundtrip() {
        for i in Instrument::ALL {
            assert_eq!(Instrument::from_slug(i.slug()), Some(*i));
        }
        assert_eq!(Instrument::from_slug("not-a-real-instrument"), None);
        assert_eq!(Instrument::from_slug(""), None);
    }

    #[test]
    fn mutopia_value_remaps_choral() {
        assert_eq!(Instrument::Choral.mutopia_value(), "Choir");
        assert_eq!(Instrument::Piano.mutopia_value(), "Piano");
    }
}
