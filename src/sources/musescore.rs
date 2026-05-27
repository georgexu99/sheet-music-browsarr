use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use futures_util::StreamExt;
use printpdf::{Mm, Op, PdfDocument, PdfPage, PdfSaveOptions, RawImage, XObjectTransform};
use reqwest::cookie::Jar;
use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, Url};
use serde::Deserialize;
use tokio::sync::Mutex;

use super::flaresolverr::{FlareSolverr, FsSolution};
use super::{BadgeKind, MetadataBadge, SearchFilters, SearchResult, Source};

/// Env var name. When set, MuseScore's Cloudflare-challenged GETs go
/// through FlareSolverr (the score-page and the /sheetmusic search page).
/// Bundle JS, /api/jmuse, and CDN PNG fetches stay direct.
const FLARESOLVERR_ENV: &str = "FLARESOLVERR_URL";

// MuseScore.com sits behind Cloudflare; stale or obvious-bot UAs get the
// "Just a moment…" challenge page (HTTP 403). The full set of headers a
// modern desktop Chrome sends on a top-level navigation is what gets us
// past the basic bot check. Headers safe to send on every request live in
// `default_headers()`; navigation-only ones (Sec-Fetch-*,
// Upgrade-Insecure-Requests) are layered per-request via `nav_headers()`
// so the bundle JS / jmuse XHR / CDN PNG fetches don't misrepresent
// themselves.
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
const ACCEPT_LANGUAGE: &str = "en-US,en;q=0.9";
const ACCEPT_DOC: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7";
const SEC_CH_UA: &str =
    "\"Chromium\";v=\"140\", \"Not?A_Brand\";v=\"24\", \"Google Chrome\";v=\"140\"";
const SEC_CH_UA_PLATFORM: &str = "\"Windows\"";

const TIMEOUT: Duration = Duration::from_secs(20);

/// Per-request headers a real Chrome sends on a top-level navigation
/// (typed URL / link click). Cloudflare's bot heuristics weight these
/// heavily, especially `Sec-Fetch-Site: none` (i.e., not from a referrer).
/// Layered on top of the Client's `default_headers` for score-page and
/// search-page fetches only.
fn nav_headers() -> [(HeaderName, HeaderValue); 6] {
    [
        (header::ACCEPT, HeaderValue::from_static(ACCEPT_DOC)),
        (
            header::UPGRADE_INSECURE_REQUESTS,
            HeaderValue::from_static("1"),
        ),
        (
            HeaderName::from_static("sec-fetch-dest"),
            HeaderValue::from_static("document"),
        ),
        (
            HeaderName::from_static("sec-fetch-mode"),
            HeaderValue::from_static("navigate"),
        ),
        (
            HeaderName::from_static("sec-fetch-site"),
            HeaderValue::from_static("none"),
        ),
        (
            HeaderName::from_static("sec-fetch-user"),
            HeaderValue::from_static("?1"),
        ),
    ]
}

/// MuseScore.com — community-uploaded sheet music. The site does not expose
/// a server-side PDF for user uploads (`is_pdf == 0` for ~all free scores);
/// instead it serves per-page PNG renderings through an authenticated API
/// (`/api/jmuse`) that requires a short-lived MD5 token derived from a salt
/// that's embedded in their webpack bundle. We port the technique from the
/// `musescore-downloader` browser extension:
///
///   1. Fetch a score page and extract the bundle URL from a `<link>` tag.
///   2. Download the bundle (~0.5 MB minified JS).
///   3. Locate the MD5 module (contains `_digestsize` + `_blocksize`) and
///      surgically rewrite it into a callable `window.generateToken` MD5
///      function, executed in QuickJS to mint per-request tokens.
///   4. For each page index 0..pages_count, mint a token, call jmuse for the
///      `type=img` CDN URL, GET the PNG.
///   5. Stitch PNGs into a single PDF (printpdf) and return the bytes.
///
/// The prepared script is cached by bundle URL; bundle URLs change on every
/// MuseScore deploy (the path embeds a content hash), so a single cache
/// entry is sufficient — when MuseScore deploys, we re-prepare once.
pub struct Musescore {
    http: Client,
    /// Shared cookie jar — anything Cloudflare hands us (`cf_clearance`,
    /// `__cf_bm`, etc.) gets stashed here automatically by reqwest on
    /// direct fetches, and gets injected explicitly from FlareSolverr's
    /// solution payload after a challenge-solved fetch. Either way the
    /// cookies are replayed on subsequent direct calls (bundle JS,
    /// /api/jmuse, CDN PNGs).
    jar: Arc<Jar>,
    /// Optional FlareSolverr proxy. Some(_) when `FLARESOLVERR_URL` is
    /// set at startup; None otherwise. `fetch_html_challenged()` routes
    /// through it when present and falls back to direct otherwise.
    fs: Option<FlareSolverr>,
    cached: Mutex<Option<CachedAlgorithm>>,
}

struct CachedAlgorithm {
    bundle_url: String,
    prepared_js: String,
    random_token: String,
}

