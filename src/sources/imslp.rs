use std::time::Duration;

use anyhow::Context;
use reqwest::Client;
use scraper::{Html, Selector};

use super::SearchResult;

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
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(20))
            .gzip(true)
            .build()?;
        Ok(Self { http })
    }

    pub async fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<SearchResult>> {
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

        // OpenSearch response: [query, [titles], [descriptions], [urls]]
        let arr = resp
            .as_array()
            .context("imslp opensearch: expected array")?;
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

    /// Resolve a work page id to a direct PDF URL by scraping the first
    /// downloadable PDF link from the wiki page.
    pub async fn fetch_pdf_url(&self, page_id: &str) -> anyhow::Result<String> {
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

        let doc = Html::parse_document(&html);
        // IMSLP file download links carry either an explicit Special:ImagefromIndex
        // path or end in .pdf. Look at the first matching <a>.
        let selector = Selector::parse("a").map_err(|e| anyhow::anyhow!("selector: {e}"))?;
        for el in doc.select(&selector) {
            let href = match el.value().attr("href") {
                Some(h) => h,
                None => continue,
            };
            if href.contains("Special:ImagefromIndex") || href.ends_with(".pdf") {
                let abs = if href.starts_with("http") {
                    href.to_string()
                } else if let Some(stripped) = href.strip_prefix("//") {
                    format!("https://{stripped}")
                } else {
                    format!("https://imslp.org{href}")
                };
                return Ok(abs);
            }
        }
        anyhow::bail!("no PDF link found on {page_url}")
    }

    pub fn http(&self) -> &Client {
        &self.http
    }
}

fn page_id_from_url(url: &str) -> Option<String> {
    url.split_once("/wiki/").map(|(_, rest)| rest.to_string())
}
