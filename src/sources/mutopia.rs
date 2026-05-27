use std::io::Write;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use futures_util::StreamExt;
use reqwest::Client;
use scraper::{Html, Selector};

use super::{BadgeKind, MetadataBadge, SearchFilters, SearchResult, Source};

/// Cap on the PDF size we'll download just to generate a thumbnail.
/// Most Mutopia PDFs are well under this; engraved scores grow with
/// page count, and we only need page 1, so refusing oversized PDFs
/// here keeps thumbnail generation latency bounded.
const THUMBNAIL_PDF_MAX_BYTES: usize = 4 * 1024 * 1024;
/// Hard wall-clock cap on pdftoppm. Page-1 rasterization on a typical
/// score takes <1s; 15s is safety margin for a runaway invocation.
const PDFTOPPM_TIMEOUT_SECS: u64 = 15;

const USER_AGENT: &str = concat!(
    "sheet-music-browsarr/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/georgexu99/sheet-music-browsarr)"
);

const BASE: &str = "https://www.mutopiaproject.org";

/// Mutopia Project — public-domain sheet music engravings. Catalog is
/// small (~2,000 pieces) but covers Bach, Mozart, etc. Search uses the
/// classic CGI endpoint that returns HTML; each result links to a piece
/// detail page that hosts the PDF directly (no disclaimer, no auth).
#[derive(Clone)]
pub struct Mutopia {
    http: Client,
}

impl Mutopia {
    pub fn new() -> anyhow::Result<Self> {
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(20))
            .gzip(true)
            .build()?;
        Ok(Self { http })
    }
}

