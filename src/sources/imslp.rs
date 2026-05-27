use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::cookie::Jar;
use reqwest::{Client, Url};
use scraper::{Html, Selector};

use super::{Instrument, SearchFilters, SearchResult, Source};

const USER_AGENT: &str = concat!(
    "sheet-music-browsarr/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/georgexu99/sheet-music-browsarr)"
);

#[derive(Clone)]
pub struct Imslp {
    http: Client,
}

impl Imslp {
    pub fn new() -> anyhow::Result<Self> {
        let jar = Arc::new(Jar::default());
        let base: Url = "https://imslp.org".parse().expect("static URL");
        for cookie in [
            "imslpdisclaim=true; Domain=.imslp.org; Path=/",
            "imslpdisclaimer=true; Domain=.imslp.org; Path=/",
            "imslpfileaccess=true; Domain=.imslp.org; Path=/",
            "imslp_disclaimer_accepted=1; Domain=.imslp.org; Path=/",
        ] {
            jar.add_cookie_str(cookie, &base);
        }

        let http = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(20))
            .gzip(true)
            .cookie_provider(jar)
            .build()?;
        Ok(Self { http })
    }

    /// Find a preview image for a work page by scraping the wiki page
    /// for the first `imslp.org/imglnks/.../*.{png,jpg,jpeg,gif}` URL.
    /// Used by the lazy `/thumbnail/imslp/:id` route; the resolved URL is
    /// cached in `AppState::thumbnail_cache` so each work is scraped once
    /// per 24h window per process.
    pub async fn find_thumbnail_url(&self, page_id: &str) -> anyhow::Result<String> {
        let page_url = format!("https://imslp.org/wiki/{}", page_id);
        let html = self
            .http
            .get(&page_url)
            .send()
            .await
            .context("imslp page fetch")?
            .error_for_status()
            .context("imslp page status")?
            .text()
            .await
            .context("imslp page body")?;

        scrape_thumbnail_image_url(&html)
            .ok_or_else(|| anyhow::anyhow!("no preview image found on {page_url}"))
    }

    /// Resolve a work page id to a direct PDF URL. Prefers the IMSLP CDN
    /// (`imslp.org/imglnks/.../*.pdf`); falls back to the
    /// `Special:ImagefromIndex/...` form and follows it once to extract the
    /// real CDN URL embedded in the disclaimer interstitial.
    async fn find_pdf_url(&self, page_id: &str) -> anyhow::Result<String> {
        let page_url = format!("https://imslp.org/wiki/{}", page_id);
        let html = self
            .http
            .get(&page_url)
            .send()
            .await
            .context("imslp page fetch")?
            .error_for_status()
            .context("imslp page status")?
            .text()
            .await
            .context("imslp page body")?;

        let candidate = {
            let doc = Html::parse_document(&html);
            let sel = Selector::parse("a").map_err(|e| anyhow::anyhow!("selector: {e}"))?;

            let mut direct_cdn: Option<String> = None;
            let mut disclaimer_gated: Option<String> = None;
            let mut other_pdf: Option<String> = None;

            for el in doc.select(&sel) {
                let Some(href) = el.value().attr("href") else {
                    continue;
                };
                let lower = href.to_lowercase();

                if direct_cdn.is_none() && lower.contains("/imglnks/") && lower.contains(".pdf") {
                    direct_cdn = Some(absolutize(href));
                } else if disclaimer_gated.is_none() && href.contains("Special:ImagefromIndex") {
                    disclaimer_gated = Some(absolutize(href));
                } else if other_pdf.is_none() && lower.ends_with(".pdf") {
                    other_pdf = Some(absolutize(href));
                }
            }

            direct_cdn
                .or(disclaimer_gated)
                .or(other_pdf)
                .ok_or_else(|| anyhow::anyhow!("no PDF link on {page_url}"))?
        };

        if candidate.to_lowercase().contains("/imglnks/") {
            return Ok(candidate);
        }

        match self.http.get(&candidate).send().await {
            Ok(resp) if resp.status().is_success() => {
                let ct = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_lowercase();
                if ct.starts_with("application/pdf") {
                    return Ok(candidate);
                }
                if let Ok(body) = resp.text().await {
                    if let Some(actual) = scrape_cdn_pdf_url(&body) {
                        return Ok(actual);
                    }
                }
            }
            _ => {}
        }

        Ok(candidate)
    }
}