impl Musescore {
    pub fn new() -> anyhow::Result<Self> {
        // Headers safe to send on every request type (navigation, XHR,
        // script, CDN). Per-request navigation-only headers are added on
        // top by callers that need them via `nav_headers()`.
        let mut default_headers = HeaderMap::new();
        default_headers.insert(
            header::ACCEPT_LANGUAGE,
            HeaderValue::from_static(ACCEPT_LANGUAGE),
        );
        default_headers.insert(
            HeaderName::from_static("sec-ch-ua"),
            HeaderValue::from_static(SEC_CH_UA),
        );
        default_headers.insert(
            HeaderName::from_static("sec-ch-ua-mobile"),
            HeaderValue::from_static("?0"),
        );
        default_headers.insert(
            HeaderName::from_static("sec-ch-ua-platform"),
            HeaderValue::from_static(SEC_CH_UA_PLATFORM),
        );

        let jar = Arc::new(Jar::default());
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(TIMEOUT)
            .gzip(true)
            .default_headers(default_headers)
            .cookie_provider(jar.clone())
            .build()?;

        // Opt-in FlareSolverr wiring. We log once at startup so the
        // operator can confirm the env var was picked up; further FS
        // failures are logged at the call site.
        let fs = match std::env::var(FLARESOLVERR_ENV).ok().filter(|s| !s.is_empty()) {
            Some(url) => {
                tracing::info!(flaresolverr_url = %url, "MuseScore: routing CF-challenged requests through FlareSolverr");
                Some(FlareSolverr::new(url).context("constructing FlareSolverr client")?)
            }
            None => {
                tracing::debug!("MuseScore: FLARESOLVERR_URL unset; direct fetches only");
                None
            }
        };

        Ok(Self {
            http,
            jar,
            fs,
            cached: Mutex::new(None),
        })
    }