#[async_trait]
impl Source for Mutopia {
    fn id(&self) -> &'static str {
        "mutopia"
    }

    fn display_name(&self) -> &'static str {
        "Mutopia Project"
    }

    fn external_url(&self, id: &str) -> String {
        // We encode either the piece-detail page URL or the direct PDF URL
        // into the id. external_url returns whichever decodes successfully,
        // falling back to the search page if the id is malformed.
        decode_url(id).unwrap_or_else(|| format!("{BASE}/cgibin/make-table.cgi"))
    }

    async fn search(
        &self,
        query: &str,
        filters: &SearchFilters,
        limit: usize,
    ) -> anyhow::Result<Vec<SearchResult>> {
        // Mutopia exposes Instrument as a first-class CGI facet; just
        // populate its existing field. Title-case strings ("Piano",
        // "Guitar", …); see Instrument::mutopia_value for the mapping.
        let instrument = filters
            .instrument
            .map(|i| i.mutopia_value())
            .unwrap_or("");
        let resp = self
            .http
            .get(format!("{BASE}/cgibin/make-table.cgi"))
            .query(&[
                ("searchingfor", query),
                ("Composer", ""),
                ("Style", ""),
                ("Instrument", instrument),
                ("Collection", ""),
                ("recent", ""),
                ("timelength", ""),
                ("timeunit", "days"),
                ("Preview", "1"),
            ])
            .send()
            .await
            .context("mutopia search")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(200).collect();
            anyhow::bail!("mutopia search HTTP {status}: {snippet}");
        }
        let html = resp.text().await.context("mutopia search body")?;

        // Mutopia's results page lays each piece out as a `table.result-table`
        // block with rows:
        //   r1: <title> | <composer> | <opus/cat> | n/a
        //   r2: "for <instrumentation>" | <year/century> | <style/period> | n/a
        //   r3: <notes> | <license> | <piece-info link> | <date>
        //   r4-r5: download links (ly, mid, preview, ftp, ps.gz, A4 pdf, ...)
        // We walk per-piece tables so each result owns a stable title,
        // description, and metadata badge set rather than reverse-engineering
        // them from the PDF filename.
        let results = {
            let doc = Html::parse_document(&html);
            let table_sel = Selector::parse("table.result-table")
                .map_err(|e| anyhow::anyhow!("table selector: {e}"))?;
            let row_sel = Selector::parse("tr")
                .map_err(|e| anyhow::anyhow!("row selector: {e}"))?;
            let cell_sel = Selector::parse("td")
                .map_err(|e| anyhow::anyhow!("cell selector: {e}"))?;
            let a_sel = Selector::parse("a[href]")
                .map_err(|e| anyhow::anyhow!("anchor selector: {e}"))?;

            let mut seen = std::collections::HashSet::new();
            let mut out: Vec<SearchResult> = Vec::new();
            for table in doc.select(&table_sel) {
                if out.len() >= limit {
                    break;
                }
                let rows: Vec<_> = table.select(&row_sel).collect();
                if rows.is_empty() {
                    continue;
                }

                // Prefer the A4 PDF; fall back to any *.pdf anchor.
                let mut a4_pdf: Option<String> = None;
                let mut any_pdf: Option<String> = None;
                for a in table.select(&a_sel) {
                    let Some(href) = a.value().attr("href") else { continue };
                    let lower = href.to_lowercase();
                    if !lower.ends_with(".pdf") {
                        continue;
                    }
                    if a4_pdf.is_none() && lower.ends_with("-a4.pdf") {
                        a4_pdf = Some(absolutize(href));
                    } else if any_pdf.is_none() {
                        any_pdf = Some(absolutize(href));
                    }
                }
                let url = match a4_pdf.or(any_pdf) {
                    Some(u) => u,
                    None => continue,
                };
                if !seen.insert(url.clone()) {
                    continue;
                }

                // r1 cell 0 = title.
                let first_cells: Vec<_> = rows[0].select(&cell_sel).collect();
                let title = first_cells
                    .first()
                    .map(|c| clean_cell_text(&c.text().collect::<String>()))
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| {
                        // Fall back to the PDF-filename heuristic if the
                        // table layout shifted from under us.
                        let filename = url.rsplit('/').next().unwrap_or("").to_string();
                        filename
                            .trim_end_matches(".pdf")
                            .trim_end_matches("-a4")
                            .trim_end_matches("-let")
                            .replace('-', " ")
                            .replace('_', " ")
                    });
                if title.is_empty() {
                    continue;
                }
                let id = encode_url(&url);
                // Point at our lazy server-rendered thumbnail route.
                // Mutopia hosts no preview API, so the route shells out
                // to pdftoppm at request time and caches the PNG in
                // moka (see `routes::public::thumbnail_handler`). The
                // template's `loading="lazy"` keeps this off the search
                // critical path.
                let thumbnail_url = Some(format!("/thumbnail/mutopia/{id}"));

                // Composer (r1 cell 1) becomes the description, stripped of
                // the "by " prefix Mutopia uses for attributed works.
                let composer = first_cells
                    .get(1)
                    .map(|c| clean_cell_text(&c.text().collect::<String>()))
                    .filter(|s| !s.is_empty() && s != "Anonymous")
                    .map(|s| s.trim_start_matches("by ").to_string());

                // r2 cell 0 = "for <instrumentation>"; cell 1 = year/century;
                // cell 2 = style/period.
                let mut metadata: Vec<MetadataBadge> = Vec::new();
                if let Some(r2) = rows.get(1) {
                    let r2_cells: Vec<_> = r2.select(&cell_sel).collect();
                    if let Some(c) = r2_cells.first() {
                        let raw = clean_cell_text(&c.text().collect::<String>());
                        if let Some(inst) = raw.strip_prefix("for ").map(|s| s.to_string()) {
                            if !inst.is_empty() {
                                metadata.push(MetadataBadge {
                                    label: inst,
                                    kind: BadgeKind::Instrument,
                                });
                            }
                        }
                    }
                    if let Some(c) = r2_cells.get(1) {
                        let raw = clean_cell_text(&c.text().collect::<String>());
                        if !raw.is_empty() && raw != "n/a" {
                            metadata.push(MetadataBadge {
                                label: raw,
                                kind: BadgeKind::Year,
                            });
                        }
                    }
                    if let Some(c) = r2_cells.get(2) {
                        let raw = clean_cell_text(&c.text().collect::<String>());
                        if !raw.is_empty() && raw != "n/a" {
                            metadata.push(MetadataBadge {
                                label: raw,
                                kind: BadgeKind::Generic,
                            });
                        }
                    }
                }
                out.push(SearchResult {
                    source: "mutopia".to_string(),
                    id,
                    title,
                    description: composer,
                    external_url: url,
                    thumbnail_url,
                    metadata,
                });
            }
            out
        };

        Ok(results)
    }

    async fn fetch_pdf_bytes(&self, id: &str, max_bytes: usize) -> anyhow::Result<Vec<u8>> {
        let url = decode_url(id).ok_or_else(|| anyhow::anyhow!("malformed mutopia id"))?;
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .context("mutopia pdf fetch")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(200).collect();
            anyhow::bail!("mutopia pdf HTTP {status}: {snippet}");
        }

        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();
        anyhow::ensure!(
            ct.starts_with("application/pdf") || ct.starts_with("application/octet-stream"),
            "upstream returned {ct:?} (not a PDF)"
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

    /// Rasterize the first page of the work's PDF to PNG. Mutopia has
    /// no preview-image API, so we fetch the (small) PDF and shell out
    /// to `pdftoppm` from the runtime image's `poppler-utils` package.
    /// The route caches the returned bytes in moka so subsequent loads
    /// of the same result skip the fetch + rasterize entirely.
    async fn thumbnail_bytes(&self, id: &str) -> anyhow::Result<(Vec<u8>, &'static str)> {
        let pdf_bytes = self.fetch_pdf_bytes(id, THUMBNAIL_PDF_MAX_BYTES).await?;

        // pdftoppm is a synchronous CLI — run the spawn + read on a
        // blocking thread so it doesn't stall the async runtime. We
        // can't pipe the PDF in on stdin because pdftoppm only writes
        // its output to a file prefix (no stdout streaming for -png
        // -singlefile), so a real on-disk temp file is unavoidable.
        let png = tokio::time::timeout(
            Duration::from_secs(PDFTOPPM_TIMEOUT_SECS),
            tokio::task::spawn_blocking(move || rasterize_first_page(&pdf_bytes)),
        )
        .await
        .context("pdftoppm timed out")?
        .context("pdftoppm task join")??;

        Ok((png, "image/png"))
    }
}