#[async_trait]
impl Source for Imslp {
    fn id(&self) -> &'static str {
        "imslp"
    }

    fn display_name(&self) -> &'static str {
        "IMSLP"
    }

    fn external_url(&self, id: &str) -> String {
        format!("https://imslp.org/wiki/{}", id)
    }

    async fn search(
        &self,
        query: &str,
        filters: &SearchFilters,
        limit: usize,
    ) -> anyhow::Result<Vec<SearchResult>> {
        // IMSLP's OpenSearch API has no instrument facet. We keep the query
        // clean (polluting it with the instrument slug would degrade
        // MediaWiki's ranking) and instead filter in-memory after parsing —
        // see the retain() block below. When a filter is active we widen
        // the upstream limit so the filter has more candidates to choose
        // from before we truncate to the caller's limit. We also always
        // over-fetch by ~50% to absorb the composer-landing-page filter
        // (the loop below skips entries that aren't work pages, which
        // would otherwise leave the caller with fewer than `limit`).
        let upstream_limit = if filters.instrument.is_some() {
            limit.saturating_mul(4).max(20)
        } else {
            limit.saturating_add(limit / 2).max(15)
        };
        let resp = self
            .http
            .get("https://imslp.org/api.php")
            .query(&[
                ("action", "opensearch"),
                ("search", query),
                ("format", "json"),
                ("limit", &upstream_limit.to_string()),
                ("namespace", "0"),
            ])
            .send()
            .await
            .context("imslp opensearch request")?;
        let status = resp.status();
        if !status.is_success() {
            // Pull a short body snippet for the log — IMSLP often returns
            // a recognisable rate-limit / blocked-bot page.
            let body = resp.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(200).collect();
            anyhow::bail!("imslp opensearch HTTP {status}: {snippet}");
        }
        let resp: serde_json::Value = resp
            .json()
            .await
            .context("imslp opensearch json")?;

        let arr = resp.as_array().context("imslp opensearch: expected array")?;
        let titles = arr.get(1).and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let descs = arr.get(2).and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let urls = arr.get(3).and_then(|v| v.as_array()).cloned().unwrap_or_default();

        let mut results = Vec::with_capacity(titles.len());
        for (i, title) in titles.iter().enumerate() {
            let title = title.as_str().unwrap_or_default().to_string();
            if title.is_empty() {
                continue;
            }
            // Skip composer landing pages, lists, and other non-work
            // pages. These appear in OpenSearch results (the namespace=0
            // filter on the API call doesn't catch them — they're still
            // article-namespace, just not work pages) and can't be
            // resolved to a PDF by `find_pdf_url`, so clicking them
            // produces a `fetch_failed_redirect` to the IMSLP wiki.
            // IMSLP work pages are reliably titled with the
            // "(Lastname, Firstname)" composer suffix:
            //   "Symphony No.9, Op.125 (Beethoven, Ludwig van)"
            //   "Nocturne Op.9 No.2 (Chopin, Frédéric)"
            // Composer landing pages and lists don't have it
            // ("Chopin", "List of compositions by Chopin").
            if !looks_like_imslp_work_page(&title) {
                continue;
            }
            let url = urls
                .get(i)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let desc = descs
                .get(i)
                .and_then(|v| v.as_str())
                .map(String::from)
                .filter(|s| !s.is_empty());
            let id = page_id_from_url(&url).unwrap_or_else(|| title.clone());
            // IMSLP's OpenSearch API doesn't return thumbnails inline.
            // Point the browser at our own lazy /thumbnail/imslp/:id
            // endpoint; it scrapes the wiki page on first hit, caches the
            // resolved CDN URL for 24h, and redirects.
            let thumbnail_url = Some(format!("/thumbnail/imslp/{}", id));
            // IMSLP's OpenSearch endpoint only returns title/desc/url — no
            // structured metadata (pages/key/instrumentation). Enriching
            // each result would require a per-result wiki-page scrape and
            // blow the search latency budget; leave empty for now.
            results.push(SearchResult {
                source: "imslp".to_string(),
                id,
                title,
                description: desc,
                external_url: url,
                thumbnail_url,
                metadata: Vec::new(),
                // IMSLP has no difficulty rating signal in OpenSearch
                // results. The catalog is PD-focused so we can safely
                // hard-code is_public_domain. is_official doesn't apply
                // (IMSLP works are inherently community engravings of PD
                // works).
                complexity: None,
                is_public_domain: Some(true),
                is_official: None,
            });
        }

        // Post-hoc instrument filter: drop entries whose title/description
        // don't mention the instrument keyword. Lossy (a work *for* the
        // instrument that doesn't name it in title or snippet is missed —
        // e.g., a Chopin Mazurka under instrument=Piano), but precise (no
        // false positives), and keeps OpenSearch ranking on the clean
        // query. The right long-term fix is IMSLP MediaWiki category
        // traversal; deferred.
        if let Some(inst) = filters.instrument {
            let needles = instrument_keywords(inst);
            results.retain(|r| {
                let title_lc = r.title.to_lowercase();
                let desc_lc = r
                    .description
                    .as_deref()
                    .map(str::to_lowercase)
                    .unwrap_or_default();
                needles
                    .iter()
                    .any(|n| title_lc.contains(n) || desc_lc.contains(n))
            });
        }
        results.truncate(limit);

        Ok(results)
    }

    async fn thumbnail_url(&self, id: &str) -> anyhow::Result<String> {
        self.find_thumbnail_url(id).await
    }

    async fn fetch_pdf_bytes(&self, id: &str, max_bytes: usize) -> anyhow::Result<Vec<u8>> {
        let url = self.find_pdf_url(id).await?;
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .context("imslp pdf fetch")?
            .error_for_status()
            .context("imslp pdf status")?;

        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();
        anyhow::ensure!(
            ct.starts_with("application/pdf"),
            "upstream returned {ct:?} (not a PDF); IMSLP disclaimer may be blocking"
        );
        if let Some(len) = resp.content_length() {
            anyhow::ensure!(
                (len as usize) <= max_bytes,
                "PDF too large ({len} > {max_bytes})"
            );
        }

        let mut bytes = Vec::with_capacity(64 * 1024);
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if bytes.len() + chunk.len() > max_bytes {
                anyhow::bail!("PDF exceeds {max_bytes} bytes during streaming");
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

fn page_id_from_url(url: &str) -> Option<String> {
    url.split_once("/wiki/").map(|(_, rest)| rest.to_string())
}

/// Keywords used to recognise an instrument in IMSLP titles/descriptions
/// during post-hoc filtering. Most instruments match on their slug alone;
/// Voice/Choral expand to common formal labels (lieder, mass, etc.) and
/// Cello includes IMSLP's preferred "violoncello".
/// Discriminate IMSLP work pages from composer landing pages, lists,
/// and other non-PDF-resolvable entries that OpenSearch returns in
/// the same result set. IMSLP work pages reliably end with a
/// `(Lastname, Firstname)` composer suffix:
///   "Symphony No.9, Op.125 (Beethoven, Ludwig van)"      ← work
///   "Nocturne Op.9 No.2 (Chopin, Frédéric)"              ← work
///   "Chopin"                                              ← composer redirect
///   "List of compositions by Chopin"                      ← list page
/// We require a `,` inside a final parenthetical so plain
/// "(Frédéric Chopin)" or "(en français)" don't accidentally pass.
fn looks_like_imslp_work_page(title: &str) -> bool {
    let Some(open) = title.rfind('(') else {
        return false;
    };
    if !title.ends_with(')') {
        return false;
    }
    // Inside the parens, expect "Lastname, Firstname" — at minimum a
    // comma separator. Filters out non-composer parentheticals like
    // "(piano transcription)" or "(arr. Brahms)".
    title[open + 1..title.len() - 1].contains(", ")
}

fn instrument_keywords(inst: Instrument) -> &'static [&'static str] {
    match inst {
        Instrument::Piano => &["piano"],
        Instrument::Guitar => &["guitar"],
        Instrument::Violin => &["violin"],
        Instrument::Viola => &["viola"],
        Instrument::Cello => &["cello", "violoncello"],
        Instrument::Flute => &["flute"],
        Instrument::Clarinet => &["clarinet"],
        Instrument::Voice => &["voice", "vocal", "lied", "lieder", "song", "aria"],
        Instrument::Choral => &["choral", "choir", "chorus", "mass", "requiem", "motet"],
        Instrument::Organ => &["organ"],
    }
}

fn absolutize(href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        href.to_string()
    } else if let Some(stripped) = href.strip_prefix("//") {
        format!("https://{stripped}")
    } else {
        format!("https://imslp.org{href}")
    }
}