    /// Fetch a Cloudflare-challenged URL. Routes through FlareSolverr if
    /// configured; falls back to a direct fetch otherwise. Cookies from
    /// the FS response are injected into our shared jar so subsequent
    /// direct fetches (bundle JS, /api/jmuse, CDN PNGs) carry the
    /// `cf_clearance` if MuseScore expands CF coverage to those paths.
    async fn fetch_html_challenged(&self, url: &str, ctx_label: &'static str) -> anyhow::Result<String> {
        match &self.fs {
            Some(fs) => {
                let solution: FsSolution = fs.get(url).await.with_context(|| {
                    format!("flaresolverr {ctx_label} {url}")
                })?;
                if solution.status >= 400 {
                    anyhow::bail!(
                        "flaresolverr {ctx_label} HTTP {}: {}",
                        solution.status,
                        truncate_for_log(&solution.response, 200)
                    );
                }
                self.absorb_fs_cookies(&solution);
                Ok(solution.response)
            }
            None => {
                let mut req = self.http.get(url);
                for (k, v) in nav_headers() {
                    req = req.header(k, v);
                }
                let resp = req
                    .send()
                    .await
                    .with_context(|| format!("musescore {ctx_label} request"))?;
                let status = resp.status();
                if !status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    let snippet = truncate_for_log(&body, 200);
                    anyhow::bail!("musescore {ctx_label} HTTP {status}: {snippet}");
                }
                resp.text()
                    .await
                    .with_context(|| format!("musescore {ctx_label} body"))
            }
        }
    }

    /// Inject every cookie FlareSolverr captured back into our reqwest
    /// cookie jar. Reqwest enforces domain / path / Secure scoping at
    /// match-time, so we serialize each cookie as a Set-Cookie-style
    /// string scoped to its own domain and let the jar do the right
    /// thing for subsequent direct requests.
    fn absorb_fs_cookies(&self, solution: &FsSolution) {
        // `Jar::add_cookie_str` wants a URL whose scheme + host imply the
        // cookie's domain. musescore.com (and its subdomains) is the only
        // host we care about; if FS ever returns cookies for a different
        // origin we'd be storing them too narrowly, but that's harmless.
        let Ok(base) = Url::parse("https://musescore.com/") else {
            return;
        };
        for c in &solution.cookies {
            let path = if c.path.is_empty() { "/" } else { c.path.as_str() };
            let secure = if c.secure { "; Secure" } else { "" };
            // Strip a leading dot from the domain — `Set-Cookie: Domain=` may
            // begin with one (RFC 6265 historical quirk) but reqwest's parser
            // prefers it without.
            let domain = c.domain.trim_start_matches('.');
            let cookie_str = format!(
                "{}={}; Domain={}; Path={}{}",
                c.name, c.value, domain, path, secure
            );
            self.jar.add_cookie_str(&cookie_str, &base);
        }
    }

    /// Fetch a score page and parse out the bundle URL plus the hydration
    /// JSON. Returns (bundle_url, score_meta).
    async fn fetch_score_page(&self, id: &str) -> anyhow::Result<(String, ScoreMeta)> {
        let url = format!("https://musescore.com/score/{id}");
        let html = self.fetch_html_challenged(&url, "page").await?;

        let bundle_url = extract_bundle_url(&html)
            .ok_or_else(|| anyhow::anyhow!("could not find musescore bundle URL on {url}"))?;
        let meta = extract_score_meta(&html)
            .ok_or_else(|| anyhow::anyhow!("could not parse hydration JSON on {url}"))?;
        Ok((bundle_url, meta))
    }

    /// Returns a prepared JS bundle and the extracted `randomToken` salt,
    /// reusing the cache if the bundle URL hasn't changed.
    async fn prepare_algorithm(&self, bundle_url: &str) -> anyhow::Result<(String, String)> {
        {
            let guard = self.cached.lock().await;
            if let Some(cached) = guard.as_ref() {
                if cached.bundle_url == bundle_url {
                    return Ok((cached.prepared_js.clone(), cached.random_token.clone()));
                }
            }
        }

        let bundle = self
            .http
            .get(bundle_url)
            .send()
            .await
            .context("musescore bundle fetch")?
            .error_for_status()
            .context("musescore bundle status")?
            .text()
            .await
            .context("musescore bundle body")?;

        let (prepared_js, random_token) = rewrite_bundle(&bundle)
            .context("rewriting musescore bundle into callable token algorithm")?;

        let mut guard = self.cached.lock().await;
        *guard = Some(CachedAlgorithm {
            bundle_url: bundle_url.to_string(),
            prepared_js: prepared_js.clone(),
            random_token: random_token.clone(),
        });
        Ok((prepared_js, random_token))
    }

    /// Mint the 4-character token for (score_id, type, index) by running the
    /// rewritten bundle in Boa (pure-Rust JS engine — chosen over QuickJS to
    /// keep the build toolchain-light: no C compiler needed on host or in
    /// the bookworm Docker builder beyond what Cargo already provides). Boa
    /// is synchronous, so we hop to a blocking thread to keep the tokio
    /// runtime free.
    async fn mint_token(
        prepared_js: String,
        random_token: String,
        score_id: String,
        media_type: String,
        index: usize,
    ) -> anyhow::Result<String> {
        tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            use boa_engine::{js_string, Context, JsValue, Source};

            let mut ctx = Context::default();

            // The rewritten bundle expects a top-level `window` global.
            ctx.eval(Source::from_bytes("var window = {};"))
                .map_err(|e| anyhow::anyhow!("eval window stub: {e}"))?;
            ctx.eval(Source::from_bytes(prepared_js.as_bytes()))
                .map_err(|e| anyhow::anyhow!("eval prepared bundle: {e}"))?;

            let window = ctx
                .global_object()
                .get(js_string!("window"), &mut ctx)
                .map_err(|e| anyhow::anyhow!("get window: {e}"))?;
            let window_obj = window
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("window is not an object"))?
                .clone();
            let generate_token_val = window_obj
                .get(js_string!("generateToken"), &mut ctx)
                .map_err(|e| anyhow::anyhow!("window.generateToken missing: {e}"))?;
            let generate_token_obj = generate_token_val
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("window.generateToken is not a function"))?
                .clone();

            // sandbox.js: md5(id + type + index + randomToken).substring(0, 4)
            let input = format!("{score_id}{media_type}{index}{random_token}");
            let arg = JsValue::from(js_string!(input.as_str()));
            let result = generate_token_obj
                .call(&JsValue::undefined(), &[arg], &mut ctx)
                .map_err(|e| anyhow::anyhow!("generateToken call: {e}"))?;
            let digest = result
                .to_string(&mut ctx)
                .map_err(|e| anyhow::anyhow!("digest to_string: {e}"))?
                .to_std_string_lossy();

            if digest.len() < 4 {
                anyhow::bail!("generateToken returned short digest {digest:?}");
            }
            Ok(digest[..4].to_string())
        })
        .await
        .context("token-mint task join")?
    }

    /// Hit `/api/jmuse` for a single (id, type, index) tuple and return the
    /// resolved CDN URL.
    async fn jmuse_url(
        &self,
        token: &str,
        referer: &str,
        id: &str,
        media_type: &str,
        index: usize,
    ) -> anyhow::Result<String> {
        let url = format!("https://musescore.com/api/jmuse?id={id}&index={index}&type={media_type}");
        let resp: JmuseResponse = self
            .http
            .get(&url)
            .header(reqwest::header::AUTHORIZATION, token)
            .header(reqwest::header::REFERER, referer)
            .send()
            .await
            .context("musescore jmuse request")?
            .error_for_status()
            .context("musescore jmuse status")?
            .json()
            .await
            .context("musescore jmuse json")?;

        if resp.result != "success" {
            anyhow::bail!("musescore jmuse error: {:?}", resp.error);
        }
        let info = resp
            .info
            .ok_or_else(|| anyhow::anyhow!("musescore jmuse missing info for type={media_type}"))?;
        if info.url.is_empty() {
            anyhow::bail!("musescore jmuse returned empty url for type={media_type} index={index}");
        }
        Ok(info.url)
    }

    /// GET a CDN URL and return the body, bounded by `max_bytes`.
    async fn fetch_bytes(&self, url: &str, max_bytes: usize) -> anyhow::Result<Vec<u8>> {
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .context("musescore cdn fetch")?
            .error_for_status()
            .context("musescore cdn status")?;
        if let Some(len) = resp.content_length() {
            anyhow::ensure!(
                (len as usize) <= max_bytes,
                "musescore asset too large ({len} > {max_bytes})"
            );
        }
        let mut buf = Vec::with_capacity(64 * 1024);
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if buf.len() + chunk.len() > max_bytes {
                anyhow::bail!("musescore asset exceeds {max_bytes} bytes during streaming");
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(buf)
    }
}

