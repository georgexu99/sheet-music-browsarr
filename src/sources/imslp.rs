use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use reqwest::cookie::Jar;
use reqwest::{Client, Url};
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
        // Pre-seed cookies that bypass IMSLP's "click to accept disclaimer"
        // interstitial. The exact cookie name has changed over the years —
        // setting several known/historical names defensively. Worst case, the
        // upstream Content-Type check downstream catches a miss.
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

    /// Resolve a work page id to a direct PDF URL. Prefers IMSLP's CDN
    /// (`imslp.org/imglnks/.../*.pdf`) which bypasses the disclaimer; falls
    /// back to the `Special:ImagefromIndex/...` form which usually serves the
    /// file directly once the disclaimer cookie is set.
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
            .ok_or_else(|| anyhow::anyhow!("no PDF link on {page_url}"))
    }

    pub fn http(&self) -> &Client {
        &self.http
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