/// Synchronous body of `thumbnail_bytes`: writes the PDF to a temp
/// directory, runs `pdftoppm`, and reads the resulting PNG back. The
/// `TempDir` handle is dropped at function exit so both files are
/// cleaned up even on error paths.
fn rasterize_first_page(pdf_bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let dir = tempfile::tempdir().context("create temp dir")?;
    let pdf_path = dir.path().join("in.pdf");
    let out_prefix = dir.path().join("out");

    {
        let mut f = std::fs::File::create(&pdf_path).context("open temp pdf")?;
        f.write_all(pdf_bytes).context("write temp pdf")?;
        f.flush().ok();
    }

    // `-singlefile` writes exactly one PNG named `<prefix>.png` (no
    // page-number suffix). `-r 72` keeps the image small (one screen
    // DPI is plenty for a card thumbnail). `-f 1 -l 1` defends against
    // multi-page output if a future poppler version changes defaults.
    let out = std::process::Command::new("pdftoppm")
        .arg("-png")
        .arg("-singlefile")
        .arg("-r")
        .arg("72")
        .arg("-f")
        .arg("1")
        .arg("-l")
        .arg("1")
        .arg(&pdf_path)
        .arg(&out_prefix)
        .output()
        .context("spawn pdftoppm")?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "pdftoppm exit {:?}: {}",
            out.status.code(),
            stderr.trim()
        );
    }

    let png_path = dir.path().join("out.png");
    let png = std::fs::read(&png_path).context("read pdftoppm output")?;
    Ok(png)
}

fn absolutize(href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        href.to_string()
    } else if let Some(stripped) = href.strip_prefix("//") {
        format!("https://{stripped}")
    } else if href.starts_with('/') {
        format!("{BASE}{href}")
    } else {
        format!("{BASE}/{href}")
    }
}

/// Encode an arbitrary URL into a URL-safe id (no slashes, no padding).
/// Lets us avoid wildcard path-params and keeps the route /pdf/:src/:id
/// clean.
fn encode_url(url: &str) -> String {
    B64.encode(url.as_bytes())
}

fn decode_url(id: &str) -> Option<String> {
    B64.decode(id.as_bytes())
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
}

/// Collapse runs of whitespace (including the `&nbsp;` chars Mutopia litters
/// throughout the results table) into single spaces and trim. Mutopia uses
/// a literal "&nbsp;" placeholder in empty cells; scraper renders that as
/// U+00A0, which `trim` does not strip.
fn clean_cell_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = true;
    for c in s.chars() {
        if c.is_whitespace() || c == '\u{00A0}' {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out.trim().to_string()
}