#[async_trait]
impl Source for Musescore {
    fn id(&self) -> &'static str {
        "musescore"
    }

    fn display_name(&self) -> &'static str {
        "MuseScore"
    }

    fn external_url(&self, id: &str) -> String {
        format!("https://musescore.com/score/{id}")
    }

    async fn search(
        &self,
        query: &str,
        filters: &SearchFilters,
        limit: usize,
    ) -> anyhow::Result<Vec<SearchResult>> {
        // MuseScore's /sheetmusic search page accepts an `instrument` slug
        // param. Slugs line up with our Instrument::slug() values for the
        // common cases; for ones MuseScore doesn't recognise the param is
        // silently ignored, leaving the bare text search.
        let url = match filters.instrument {
            Some(inst) => format!(
                "https://musescore.com/sheetmusic?text={}&instrument={}",
                urlencoding::encode(query),
                inst.slug()
            ),
            None => format!(
                "https://musescore.com/sheetmusic?text={}",
                urlencoding::encode(query)
            ),
        };
        let html = self.fetch_html_challenged(&url, "search").await?;

        let scores = match extract_search_scores(&html) {
            Some(s) => s,
            None => {
                tracing::warn!(
                    "musescore search hydration not found; site layout may have changed"
                );
                return Ok(Vec::new());
            }
        };

        let mut results = Vec::with_capacity(scores.len().min(limit));
        for s in scores.into_iter().take(limit) {
            let title = strip_highlight_markers(&s.title);
            let mut metadata: Vec<MetadataBadge> = Vec::new();
            if let Some(p) = s.pages_count {
                if p > 0 {
                    metadata.push(MetadataBadge {
                        label: if p == 1 {
                            "1 page".to_string()
                        } else {
                            format!("{p} pages")
                        },
                        kind: BadgeKind::Pages,
                    });
                }
            }
            if let Some(parts) = s.parts_count {
                if parts > 1 {
                    metadata.push(MetadataBadge {
                        label: format!("{parts} parts"),
                        kind: BadgeKind::Generic,
                    });
                }
            }
            for inst in s.instrumentations {
                metadata.push(MetadataBadge {
                    label: inst,
                    kind: BadgeKind::Instrument,
                });
            }
            results.push(SearchResult {
                source: "musescore".to_string(),
                id: s.id.to_string(),
                title,
                description: s.composer_name,
                external_url: s
                    .href
                    .unwrap_or_else(|| format!("https://musescore.com/score/{}", s.id)),
                thumbnail_url: s.thumbnail_url,
                metadata,
            });
        }
        Ok(results)
    }

    async fn fetch_pdf_bytes(&self, id: &str, max_bytes: usize) -> anyhow::Result<Vec<u8>> {
        let (bundle_url, meta) = self.fetch_score_page(id).await?;
        let pages_count = meta.pages_count.unwrap_or(1).max(1);
        anyhow::ensure!(pages_count <= 200, "musescore score has implausible pages_count={pages_count}");
        let referer = self.external_url(id);

        let (prepared_js, random_token) = self.prepare_algorithm(&bundle_url).await?;

        // Mint tokens + resolve per-page CDN URLs in series. Parallelizing
        // here is tempting but musescore's per-IP rate limit on `/api/jmuse`
        // is hair-trigger; serial keeps the failure modes predictable.
        let mut png_urls = Vec::with_capacity(pages_count);
        for index in 0..pages_count {
            let token = Self::mint_token(
                prepared_js.clone(),
                random_token.clone(),
                id.to_string(),
                "img".to_string(),
                index,
            )
            .await
            .with_context(|| format!("minting token for page {index}"))?;
            let url = self
                .jmuse_url(&token, &referer, id, "img", index)
                .await
                .with_context(|| format!("resolving CDN url for page {index}"))?;
            png_urls.push(url);
        }

        // Reserve a budget for the assembled PDF; each PNG roughly fits in
        // its own slice of max_bytes. We let printpdf decide actual encoding
        // and only enforce the cap on the final output.
        let per_page_budget = max_bytes;
        let mut pngs: Vec<Vec<u8>> = Vec::with_capacity(pages_count);
        let mut running = 0usize;
        for url in &png_urls {
            let bytes = self.fetch_bytes(url, per_page_budget).await?;
            running = running.saturating_add(bytes.len());
            anyhow::ensure!(
                running <= max_bytes,
                "musescore PNGs aggregate exceeds {max_bytes} bytes"
            );
            pngs.push(bytes);
        }

        let pdf_bytes = tokio::task::spawn_blocking(move || assemble_pdf(&pngs))
            .await
            .context("pdf-assemble task join")??;

        anyhow::ensure!(
            pdf_bytes.len() <= max_bytes,
            "musescore assembled PDF exceeds {max_bytes} bytes"
        );
        Ok(pdf_bytes)
    }
}

#[derive(Debug, Deserialize)]
struct JmuseResponse {
    result: String,
    #[serde(default)]
    error: Option<serde_json::Value>,
    #[serde(default)]
    info: Option<JmuseInfo>,
}

#[derive(Debug, Deserialize)]
struct JmuseInfo {
    #[serde(default)]
    url: String,
}

#[derive(Debug)]
struct ScoreMeta {
    pages_count: Option<usize>,
}

#[derive(Debug)]
struct SearchScore {
    id: u64,
    title: String,
    composer_name: Option<String>,
    href: Option<String>,
    thumbnail_url: Option<String>,
    /// Total rendered pages, when present in the hydration payload. Used
    /// for a "N pages" badge.
    pages_count: Option<usize>,
    /// Number of parts/voices; only emitted as a badge when > 1.
    parts_count: Option<usize>,
    /// Free-text instrumentation labels (e.g. "Piano", "Voice"). MuseScore
    /// usually returns 1–3 entries per score; we render each as its own
    /// pill so they stay short.
    instrumentations: Vec<String>,
}

/// Look for the JS bundle URL that matches the upstream extension's regex:
///   `https://musescore.com/static/public/build/[\w\/]+/\d+/\d+\.\w+.js`
/// The match is greedy; in practice MuseScore deploys one such URL per page.
fn extract_bundle_url(html: &str) -> Option<String> {
    // The link tag uses `<link rel='preload' href='...' as='script'>`.
    // We just regex over the whole document so we don't depend on html5
    // attribute-quote conventions.
    let needle = "https://musescore.com/static/public/build/";
    let mut start = 0;
    while let Some(rel) = html[start..].find(needle) {
        let abs = start + rel;
        let tail = &html[abs..];
        let term = tail
            .find(|c: char| matches!(c, '"' | '\'' | ' ' | '<' | '>' | '\n' | '\r'))
            .unwrap_or(tail.len());
        let candidate = &tail[..term];
        if matches_bundle_pattern(candidate) {
            return Some(candidate.to_string());
        }
        start = abs + needle.len();
    }
    None
}

