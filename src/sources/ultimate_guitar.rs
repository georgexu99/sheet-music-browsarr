//! Ultimate Guitar source — guitar tabs & chords.
//!
//! ultimate-guitar.com is the largest community tab/chord catalog. Unlike
//! the sheet-music sources (IMSLP / Mutopia / MuseScore) it has no PDF: a
//! tab is plain text — ASCII tablature and chord-over-lyric sheets, marked
//! up inline with `[tab]…[/tab]` (monospaced blocks) and `[ch]…[/ch]`
//! (chord tokens). We fit it into the app's PDF-centric pipeline by
//! rendering that text into a monospaced PDF (printpdf, built-in Courier),
//! so the existing download / email-to-self paths work unchanged.
//!
//! Both the search page and each tab page server-render a `<div
//! class="js-store" data-content="…">` whose attribute holds a big JSON
//! blob (html5ever decodes the entity-escaped attribute for us). Search
//! results live at `store.page.data.results`; a tab's text lives at
//! `store.page.data.tab_view.wiki_tab.content`. We navigate the JSON with
//! `serde_json::Value` pointers rather than mirroring UG's full schema,
//! so an upstream field reshuffle degrades to "missing field" instead of a
//! hard deserialize error.
//!
//! That div only survives in the *server-rendered* HTML. A direct fetch
//! sees it; FlareSolverr hands back the *post-JS* DOM, where UG has hydrated
//! the blob into `window.UGAPP.store` and removed the div. `extract_js_store`
//! therefore falls back to recovering the JSON straight from the
//! `window.UGAPP.store = …` assignment in an inline script, re-wrapping it as
//! `{"store": …}` so the pointers above are unchanged either way.
//!
//! Cloudflare: UG sits behind CF like MuseScore, so this source carries the
//! same opt-in FlareSolverr wiring (`FLARESOLVERR_URL`) with a direct-first
//! / FS-fallback fetch and `cf_clearance` cookie replay. Because a cold FS
//! solve is slow, the route layer treats UG as a *deferred* source (loaded
//! out-of-band alongside MuseScore) so it never blocks first paint.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use base64::Engine;
use printpdf::{
    BuiltinFont, Mm, Op, PdfDocument, PdfFontHandle, PdfPage, PdfSaveOptions, Point, Pt, TextItem,
};
use reqwest::cookie::Jar;
use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, Url};
use scraper::{Html, Selector};
use tokio::sync::Mutex;

use super::flaresolverr::{FlareSolverr, FsSolution};
use super::{BadgeKind, Instrument, MetadataBadge, SearchFilters, SearchResult, Source};

/// When set, UG's Cloudflare-challenged GETs route through FlareSolverr.
/// Shared with MuseScore — one FlareSolverr serves both.
const FLARESOLVERR_ENV: &str = "FLARESOLVERR_URL";

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
const ACCEPT_LANGUAGE: &str = "en-US,en;q=0.9";
const ACCEPT_DOC: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8";
const SEC_CH_UA: &str =
    "\"Chromium\";v=\"140\", \"Not?A_Brand\";v=\"24\", \"Google Chrome\";v=\"140\"";

const TIMEOUT: Duration = Duration::from_secs(20);

/// UG result `type` strings we surface. Deliberately strict (exact,
/// case-insensitive) so we keep guitar chords/tabs but drop "Bass Tabs",
/// "Ukulele Chords", "Drum Tabs", "Guitar Pro" (binary), "Power", "Video"
/// and paid "Official" entries — none of which render as plain guitar text.
fn is_included_type(t: &str) -> bool {
    matches!(t.trim().to_ascii_lowercase().as_str(), "chords" | "tab" | "tabs")
}

pub struct UltimateGuitar {
    http: Client,
    /// Shared cookie jar. `cf_clearance` / `__cf_bm` land here — either
    /// automatically on direct fetches or injected from a FlareSolverr
    /// solution — and replay on subsequent direct calls.
    jar: Arc<Jar>,
    /// Optional FlareSolverr proxy; `Some` when `FLARESOLVERR_URL` is set.
    fs: Option<FlareSolverr>,
    /// Lazily-created FS session id (persistent Chromium context), mirroring
    /// MuseScore. `None` until first create; stays `None` if create fails so
    /// we degrade to sessionless FS calls.
    fs_session: Mutex<Option<String>>,
    /// UA the most recent FS solve reported. `cf_clearance` is bound to
    /// (IP, UA), so the direct-clearance fast path replays this exact UA.
    fs_ua: Mutex<Option<String>>,
}

