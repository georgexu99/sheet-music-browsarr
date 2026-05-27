use std::io::Write;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use futures_util::StreamExt;
use reqwest::Client;
use scraper::{Html, Selector};

use super::{SearchResult, Source};

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

    async fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<SearchResult>> {
        let resp = self
            .http
            .get(format!("{BASE}/cgibin/make-table.cgi"))
            .query(&[
                ("searchingfor", query),
                ("Composer", ""),
                ("Style", ""),
                ("Instrument", ""),
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

        // Mutopia's results page lays each piece out as a table block. The
        // simplest way to extract them: collect every anchor whose href
        // points at a piece page (`/cgibin/piece-info.cgi?id=...`) or a
        // direct PDF file under `/ftp/`. Group by surrounding row and emit
        // one SearchResult per piece, preferring direct PDF links.
        let results = {
            let doc = Html::parse_document(&html);
            let pdf_sel =
                Selector::parse("a[href$='-a4.pdf'], a[href$='-let.pdf'], a[href$='.pdf']")
                    .map_err(|e| anyhow::anyhow!("pdf selector: {e}"))?;

            let mut seen = std::collections::HashSet::new();
            let mut out: Vec<SearchResult> = Vec::new();
            for el in doc.select(&pdf_sel) {
                if out.len() >= limit {
                    break;
                }
                let href = match el.value().attr("href") {
                    Some(h) => h,
                    None => continue,
                };
                let url = absolutize(href);
                if !seen.insert(url.clone()) {
                    continue;
                }
                // Title heuristic: filename without extension and -a4/-let suffix.
                let filename = url.rsplit('/').next().unwrap_or("").to_string();
                let title = filename
                    .trim_end_matches(".pdf")
                    .trim_end_matches("-a4")
                    .trim_end_matches("-let")
                    .replace('-', " ")
                    .replace('_', " ");
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
                out.push(SearchResult {
                    source: "mutopia".to_string(),
                    id,
                    title,
                    description: None,
                    external_url: url,
                    thumbnail_url,
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