/// Matches `https://musescore.com/static/public/build/<word|slash>+/\d+/\d+\.\w+\.js`
fn matches_bundle_pattern(url: &str) -> bool {
    let prefix = "https://musescore.com/static/public/build/";
    let Some(rest) = url.strip_prefix(prefix) else {
        return false;
    };
    if !url.ends_with(".js") {
        return false;
    }
    let segments: Vec<&str> = rest.split('/').collect();
    // We expect at least [<dir...>, <digits>, <digits>.<word>.js]
    if segments.len() < 3 {
        return false;
    }
    let last = segments[segments.len() - 1];
    let second_last = segments[segments.len() - 2];
    // <digits>.<word>.js
    if !second_last.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let last_no_ext = last.strip_suffix(".js").unwrap_or("");
    let mut parts = last_no_ext.splitn(2, '.');
    let head = parts.next().unwrap_or("");
    let hash = parts.next().unwrap_or("");
    if hash.is_empty() {
        return false;
    }
    head.chars().all(|c| c.is_ascii_digit())
        && hash.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Apply the three regex-style substitutions from the upstream extension to
/// turn the minified bundle into a self-contained script that defines
/// `window.generateToken`. Also returns the embedded `randomToken` salt.
fn rewrite_bundle(bundle: &str) -> anyhow::Result<(String, String)> {
    // 1. Extract the randomToken salt — the literal string that the bundle
    //    later passes through `.substr(0, 4)`. Regex: `"([\W\w]{1,50})"\)\.substr\(0, *4\)`
    let random_token = find_random_token(bundle)
        .ok_or_else(|| anyhow::anyhow!("randomToken salt not found in bundle"))?;

    // 2. Locate the webpack module id whose body contains both `_digestsize`
    //    and `_blocksize` (the MD5 module). The id appears immediately
    //    before its definition: `, <id>: function(...){` or `, <id>: (...) => {`.
    let function_number = find_md5_module_id(bundle)
        .ok_or_else(|| anyhow::anyhow!("MD5 module not found in bundle"))?;

    // 3. Apply the three textual substitutions.
    let script_start = build_script_start(&function_number);
    let mut script = replace_webpack_header(bundle, &script_start)?;
    script = replace_closing_paren(&script)?;
    script = replace_exports_with_window(&script)?;

    Ok((script, random_token))
}

/// Port of: `script.match(/"([\W\w]{1,50})"\)\.substr\(0, *4\)/)?.[1]`
fn find_random_token(s: &str) -> Option<String> {
    let mut start = 0;
    while let Some(pos) = s[start..].find("\")") {
        let abs_close = start + pos;
        // Walk backward to find the opening quote (up to 50 chars away).
        let lookback = abs_close.saturating_sub(50);
        if let Some(open) = s[lookback..abs_close].rfind('"') {
            let open_abs = lookback + open;
            if open_abs + 1 < abs_close {
                // Check that `.substr(0, 4)` (with optional spaces) follows.
                let after = &s[abs_close + 2..];
                if substr_zero_four_follows(after) {
                    return Some(s[open_abs + 1..abs_close].to_string());
                }
            }
        }
        start = abs_close + 2;
    }
    None
}

fn substr_zero_four_follows(after: &str) -> bool {
    let trimmed = after.trim_start();
    let Some(rest) = trimmed.strip_prefix(".substr(") else {
        return false;
    };
    let rest = rest.trim_start();
    let Some(rest) = rest.strip_prefix("0,") else {
        return false;
    };
    let rest = rest.trim_start();
    rest.starts_with('4')
}

/// Port of:
///   script.split(/, *(\d+): *(?:function)*\([\w,]{1,8}\)(?: *=> *|)\{/)
///   ...find part containing both `_digestsize` and `_blocksize` and return
///   the preceding capture group.
///
/// We don't actually port the regex — we scan for `_digestsize=`, then walk
/// backwards to the nearest `, <digits>: function(` (or `: (` arrow form).
fn find_md5_module_id(s: &str) -> Option<String> {
    let dig = s.find("_digestsize")?;
    // From `dig`, walk back to find the enclosing module header `, NNN: function` or `, NNN: (`.
    let prefix = &s[..dig];
    // Limit search to the last few KB to keep things cheap.
    let window_start = prefix.len().saturating_sub(50_000);
    let window = &prefix[window_start..];
    // Find the last `, <digits>: function(` or `, <digits>: (`.
    let mut found_id: Option<String> = None;
    let mut search_from = 0;
    while let Some(comma_off) = window[search_from..].find(',') {
        let abs = search_from + comma_off;
        // Try to parse `, *(\d+) *: *(?:function)?\(`
        let after = &window[abs + 1..];
        let after_trimmed = after.trim_start_matches(' ');
        let digits_end = after_trimmed
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after_trimmed.len());
        if digits_end > 0 {
            let digits = &after_trimmed[..digits_end];
            let rest = &after_trimmed[digits_end..];
            let rest = rest.trim_start_matches(' ');
            if let Some(rest) = rest.strip_prefix(':') {
                let rest = rest.trim_start_matches(' ');
                // function(...) or (...)=>{ ... }
                let is_module_header = rest.starts_with("function(")
                    || rest.starts_with("function (")
                    || rest.starts_with('(');
                if is_module_header {
                    found_id = Some(digits.to_string());
                }
            }
        }
        search_from = abs + 1;
    }
    found_id
}