/// Pull a usable preview-image URL out of an IMSLP wiki page.
///
/// IMSLP serves preview images from two paths, **neither of which is**
/// `imslp.org/imglnks/` (that path is the PDF download endpoint, not a
/// thumbnail bucket — the original scraper was looking in the wrong
/// place, which is why every page returned "no preview image found"):
///
///   1. `cdn.imslp.org/images/thumb/pdfs/<hash-prefix>/<hash>.png` —
///      auto-generated first-page thumbnails of uploaded PDFs. These are
///      the most reliable signal that we've grabbed actual score content
///      and not e.g. a publisher logo.
///   2. `imslp.org/images/thumb/<hash-prefix>/<hash>/<size>px-TN-<title>.{png,jpg}` —
///      MediaWiki-generated thumbnails of edition cover scans. The `TN-`
///      prefix is IMSLP's naming convention.
///
/// We try (1) first since it's an actual sheet-music page render. (2) is
/// a fallback for works where only a cover scan is uploaded. URLs may be
/// absolute (`https://`), protocol-relative (`//cdn.imslp.org/...`), or
/// site-relative (`/images/thumb/...`) — `absolutize` normalises all
/// three.
fn scrape_thumbnail_image_url(html: &str) -> Option<String> {
    // Pass 1: per-PDF auto-thumbnail on the cdn.imslp.org subdomain.
    if let Some(url) = find_image_url_containing(html, "cdn.imslp.org/images/thumb/pdfs/") {
        return Some(url);
    }
    // Pass 2: any /images/thumb/ — cover scans, MediaWiki TN thumbnails.
    if let Some(url) = find_image_url_containing(html, "/images/thumb/") {
        return Some(url);
    }
    None
}