impl UltimateGuitar {
    pub fn new() -> anyhow::Result<Self> {
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
            HeaderValue::from_static("\"Windows\""),
        );

        let jar = Arc::new(Jar::default());
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(TIMEOUT)
            .gzip(true)
            .default_headers(default_headers)
            .cookie_provider(jar.clone())
            .build()?;

        let fs = match std::env::var(FLARESOLVERR_ENV).ok().filter(|s| !s.is_empty()) {
            Some(url) => {
                tracing::info!(flaresolverr_url = %url, "UltimateGuitar: routing CF-challenged requests through FlareSolverr");
                Some(FlareSolverr::new(url).context("constructing FlareSolverr client")?)
            }
            None => {
                tracing::debug!("UltimateGuitar: FLARESOLVERR_URL unset; direct fetches only");
                None
            }
        };

        Ok(Self {
            http,
            jar,
            fs,
            fs_session: Mutex::new(None),
            fs_ua: Mutex::new(None),
        })
    }

    /// Per-request navigation headers a desktop Chrome sends on a top-level
    /// navigation — Cloudflare weights these heavily.
    fn nav_headers() -> [(HeaderName, HeaderValue); 5] {
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
        ]
    }

    /// See MuseScore's `ensure_fs_session` — same contract, separate FS
    /// session so the two sources don't share a Chromium context.
    async fn ensure_fs_session(&self) -> Option<String> {
        let fs = self.fs.as_ref()?;
        let mut guard = self.fs_session.lock().await;
        if let Some(s) = guard.as_ref() {
            return Some(s.clone());
        }
        let session_id = "ultimateguitar".to_string();
        match fs.create_session(&session_id).await {
            Ok(()) => {
                tracing::info!(session = %session_id, "FlareSolverr session created (UltimateGuitar)");
                *guard = Some(session_id.clone());
                Some(session_id)
            }
            Err(e) => {
                tracing::warn!(
                    error = %format!("{:#}", e),
                    "FlareSolverr session create failed (UltimateGuitar); falling back to sessionless mode"
                );
                None
            }
        }
    }

    /// Fetch a Cloudflare-challenged URL: replay `cf_clearance` directly when
    /// we hold it, otherwise solve through FlareSolverr (and harvest the
    /// cookie + UA for next time). Falls back to a plain direct fetch when FS
    /// is unconfigured. Mirrors MuseScore's proven path.
    async fn fetch_html_challenged(
        &self,
        url: &str,
        ctx_label: &'static str,
    ) -> anyhow::Result<String> {
        match &self.fs {
            Some(fs) => {
                if let Some(html) = self.try_direct_clearance(url, ctx_label).await {
                    return Ok(html);
                }
                let session = self.ensure_fs_session().await;
                let solution: FsSolution = fs
                    .get(url, session.as_deref())
                    .await
                    .with_context(|| format!("flaresolverr {ctx_label} {url}"))?;
                if solution.status >= 400 {
                    anyhow::bail!(
                        "flaresolverr {ctx_label} HTTP {}: {}",
                        solution.status,
                        truncate_for_log(&solution.response, 200)
                    );
                }
                self.absorb_fs_cookies(&solution);
                self.remember_fs_ua(&solution.user_agent).await;
                Ok(solution.response)
            }
            None => {
                let mut req = self.http.get(url);
                for (k, v) in Self::nav_headers() {
                    req = req.header(k, v);
                }
                let resp = req
                    .send()
                    .await
                    .with_context(|| format!("ultimate-guitar {ctx_label} request"))?;
                let status = resp.status();
                if !status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    anyhow::bail!(
                        "ultimate-guitar {ctx_label} HTTP {status}: {}",
                        truncate_for_log(&body, 200)
                    );
                }
                resp.text()
                    .await
                    .with_context(|| format!("ultimate-guitar {ctx_label} body"))
            }
        }
    }

    /// Direct cookie-replay fast path; see MuseScore's `try_direct_clearance`
    /// for the rationale. Returns `Some` only on a clean 200 that isn't a CF
    /// interstitial; any failure returns `None` so the caller falls back.
    async fn try_direct_clearance(&self, url: &str, ctx_label: &'static str) -> Option<String> {
        let ua = self.fs_ua.lock().await.clone()?;
        let mut req = self.http.get(url).header(header::USER_AGENT, ua);
        for (k, v) in Self::nav_headers() {
            req = req.header(k, v);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(ctx = ctx_label, error = %e, "ultimate-guitar direct-clearance transport error; falling back to FlareSolverr");
                return None;
            }
        };
        let status = resp.status();
        if !status.is_success() {
            tracing::debug!(ctx = ctx_label, %status, "ultimate-guitar direct-clearance non-success; falling back to FlareSolverr");
            return None;
        }
        let body = resp.text().await.ok()?;
        if looks_like_cf_challenge(&body) {
            tracing::debug!(ctx = ctx_label, "ultimate-guitar direct-clearance returned a CF interstitial; falling back to FlareSolverr");
            return None;
        }
        tracing::debug!(ctx = ctx_label, "ultimate-guitar direct-clearance hit (skipped FlareSolverr)");
        Some(body)
    }

    async fn remember_fs_ua(&self, ua: &str) {
        if ua.is_empty() {
            return;
        }
        let mut guard = self.fs_ua.lock().await;
        if guard.as_deref() != Some(ua) {
            *guard = Some(ua.to_string());
        }
    }

    /// Inject FlareSolverr-captured cookies into our jar, scoped to UG's
    /// domain, so direct fetches ride the same CF clearance.
    fn absorb_fs_cookies(&self, solution: &FsSolution) {
        let Ok(base) = Url::parse("https://www.ultimate-guitar.com/") else {
            return;
        };
        for c in &solution.cookies {
            let path = if c.path.is_empty() { "/" } else { c.path.as_str() };
            let secure = if c.secure { "; Secure" } else { "" };
            let domain = c.domain.trim_start_matches('.');
            let cookie_str = format!(
                "{}={}; Domain={}; Path={}{}",
                c.name, c.value, domain, path, secure
            );
            self.jar.add_cookie_str(&cookie_str, &base);
        }
    }
}