fn build_script_start(function_number: &str) -> String {
    format!(
        r#"(function (modules) {{
  var installedModules = {{}};
  function __webpack_require__(moduleId) {{
    if (installedModules[moduleId]) {{ return installedModules[moduleId].exports; }}
    var module = installedModules[moduleId] = {{ i: moduleId, l: false, exports: {{}} }};
    modules[moduleId].call(module.exports, module, module.exports, __webpack_require__);
    module.l = true;
    return module.exports;
  }}
  __webpack_require__.m = modules;
  __webpack_require__.c = installedModules;
  return __webpack_require__(__webpack_require__.s = {function_number});
}})("#
    )
}

/// Port of: `script.replace(/\(self\.[^}]*(?=\{(\d+):)/, getScriptStart(...))`
/// — find `(self.` followed by chars up to a `{<digits>:`, replace that span.
fn replace_webpack_header(s: &str, replacement: &str) -> anyhow::Result<String> {
    let start = s
        .find("(self.")
        .ok_or_else(|| anyhow::anyhow!("webpack header `(self.` not found"))?;
    // Find first `{<digits>:` after start.
    let from = start + "(self.".len();
    let tail = &s[from..];
    let mut pos = 0;
    let end_rel = loop {
        let Some(brace_off) = tail[pos..].find('{') else {
            anyhow::bail!("no `{{N:` brace after (self.)");
        };
        let brace_abs = pos + brace_off;
        let after = &tail[brace_abs + 1..];
        let digits_end = after
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after.len());
        if digits_end > 0 && after.as_bytes().get(digits_end) == Some(&b':') {
            break brace_abs;
        }
        pos = brace_abs + 1;
    };
    let end = from + end_rel;
    // Also need to honor the `[^}]*` constraint: the captured span must not
    // include a `}` between `(self.` and the brace.
    if s[start..end].contains('}') {
        anyhow::bail!("webpack header span unexpectedly contains a closing brace");
    }
    let mut out = String::with_capacity(s.len() + replacement.len());
    out.push_str(&s[..start]);
    out.push_str(replacement);
    out.push_str(&s[end..]);
    Ok(out)
}

/// Port of: `script.replace(/}}]\)/, '}})')`
fn replace_closing_paren(s: &str) -> anyhow::Result<String> {
    let needle = "}}])";
    let pos = s
        .find(needle)
        .ok_or_else(|| anyhow::anyhow!("closing `}}}}])` not found"))?;
    let mut out = String::with_capacity(s.len());
    out.push_str(&s[..pos]);
    out.push_str("}})");
    out.push_str(&s[pos + needle.len()..]);
    Ok(out)
}

/// Port of:
///   `script.replace(/_digestsize=(\d+),\w+\.exports=function\(/,
///     (m, a) => `_digestsize=${a},window.generateToken=function(`)`
fn replace_exports_with_window(s: &str) -> anyhow::Result<String> {
    // Find `_digestsize=` then `<digits>,<word>.exports=function(`.
    let dig_start = s
        .find("_digestsize=")
        .ok_or_else(|| anyhow::anyhow!("_digestsize= not found"))?;
    let after = &s[dig_start + "_digestsize=".len()..];
    let digits_end = after
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| anyhow::anyhow!("_digestsize= has no digits"))?;
    let digits = &after[..digits_end];
    let rest = &after[digits_end..];
    let rest = rest
        .strip_prefix(',')
        .ok_or_else(|| anyhow::anyhow!("expected `,` after digestsize digits"))?;
    let word_end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    let suffix = &rest[word_end..];
    let suffix = suffix
        .strip_prefix(".exports=function(")
        .ok_or_else(|| anyhow::anyhow!("expected `<word>.exports=function(` after digestsize"))?;
    // Reassemble.
    let head = &s[..dig_start];
    let tail = suffix;
    let replacement = format!("_digestsize={digits},window.generateToken=function(");
    let mut out = String::with_capacity(s.len() + replacement.len());
    out.push_str(head);
    out.push_str(&replacement);
    out.push_str(tail);
    Ok(out)
}

/// Locate and parse the SSR hydration JSON inside the score page or search
/// page. MuseScore wraps the entire JSON state in a `data-<sha256>` attribute
/// on a `<div class="js-<sha256>">` element. We don't validate the matching
/// hash pair — we just take the first long-hex data attribute on the page.
fn find_hydration_json(html: &str) -> Option<String> {
    // Find `data-<hex60+>="..."`.
    let mut start = 0;
    while let Some(off) = html[start..].find("data-") {
        let abs = start + off;
        let tail = &html[abs + 5..];
        let hex_end = tail
            .find(|c: char| !(c.is_ascii_hexdigit()))
            .unwrap_or(tail.len());
        if hex_end >= 60 {
            let after = &tail[hex_end..];
            if let Some(eq) = after.strip_prefix("=\"") {
                let close = eq.find('"')?;
                let raw = &eq[..close];
                return Some(html_unescape(raw));
            }
        }
        start = abs + 5;
    }
    None
}

