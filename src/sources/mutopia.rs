use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use futures_util::StreamExt;
use reqwest::Client;
use scraper::{Html, Selector};

use super::{SearchResult, Source};

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
                let thumbnail_url = derive_thumbnail_url(&url);
                out.push(SearchResult {
                    source: "mutopia".to_string(),
                    id: encode_url(&url),
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

/// Derive a Mutopia preview-image URL from a PDF URL.
/// Mutopia ships LilyPond-rendered previews next to each piece's PDF
/// at `<base>-pre.png`, where the PDF is `<base>-{a4,let}.pdf`.
/// Returns None on unexpected shapes so the template falls back to the
/// generic placeholder.
fn derive_thumbnail_url(pdf_url: &str) -> Option<String> {
    let without_pdf = pdf_url.strip_suffix(".pdf").or_else(|| pdf_url.strip_suffix(".PDF"))?;
    let base = without_pdf
        .strip_suffix("-let")
        .or_else(|| without_pdf.strip_suffix("-a4"))
        .unwrap_or(without_pdf);
    Some(format!("{base}-pre.png"))
}