#[async_trait]
impl Source for UltimateGuitar {
    fn id(&self) -> &'static str {
        "ultimate-guitar"
    }

    fn display_name(&self) -> &'static str {
        "Ultimate Guitar"
    }

    fn external_url(&self, id: &str) -> String {
        // `id` is the base64url-encoded tab URL; decode back to it. Fall back
        // to the UG homepage if the id is somehow malformed.
        decode_id(id).unwrap_or_else(|_| "https://www.ultimate-guitar.com/".to_string())
    }

    async fn search(
        &self,
        query: &str,
        filters: &SearchFilters,
        limit: usize,
    ) -> anyhow::Result<Vec<SearchResult>> {
        // UG is guitar-only. If the user filtered to a different instrument,
        // there's nothing here for them — bail cheaply before any HTTP.
        if let Some(inst) = filters.instrument {
            if inst != Instrument::Guitar {
                return Ok(Vec::new());
            }
        }

        let url = format!(
            "https://www.ultimate-guitar.com/search.php?search_type=title&value={}",
            urlencoding::encode(query)
        );
        let html = self.fetch_html_challenged(&url, "search").await?;

        let data = match extract_js_store(&html) {
            Some(v) => v,
            None => {
                let (has_js_store, has_data_content, has_ugapp, has_window_store) =
                    js_store_fingerprint(&html);
                tracing::warn!(
                    bytes = html.len(),
                    looks_like_cf = looks_like_cf_challenge(&html),
                    has_js_store,
                    has_data_content,
                    has_ugapp,
                    has_window_store,
                    "ultimate-guitar search: js-store data-content not found"
                );
                return Ok(Vec::new());
            }
        };

        let results = match data.pointer("/store/page/data/results").and_then(|v| v.as_array()) {
            Some(arr) => arr,
            None => {
                // js-store parsed but the results pointer missed — almost
                // certainly an upstream shape change. Surface the keys we
                // *did* find at each level so the correct path is a glance
                // away in the logs rather than a guess.
                tracing::warn!(
                    store_keys = %json_keys(data.pointer("/store")),
                    page_keys = %json_keys(data.pointer("/store/page")),
                    data_keys = %json_keys(data.pointer("/store/page/data")),
                    "ultimate-guitar search: js-store parsed but /store/page/data/results absent"
                );
                return Ok(Vec::new());
            }
        };

        let mut out = Vec::with_capacity(limit.min(results.len()));
        for r in results {
            let ttype = r.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if !is_included_type(ttype) {
                continue;
            }
            // Skip paid / non-public entries we can't render.
            if let Some(access) = r.get("tab_access_type").and_then(|v| v.as_str()) {
                if access != "public" {
                    continue;
                }
            }
            let (Some(tab_url), Some(song)) = (
                r.get("tab_url").and_then(|v| v.as_str()),
                r.get("song_name").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            let artist = r.get("artist_name").and_then(|v| v.as_str()).unwrap_or("");
            let title = if artist.is_empty() {
                song.to_string()
            } else {
                format!("{song} — {artist}")
            };

            let mut metadata = vec![MetadataBadge {
                label: ttype.trim().to_string(),
                kind: BadgeKind::Generic,
            }];
            if let Some(key) = r
                .get("tonality_name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                metadata.push(MetadataBadge {
                    label: format!("Key: {key}"),
                    kind: BadgeKind::Key,
                });
            }
            if let Some(ver) = r.get("version").and_then(|v| v.as_u64()).filter(|v| *v > 1) {
                metadata.push(MetadataBadge {
                    label: format!("ver. {ver}"),
                    kind: BadgeKind::Generic,
                });
            }

            out.push(SearchResult {
                source: self.id().to_string(),
                id: encode_id(tab_url),
                title,
                description: None,
                external_url: tab_url.to_string(),
                thumbnail_url: None,
                metadata,
                complexity: None,
                // Tabs are user transcriptions of (almost always) copyrighted
                // songs — not public-domain works.
                is_public_domain: Some(false),
                // The official/community split is MuseScore-only; UG community
                // tabs read as "community" (None) under the score-type filter.
                is_official: None,
            });
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    async fn fetch_pdf_bytes(&self, id: &str, max_bytes: usize) -> anyhow::Result<Vec<u8>> {
        let tab_url = decode_id(id).context("decoding ultimate-guitar tab id")?;
        let html = self.fetch_html_challenged(&tab_url, "tab").await?;
        let data = extract_js_store(&html)
            .ok_or_else(|| anyhow::anyhow!("ultimate-guitar tab: js-store not found on {tab_url}"))?;

        let content = data
            .pointer("/store/page/data/tab_view/wiki_tab/content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("ultimate-guitar tab: no wiki_tab content on {tab_url}"))?;

        // Title line for the rendered PDF, from the tab metadata when present.
        let song = data
            .pointer("/store/page/data/tab/song_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let artist = data
            .pointer("/store/page/data/tab/artist_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let title = match (song.is_empty(), artist.is_empty()) {
            (false, false) => format!("{song} — {artist}"),
            (false, true) => song.to_string(),
            _ => "Ultimate Guitar Tab".to_string(),
        };

        let body = clean_tab_text(content);
        let pdf = render_text_pdf(&title, &body).context("rendering tab to PDF")?;
        anyhow::ensure!(
            pdf.len() <= max_bytes,
            "rendered tab PDF {} bytes exceeds limit {}",
            pdf.len(),
            max_bytes
        );
        Ok(pdf)
    }
}

/// Recover UG's hydration JSON, shaped as `{"store": …}` so the downstream
/// `/store/page/...` pointers are identical regardless of which embed
/// variant the page shipped. Tried in order:
///
///   1. `<div class="js-store" data-content="…">` — the server-rendered
///      embed; html5ever decodes the entity-escaped attribute back to real
///      JSON. This is the only form a *direct* (no-FlareSolverr) fetch sees.
///   2. `window.UGAPP.store = { … };` inside a `<script>` — what's left once
///      UG hydrates and removes the div, i.e. the post-JS DOM FlareSolverr
///      hands back. We recover the object literal with a brace-balanced scan
///      that skips string contents, then re-wrap it as `{"store": <obj>}`.
///   3. `window.UGAPP.store = JSON.parse("…");` — variant of (2) where the
///      store is a JSON string literal; we JS-decode the literal then parse.
fn extract_js_store(html: &str) -> Option<serde_json::Value> {
    extract_js_store_from_div(html).or_else(|| extract_js_store_from_script(html))
}

/// Variant 1: the `data-content` attribute of `<div class="js-store">`.
fn extract_js_store_from_div(html: &str) -> Option<serde_json::Value> {
    let doc = Html::parse_document(html);
    let selector = Selector::parse("div.js-store").ok()?;
    let raw = doc
        .select(&selector)
        .next()?
        .value()
        .attr("data-content")?;
    serde_json::from_str(raw).ok()
}

/// Variants 2 & 3: recover the store from the `window.UGAPP.store = …`
/// assignment in an inline script and re-wrap it as `{"store": …}`.
fn extract_js_store_from_script(html: &str) -> Option<serde_json::Value> {
    let rhs = find_assignment(html, "window.UGAPP.store")?.trim_start();

    let store: serde_json::Value = if let Some(after) = rhs.strip_prefix("JSON.parse(") {
        // `JSON.parse("…escaped JSON…")`: decode the JS string literal to the
        // underlying JSON text, then parse that.
        let json_text = js_string_literal(after.trim_start())?;
        serde_json::from_str(&json_text).ok()?
    } else {
        // Bare object literal: balance braces (skipping string contents) and
        // parse the slice.
        serde_json::from_str(balanced_braces(rhs)?).ok()?
    };

    Some(serde_json::json!({ "store": store }))
}

/// Return the text immediately after `<lhs> =` for the first occurrence of
/// `lhs` that is actually an assignment. Tolerates whitespace around `=` and
/// skips look-alikes (e.g. `window.UGAPP.store.page`, which is followed by
/// `.page` not `=`).
fn find_assignment<'a>(html: &'a str, lhs: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(rel) = html[from..].find(lhs) {
        let end = from + rel + lhs.len();
        if let Some(rest) = html[end..].trim_start().strip_prefix('=') {
            return Some(rest);
        }
        from = end;
    }
    None
}

/// Given text that contains a `{`, return the slice spanning the first
/// brace-balanced object, or `None` if the braces never balance. String
/// literals (`"…"` / `'…'`) are skipped so braces inside strings don't throw
/// off the depth count; backslash escapes inside strings are honoured.
fn balanced_braces(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;
    let mut depth = 0usize;
    let mut in_str: Option<u8> = None;
    let mut escaped = false;
    for i in start..bytes.len() {
        let c = bytes[i];
        if let Some(quote) = in_str {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == quote {
                in_str = None;
            }
            continue;
        }
        match c {
            b'"' | b'\'' => in_str = Some(c),
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    // `{`, `}` and the scanned quotes are all ASCII, so every
                    // index we slice at is a valid char boundary.
                    return Some(&s[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Decode a single JS string literal at the start of `s` (which must begin
/// with `"` or `'`), returning its contents. Handles the escapes a
/// `JSON.parse` payload actually carries (`\"`, `\\`, `\/`, `\n`, `\r`,
/// `\t`, `\b`, `\f`, `\uXXXX` including surrogate pairs). `None` if `s`
/// doesn't open with a quote or the literal is unterminated/malformed.
fn js_string_literal(s: &str) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    let quote = *chars.first()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let mut out = String::new();
    let mut i = 1;
    while i < chars.len() {
        let c = chars[i];
        if c == quote {
            return Some(out);
        }
        if c != '\\' {
            out.push(c);
            i += 1;
            continue;
        }
        i += 1;
        match *chars.get(i)? {
            '"' => out.push('"'),
            '\'' => out.push('\''),
            '\\' => out.push('\\'),
            '/' => out.push('/'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'b' => out.push('\u{0008}'),
            'f' => out.push('\u{000C}'),
            'u' => {
                let hi = read_hex4(&chars, i + 1)?;
                i += 4;
                if (0xD800..=0xDBFF).contains(&hi) {
                    // High surrogate — the low half must follow as `\uXXXX`.
                    if chars.get(i + 1) == Some(&'\\') && chars.get(i + 2) == Some(&'u') {
                        let lo = read_hex4(&chars, i + 3)?;
                        i += 6;
                        let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                        out.push(char::from_u32(cp)?);
                    } else {
                        return None;
                    }
                } else {
                    out.push(char::from_u32(hi)?);
                }
            }
            other => out.push(other),
        }
        i += 1;
    }
    None
}

/// Parse four hex digits starting at `chars[at]` into a code unit.
fn read_hex4(chars: &[char], at: usize) -> Option<u32> {
    let hex: String = chars.get(at..at + 4)?.iter().collect();
    u32::from_str_radix(&hex, 16).ok()
}

/// Cheap structural fingerprint of a UG page, logged when `extract_js_store`
/// comes up empty so a single prod line pins down which embed variant (if
/// any) the page used: `(has_js_store, has_data_content, has_ugapp,
/// has_window_store)`.
fn js_store_fingerprint(html: &str) -> (bool, bool, bool, bool) {
    (
        html.contains("js-store"),
        html.contains("data-content"),
        html.contains("UGAPP"),
        html.contains("window.UGAPP.store"),
    )
}

/// Strip UG's inline markup and normalise newlines into plain text suitable
/// for monospaced rendering. `[tab]…[/tab]` wrap tablature blocks and
/// `[ch]…[/ch]` wrap chord tokens; both markers are dropped, leaving the
/// chord names and tab lines inline where they belong.
fn clean_tab_text(raw: &str) -> String {
    let stripped = raw
        .replace("[tab]", "")
        .replace("[/tab]", "")
        .replace("[ch]", "")
        .replace("[/ch]", "")
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    html_escape::decode_html_entities(&stripped).into_owned()
}

/// base64url (no padding) of the tab URL, used as the source-native id so it
/// round-trips through `/pdf/:source/:id` path params without slashes. UG
/// tab URLs embed an artist/song slug we can't reconstruct from the numeric
/// id alone, so we carry the whole URL.
fn encode_id(tab_url: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(tab_url.as_bytes())
}

fn decode_id(id: &str) -> anyhow::Result<String> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(id)
        .context("base64url decode")?;
    let url = String::from_utf8(bytes).context("tab id not utf-8")?;
    // Defence in depth: only ever hand back a UG URL, never an arbitrary one
    // an attacker base64-encoded into the path.
    anyhow::ensure!(
        url.starts_with("https://tabs.ultimate-guitar.com/")
            || url.starts_with("https://www.ultimate-guitar.com/"),
        "decoded id is not an ultimate-guitar URL"
    );
    Ok(url)
}

/// Comma-joined top-level keys of a JSON object, for diagnostics when an
/// expected pointer misses. `"<none>"` when the value is absent or not an
/// object.
fn json_keys(v: Option<&serde_json::Value>) -> String {
    match v.and_then(|v| v.as_object()) {
        Some(map) => map.keys().cloned().collect::<Vec<_>>().join(","),
        None => "<none>".to_string(),
    }
}

/// Heuristic for a Cloudflare interstitial (expired/absent `cf_clearance`).
fn looks_like_cf_challenge(html: &str) -> bool {
    html.contains("Just a moment") || html.contains("Attention Required")
}

/// Truncate a string to at most `max` bytes on a char boundary, for logs.
fn truncate_for_log(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    s[..end].to_string()
}

/// Lay monospaced text out into an A4 PDF using printpdf's built-in Courier
/// (font F1–F14, no embedding needed). Long lines are hard-wrapped and the
/// body paginates across as many pages as needed.
fn render_text_pdf(title: &str, body: &str) -> anyhow::Result<Vec<u8>> {
    const PAGE_W_MM: f32 = 210.0; // A4 portrait
    const PAGE_H_MM: f32 = 297.0;
    const MARGIN_MM: f32 = 15.0;
    const FONT_SIZE_PT: f32 = 8.5;
    const LINE_HEIGHT_PT: f32 = 10.5;
    const PT_PER_MM: f32 = 1.0 / 0.352_778;
    // Courier advance width is 0.6 em. Usable width / glyph width = max chars.
    let usable_w_pt = (PAGE_W_MM - 2.0 * MARGIN_MM) * PT_PER_MM;
    let glyph_w_pt = FONT_SIZE_PT * 0.6;
    let max_chars = (usable_w_pt / glyph_w_pt).floor().max(20.0) as usize;

    // Build the final list of physical lines: a title, a blank spacer, then
    // the hard-wrapped body.
    let mut lines: Vec<String> = vec![title.to_string(), String::new()];
    for raw in body.split('\n') {
        if raw.is_empty() {
            lines.push(String::new());
            continue;
        }
        let chars: Vec<char> = raw.chars().collect();
        let mut start = 0;
        while start < chars.len() {
            let end = (start + max_chars).min(chars.len());
            lines.push(chars[start..end].iter().collect());
            start = end;
        }
    }

    let usable_h_mm = PAGE_H_MM - 2.0 * MARGIN_MM;
    let line_h_mm = LINE_HEIGHT_PT * 0.352_778;
    let lines_per_page = (usable_h_mm / line_h_mm).floor().max(1.0) as usize;

    let font = PdfFontHandle::Builtin(BuiltinFont::Courier);
    let mut doc = PdfDocument::new("Ultimate Guitar Tab");
    let mut pages = Vec::new();
    for chunk in lines.chunks(lines_per_page) {
        let mut ops = vec![
            Op::StartTextSection,
            Op::SetFont {
                font: font.clone(),
                size: Pt(FONT_SIZE_PT),
            },
            Op::SetLineHeight {
                lh: Pt(LINE_HEIGHT_PT),
            },
            Op::SetTextCursor {
                pos: Point::new(Mm(MARGIN_MM), Mm(PAGE_H_MM - MARGIN_MM)),
            },
        ];
        for line in chunk {
            ops.push(Op::ShowText {
                items: vec![TextItem::Text(line.clone())],
            });
            ops.push(Op::AddLineBreak);
        }
        ops.push(Op::EndTextSection);
        pages.push(PdfPage::new(Mm(PAGE_W_MM), Mm(PAGE_H_MM), ops));
    }
    if pages.is_empty() {
        pages.push(PdfPage::new(Mm(PAGE_W_MM), Mm(PAGE_H_MM), Vec::new()));
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
    fn id_roundtrips() {
        let url = "https://tabs.ultimate-guitar.com/tab/oasis/wonderwall-chords-27596";
        let id = encode_id(url);
        assert!(!id.contains('/'), "id must be path-safe");
        assert_eq!(decode_id(&id).unwrap(), url);
    }

    #[test]
    fn decode_rejects_non_ug_url() {
        let evil = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"https://evil.example/x");
        assert!(decode_id(&evil).is_err());
    }

    #[test]
    fn included_types_are_guitar_text_only() {
        for t in ["Chords", "chords", "Tab", "Tabs", " tabs "] {
            assert!(is_included_type(t), "{t} should be included");
        }
        for t in ["Guitar Pro", "Bass Tabs", "Ukulele Chords", "Drum Tabs", "Pro", "Video", "Official", ""] {
            assert!(!is_included_type(t), "{t} should be excluded");
        }
    }

    #[test]
    fn clean_strips_markup_and_normalises_newlines() {
        let raw = "[ch]Am[/ch] [ch]G[/ch]\r\n[tab]e|--0--|[/tab]\r\nAmpersand &amp; co";
        let cleaned = clean_tab_text(raw);
        assert_eq!(cleaned, "Am G\ne|--0--|\nAmpersand & co");
        assert!(!cleaned.contains("[ch]") && !cleaned.contains("[tab]"));
    }

    #[test]
    fn renders_a_nonempty_pdf() {
        let pdf = render_text_pdf("Song — Artist", "line one\nline two").unwrap();
        assert!(pdf.starts_with(b"%PDF-"), "should be a PDF");
        assert!(pdf.len() > 200);
    }

    #[test]
    fn extract_js_store_reads_data_content() {
        // html5ever decodes the entity-escaped attribute back to JSON.
        let html = r#"<html><body><div class="js-store" data-content="{&quot;store&quot;:{&quot;page&quot;:{&quot;data&quot;:{&quot;results&quot;:[]}}}}"></div></body></html>"#;
        let v = extract_js_store(html).expect("should parse");
        assert!(v.pointer("/store/page/data/results").unwrap().is_array());
    }

    #[test]
    fn extract_js_store_recovers_from_script_object() {
        // The hydrated form FlareSolverr returns: no div, store assigned in a
        // script. A `}{` lives inside a string value to prove the brace scan
        // honours string literals.
        let html = r#"<html><head><script>
            window.UGAPP = window.UGAPP || {};
            window.UGAPP.store = {"page":{"data":{"results":[{"song_name":"X}{"}]}}};
            window.UGAPP.store.page.template = "search";
        </script></head><body></body></html>"#;
        let v = extract_js_store(html).expect("should recover from script");
        assert!(v.pointer("/store/page/data/results").unwrap().is_array());
        assert_eq!(
            v.pointer("/store/page/data/results/0/song_name")
                .and_then(|x| x.as_str()),
            Some("X}{"),
            "a brace inside a string must not truncate the object"
        );
    }

    #[test]
    fn extract_js_store_recovers_from_json_parse() {
        // `window.UGAPP.store = JSON.parse("…")`: the store is a JSON string
        // literal. `\\n` in the literal decodes to a `\n` JSON escape, which
        // serde then turns into a real newline in the content.
        let html = r#"<script>window.UGAPP.store = JSON.parse("{\"page\":{\"data\":{\"tab_view\":{\"wiki_tab\":{\"content\":\"line\\ntwo\"}}}}}");</script>"#;
        let v = extract_js_store(html).expect("should recover from JSON.parse");
        assert_eq!(
            v.pointer("/store/page/data/tab_view/wiki_tab/content")
                .and_then(|x| x.as_str()),
            Some("line\ntwo")
        );
    }

    #[test]
    fn div_form_takes_precedence_over_script() {
        // When both are present the server-rendered div wins (it's the
        // authoritative pre-hydration blob).
        let html = r#"<div class="js-store" data-content="{&quot;store&quot;:{&quot;page&quot;:{&quot;tag&quot;:&quot;div&quot;}}}"></div>
            <script>window.UGAPP.store = {"page":{"tag":"script"}};</script>"#;
        let v = extract_js_store(html).expect("should parse");
        assert_eq!(
            v.pointer("/store/page/tag").and_then(|x| x.as_str()),
            Some("div")
        );
    }

    #[test]
    fn balanced_braces_skips_string_contents() {
        assert_eq!(
            balanced_braces(r#"  {"a":"}{","b":{"c":1}} trailing;"#),
            Some(r#"{"a":"}{","b":{"c":1}}"#)
        );
        assert_eq!(balanced_braces("no brace here"), None);
        assert_eq!(balanced_braces("{ never closes "), None);
        // A `}` hidden inside a single-quoted string must not close early.
        assert_eq!(balanced_braces("{a:'}'}"), Some("{a:'}'}"));
    }

    #[test]
    fn js_string_literal_decodes_escapes() {
        // In these raw strings every `\` is a literal backslash, so the input
        // is exactly the JS source the decoder must handle.
        assert_eq!(
            js_string_literal(r#""a\nb\tcA\"d\\e""#).as_deref(),
            Some("a\nb\tcA\"d\\e")
        );
        // BMP `\uXXXX` escape — input is the 8 chars `"xAy"` (U+0041 = 'A').
        assert_eq!(js_string_literal("\"x\\u0041y\"").as_deref(), Some("xAy"));
        // Surrogate pair `😀` decodes to U+1F600 😀.
        assert_eq!(
            js_string_literal("\"\\uD83D\\uDE00\"").as_deref(),
            Some("😀")
        );
        // Stops at the closing quote, ignoring trailing source.
        assert_eq!(js_string_literal(r#""hi");more"#).as_deref(), Some("hi"));
        assert_eq!(js_string_literal("not a string"), None);
    }

    #[test]
    fn fingerprint_distinguishes_variants() {
        let div = r#"<div class="js-store" data-content="{}"></div>"#;
        assert_eq!(js_store_fingerprint(div), (true, true, false, false));

        let script = r#"<script>window.UGAPP.store = {};</script>"#;
        assert_eq!(js_store_fingerprint(script), (false, false, true, true));
    }
}
