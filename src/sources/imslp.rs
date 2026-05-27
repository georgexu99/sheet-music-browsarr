use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::cookie::Jar;
use reqwest::{Client, Url};
use scraper::{Html, Selector};

use super::{SearchResult, Source};

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

    async fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<SearchResult>> {
        let resp: serde_json::Value = self
            .http
            .get("https://imslp.org/api.php")
            .query(&[
                ("action", "opensearch"),
                ("search", query),
                ("format", "json"),
                ("limit", &limit.to_string()),
                ("namespace", "0"),
            ])
            .send()
            .await
            .context("imslp opensearch request")?
            .error_for_status()
            .context("imslp opensearch status")?
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
            results.push(SearchResult {
                source: "imslp".to_string(),
                id,
                title,
                description: desc,
                external_url: url,
            });
        }
        Ok(results)
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

fn absolutize(href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        href.to_string()
    } else if let Some(stripped) = href.strip_prefix("//") {
        format!("https://{stripped}")
    } else {
        format!("https://imslp.org{href}")
    }
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