/// Decode HTML entities — both named (`&eacute;`, `&ndash;`, …) and
/// numeric (`&#039;`, `&#x2014;`). Used in two places:
///   1. Unescaping the `data-<hash>="…"` attribute that wraps the
///      hydration JSON: turns `&quot;` etc. back into JSON syntax.
///   2. Unescaping individual JSON string values (`title`,
///      `composer_name`) which MuseScore stores in HTML-encoded form.
fn html_unescape(s: &str) -> String {
    html_escape::decode_html_entities(s).into_owned()
}

/// Bound an error-log snippet to `max` Unicode characters. Used when
/// quoting upstream response bodies in `bail!` messages so a 4 MB
/// Cloudflare challenge page doesn't flood the logs.
fn truncate_for_log(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn extract_score_meta(html: &str) -> Option<ScoreMeta> {
    let json = find_hydration_json(html)?;
    let v: serde_json::Value = serde_json::from_str(&json).ok()?;
    let pages_count = find_pages_count(&v);
    Some(ScoreMeta { pages_count })
}

fn find_pages_count(v: &serde_json::Value) -> Option<usize> {
    match v {
        serde_json::Value::Object(map) => {
            if let Some(pc) = map.get("pages_count").and_then(|x| x.as_u64()) {
                if map.contains_key("id") && map.contains_key("title") {
                    return Some(pc as usize);
                }
            }
            for (_, child) in map {
                if let Some(found) = find_pages_count(child) {
                    return Some(found);
                }
            }
            None
        }
        serde_json::Value::Array(arr) => arr.iter().find_map(find_pages_count),
        _ => None,
    }
}

fn extract_search_scores(html: &str) -> Option<Vec<SearchScore>> {
    let json = find_hydration_json(html)?;
    let v: serde_json::Value = serde_json::from_str(&json).ok()?;
    let scores = find_scores_array(&v)?;
    let mut out = Vec::with_capacity(scores.len());
    for s in scores {
        let id = s.get("id").and_then(|x| x.as_u64())?;
        // MuseScore's hydration JSON stores user-facing text HTML-escaped
        // even inside JSON string values, so titles come through as
        // "Pr&eacute;lude…" and "Sonata &ndash; First Movement". Decode
        // here so the rest of the pipeline (caching, dedup, render) sees
        // the real text.
        let title = html_unescape(s.get("title").and_then(|x| x.as_str())?);
        let composer_name = s
            .get("composer_name")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(html_unescape);
        let href = s
            .get("href")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);
        // Try several known thumbnail field shapes — MuseScore's hydration
        // schema has shifted over time. First hit wins; None falls back to
        // the placeholder in the template.
        let thumbnail_url = s
            .get("thumbnails")
            .and_then(|t| {
                t.get("original")
                    .or_else(|| t.get("large"))
                    .or_else(|| t.get("medium"))
                    .or_else(|| t.get("small"))
            })
            .and_then(|x| x.as_str())
            .or_else(|| s.get("share_pic_url").and_then(|x| x.as_str()))
            .or_else(|| s.get("share_pic").and_then(|x| x.as_str()))
            .or_else(|| s.get("cover_pic").and_then(|x| x.as_str()))
            .or_else(|| s.get("thumbnail_url").and_then(|x| x.as_str()))
            .filter(|s| !s.is_empty())
            .map(String::from);
        // MuseScore's hydration JSON exposes pages_count and (less reliably)
        // parts_count and an instrumentations array. We pull whatever's
        // present; missing keys just mean no badge for that score.
        let pages_count = s
            .get("pages_count")
            .and_then(|x| x.as_u64())
            .map(|n| n as usize);
        let parts_count = s
            .get("parts_count")
            .and_then(|x| x.as_u64())
            .map(|n| n as usize);
        let instrumentations = extract_instrumentations(s.get("instrumentations"));
        out.push(SearchScore {
            id,
            title,
            composer_name,
            href,
            thumbnail_url,
            pages_count,
            parts_count,
            instrumentations,
        });
    }
    Some(out)
}

fn find_scores_array(v: &serde_json::Value) -> Option<Vec<serde_json::Value>> {
    match v {
        serde_json::Value::Object(map) => {
            if let Some(arr) = map.get("scores").and_then(|x| x.as_array()) {
                if arr
                    .first()
                    .and_then(|x| x.as_object())
                    .map(|o| o.contains_key("id"))
                    .unwrap_or(false)
                {
                    return Some(arr.clone());
                }
            }
            for (_, child) in map {
                if let Some(found) = find_scores_array(child) {
                    return Some(found);
                }
            }
            None
        }
        serde_json::Value::Array(arr) => arr.iter().find_map(find_scores_array),
        _ => None,
    }
}