/// Find the first URL in `html` that contains `needle` and ends in a
/// common image extension. Robust to absolute, protocol-relative, and
/// site-relative forms — we walk back to the last attribute-boundary
/// character (quote, whitespace, angle bracket) rather than searching for
/// a scheme prefix, so `src="//cdn..."` and `src="/images/..."` both
/// resolve cleanly.
fn find_image_url_containing(html: &str, needle: &str) -> Option<String> {
    let mut search_start = 0;
    while let Some(rel) = html[search_start..].find(needle) {
        let abs = search_start + rel;
        let prefix = &html[..abs];
        let url_start = prefix
            .rfind(|c: char| matches!(c, '"' | '\'' | ' ' | '<' | '>' | '\n' | '\r' | '\t'))
            .map(|i| i + 1)
            .unwrap_or(0);
        let tail = &html[url_start..];
        let term_idx = tail
            .find(|c: char| matches!(c, '"' | '\'' | ' ' | '<' | '>' | '\n' | '\r' | '\t'))
            .unwrap_or(tail.len());
        let raw = &tail[..term_idx];
        let lower = raw.to_lowercase();
        if lower.ends_with(".png")
            || lower.ends_with(".jpg")
            || lower.ends_with(".jpeg")
            || lower.ends_with(".gif")
        {
            return Some(absolutize(raw));
        }
        search_start = abs + needle.len();
    }
    None
}

fn scrape_cdn_pdf_url(html: &str) -> Option<String> {
    for needle in ["imslp.org/imglnks/", "imslp.org\\/imglnks\\/"] {
        let mut search_start = 0;
        while let Some(rel) = html[search_start..].find(needle) {
            let abs = search_start + rel;
            let prefix = &html[..abs];
            let scheme_start = match prefix.rfind("https://").or_else(|| prefix.rfind("http://")) {
                Some(s) => s,
                None => {
                    search_start = abs + needle.len();
                    continue;
                }
            };
            let tail = &html[scheme_start..];
            let term_idx = tail
                .find(|c: char| matches!(c, '"' | '\'' | ' ' | '<' | '>' | '\n' | '\r'))
                .unwrap_or(tail.len());
            let raw = &tail[..term_idx];
            let url = raw.replace("\\/", "/");
            if url.to_lowercase().contains(".pdf") {
                return Some(url);
            }
            search_start = abs + needle.len();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixture: minimal HTML matching the shapes that appear on a real
    // IMSLP work page (verified against the Chopin Nocturnes Op. 9 page
    // on 2026-05-27). Both URL forms appear in attribute context, with
    // surrounding tags.
    const FIXTURE_BOTH: &str = r#"
        <html>
          <body>
            <img src="/images/thumb/2/2d/TN-PMLP2312-cover.png/120px-TN-PMLP2312-cover.png" alt="cover">
            <p>Some text</p>
            <img src="//cdn.imslp.org/images/thumb/pdfs/98/23530132146cbeaa9ba0d105f91e9102d0fb124c.png">
          </body>
        </html>
    "#;

    const FIXTURE_COVER_ONLY: &str = r#"
        <img src="/images/thumb/a/a0/TN-Chopin_Nocturnes_Cover.jpg/438px-TN-Chopin_Nocturnes_Cover.jpg">
    "#;

    const FIXTURE_NONE: &str = r#"
        <html>
          <body>
            <p>No images here.</p>
            <a href="/imglnks/usimg/foo.pdf">Download PDF</a>
          </body>
        </html>
    "#;

    #[test]
    fn prefers_cdn_pdf_thumbnail_over_cover() {
        let url = scrape_thumbnail_image_url(FIXTURE_BOTH).expect("should find a thumbnail");
        assert!(
            url.starts_with("https://cdn.imslp.org/images/thumb/pdfs/"),
            "expected cdn.imslp.org PDF thumbnail first, got {url}"
        );
        assert!(url.ends_with(".png"));
    }

    #[test]
    fn falls_back_to_cover_when_no_pdf_thumbnail() {
        let url = scrape_thumbnail_image_url(FIXTURE_COVER_ONLY).expect("should find cover");
        assert!(
            url.starts_with("https://imslp.org/images/thumb/"),
            "expected absolutized imslp.org cover, got {url}"
        );
        assert!(url.ends_with(".jpg"));
    }

    #[test]
    fn returns_none_when_page_has_no_thumbnails() {
        assert!(scrape_thumbnail_image_url(FIXTURE_NONE).is_none());
    }

    #[test]
    fn work_page_filter_accepts_real_work_titles() {
        assert!(looks_like_imslp_work_page(
            "Symphony No.9, Op.125 (Beethoven, Ludwig van)"
        ));
        assert!(looks_like_imslp_work_page("Nocturne Op.9 No.2 (Chopin, Frédéric)"));
        assert!(looks_like_imslp_work_page(
            "Goldberg Variations, BWV 988 (Bach, Johann Sebastian)"
        ));
    }

    #[test]
    fn work_page_filter_rejects_composer_landing_and_lists() {
        // Bare composer redirect — the case that produced
        // fetch_failed_redirect on the audit log.
        assert!(!looks_like_imslp_work_page("Chopin"));
        // List pages.
        assert!(!looks_like_imslp_work_page("List of compositions by Chopin"));
        // Parenthetical without a comma — disambiguator, not composer.
        assert!(!looks_like_imslp_work_page("Sonata (piano transcription)"));
        // Parens not at end.
        assert!(!looks_like_imslp_work_page("Sonata (1820) revisited"));
    }

    #[test]
    fn absolutize_handles_protocol_relative() {
        assert_eq!(
            absolutize("//cdn.imslp.org/foo.png"),
            "https://cdn.imslp.org/foo.png"
        );
    }

    #[test]
    fn absolutize_handles_site_relative() {
        assert_eq!(
            absolutize("/images/thumb/foo.png"),
            "https://imslp.org/images/thumb/foo.png"
        );
    }

    // Regression: the old scraper only matched URLs containing
    // `imslp.org/imglnks/` and only walked back to `https://`. Any page
    // with the actual thumbnail paths (cdn.imslp.org/.../pdfs/ or
    // /images/thumb/) returned None, which is what made every IMSLP
    // result render with the placeholder SVG on 2026-05-27. This test
    // would have failed under the old implementation.
    #[test]
    fn regression_old_scraper_would_have_missed_real_imslp_shapes() {
        assert!(scrape_thumbnail_image_url(FIXTURE_BOTH).is_some());
        assert!(scrape_thumbnail_image_url(FIXTURE_COVER_ONLY).is_some());
    }
}