/// MuseScore's `instrumentations` field has varied shape across deploys:
/// sometimes a list of strings, sometimes a list of `{name: "Piano"}`
/// objects, sometimes absent. Normalise to a Vec<String> of cleaned labels.
/// We cap at 2 entries to keep the badge row visually quiet on dense
/// orchestral works.
fn extract_instrumentations(v: Option<&serde_json::Value>) -> Vec<String> {
    let Some(arr) = v.and_then(|x| x.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in arr {
        let label = if let Some(s) = entry.as_str() {
            Some(s.to_string())
        } else if let Some(obj) = entry.as_object() {
            obj.get("name")
                .and_then(|x| x.as_str())
                .map(String::from)
                .or_else(|| obj.get("title").and_then(|x| x.as_str()).map(String::from))
        } else {
            None
        };
        if let Some(l) = label {
            let l = l.trim();
            if !l.is_empty() {
                out.push(l.to_string());
            }
        }
        if out.len() >= 2 {
            break;
        }
    }
    out
}

/// MuseScore wraps highlighted query matches in `[b]...[/b]` (BBCode-ish).
fn strip_highlight_markers(s: &str) -> String {
    s.replace("[b]", "").replace("[/b]", "")
}

/// Stitch a Vec of PNG byte buffers into a single PDF document, one page
/// per image. Page size is derived from the PNG dimensions at 96 DPI so the
/// page proportions match the rendered score.
fn assemble_pdf(pngs: &[Vec<u8>]) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(!pngs.is_empty(), "no PNG pages to assemble");
    let mut doc = PdfDocument::new("MuseScore Score");
    let mut pages = Vec::with_capacity(pngs.len());

    let mut warnings = Vec::new();
    for (i, png_bytes) in pngs.iter().enumerate() {
        let image = RawImage::decode_from_bytes(png_bytes, &mut warnings)
            .map_err(|e| anyhow::anyhow!("decoding PNG page {i}: {e}"))?;
        let width_px = image.width as f32;
        let height_px = image.height as f32;
        let dpi = 96.0_f32;
        let width_mm = (width_px / dpi) * 25.4;
        let height_mm = (height_px / dpi) * 25.4;
        let image_id = doc.add_image(&image);
        let contents = vec![Op::UseXobject {
            id: image_id,
            transform: XObjectTransform::default(),
        }];
        pages.push(PdfPage::new(Mm(width_mm), Mm(height_mm), contents));
    }

    let bytes = doc
        .with_pages(pages)
        .save(&PdfSaveOptions::default(), &mut Vec::new());
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_pattern_matches_score_bundle() {
        assert!(matches_bundle_pattern(
            "https://musescore.com/static/public/build/musescore_es6/202605/2946.39bf11a4f5f177d8d4f4d5c31f8d973e.js"
        ));
        // ms.<hash>.js — head segment must be digits
        assert!(!matches_bundle_pattern(
            "https://musescore.com/static/public/build/musescore_es6/202605/ms.8161d273ff40c7bcaf29ff0743fbc076.js"
        ));
        // vendor.<hash>.js
        assert!(!matches_bundle_pattern(
            "https://musescore.com/static/public/build/musescore_es6/202605/vendor.05a258a5192adf06a493aca23bbc02ab.js"
        ));
    }

    #[test]
    fn strip_markers() {
        assert_eq!(
            strip_highlight_markers("[b]Fur[/b] [b]Elise[/b]"),
            "Fur Elise"
        );
    }

    // ----- Integration smoke test (Phase D) -----
    //
    // `#[ignore]` so it never runs as part of `cargo test`. Exercises the
    // whole MuseScore pipeline against the live site:
    //   search → score page → bundle fetch → rewrite_bundle → Boa →
    //   /api/jmuse → per-page PNGs → printpdf assembly.
    //
    // Designed for the CI Linux runner; the Windows dev host can't link
    // boa_engine without gcc. Run manually with:
    //
    //     cargo test musescore_smoke -- --ignored --nocapture
    //
    // Failure modes guide where the rewriter / pipeline is broken:
    //   * "musescore search HTTP …" — Phase B headers needed
    //   * "could not find musescore bundle URL …" — Phase E rewriter
    //   * "MD5 module not found in bundle" — Phase E (find_md5_module_id)
    //   * "randomToken salt not found in bundle" — Phase E (find_random_token)
    //   * "generateToken call: …" / "window.generateToken missing" — Phase E (Boa eval)
    //   * "musescore jmuse error: …" — token mint wrong, or MuseScore Pro content
    //   * "bytes don't start with %PDF-1." — printpdf re-encode regression
    //
    // Override the score id via MUSESCORE_SMOKE_QUERY / MUSESCORE_SMOKE_ID
    // env vars to pin to a known-stable score when MuseScore takes one
    // down. Default query "bach" should always return free public-domain
    // user uploads.
    #[tokio::test]
    #[ignore]
    async fn musescore_smoke_search_and_fetch_pdf() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        let m = Musescore::new().expect("Musescore::new");

        let query =
            std::env::var("MUSESCORE_SMOKE_QUERY").unwrap_or_else(|_| "bach".to_string());

        let results = m
            .search(&query, &SearchFilters::default(), 5)
            .await
            .expect("search must not error for a stable query");
        assert!(!results.is_empty(), "search returned 0 results for {query:?}");

        // If env-pinned, prefer that id. Otherwise try each result in
        // order until one's PDF resolves — guards against the top result
        // being MuseScore-Pro-only content.
        let candidates: Vec<String> = match std::env::var("MUSESCORE_SMOKE_ID").ok() {
            Some(id) => vec![id],
            None => results.iter().take(3).map(|r| r.id.clone()).collect(),
        };

        let mut last_err: Option<anyhow::Error> = None;
        for id in &candidates {
            match m.fetch_pdf_bytes(id, 25 * 1024 * 1024).await {
                Ok(bytes) => {
                    assert!(
                        bytes.starts_with(b"%PDF-1."),
                        "id={id} bytes don't start with %PDF-1.; got first 8 bytes: {:?}",
                        &bytes[..bytes.len().min(8)]
                    );
                    assert!(bytes.len() > 1024, "id={id} PDF suspiciously small: {} B", bytes.len());
                    eprintln!("OK: id={id} bytes={} (first 8 bytes look like a PDF)", bytes.len());
                    return;
                }
                Err(e) => {
                    eprintln!("id={id} fetch failed: {e:#}");
                    last_err = Some(e);
                }
            }
        }
        panic!(
            "all {} candidate score ids failed; last error: {:?}",
            candidates.len(),
            last_err.expect("at least one candidate"),
        );
    }
}
