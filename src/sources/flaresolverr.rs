//! FlareSolverr proxy helper for the MuseScore source.
//!
//! MuseScore.com sits behind Cloudflare, and reqwest's TLS fingerprint /
//! header order are recognisable enough that even a fresh Chrome UA plus
//! browser-shaped headers can still get the "Just a moment…" interactive
//! challenge (HTTP 403). FlareSolverr is a small companion service (Docker
//! container, port 8191) that spins up a real headless Chromium, navigates
//! to the URL, waits for the JS challenge to clear, and returns the
//! post-challenge HTML plus the issued `cf_clearance` cookie.
//!
//! Wiring is opt-in via the `FLARESOLVERR_URL` env var (e.g.
//! `http://flaresolverr:8191`). When unset, MuseScore makes direct
//! requests as before. When set, the score-page and search-page fetches
//! route through here; cookies harvested from the response get injected
//! into MuseScore's shared cookie jar so the subsequent direct fetches
//! (bundle JS, /api/jmuse, CDN PNGs) ride the same CF clearance.
//!
//! Cookie replay caveat: `cf_clearance` is bound to the (IP, UA) tuple.
//! Replay works when our app's container and the FlareSolverr container
//! share the same egress NAT (typical on a single home server) AND we use
//! the UA that FlareSolverr's bundled Chromium reports. The caller is
//! responsible for honouring `FsSolution::user_agent` on direct fetches.

use std::time::Duration;

use anyhow::Context;
use reqwest::Client;
use serde::{Deserialize, Serialize};

/// Wall-clock cap on the FlareSolverr HTTP call itself. Headless Chromium
/// startup + challenge solving typically completes in 5–15s; 60s is the
/// safety net for cold-start or a slow challenge.
const FS_TIMEOUT: Duration = Duration::from_secs(60);

/// `maxTimeout` field passed to FlareSolverr — the budget *it* applies to
/// solving the challenge. Matches our HTTP-side timeout so the failure
/// mode is a single clean error rather than a layered double-timeout.
const FS_MAX_TIMEOUT_MS: u64 = 60_000;

#[derive(Clone)]
pub struct FlareSolverr {
    http: Client,
    base_url: String,
}

#[derive(Debug, Deserialize)]
struct FsEnvelope {
    status: String,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    solution: Option<FsSolution>,
}

/// The successful response shape returned by FlareSolverr's `request.get`.
/// Fields we don't need (headers, response_url metadata) are dropped.
#[derive(Debug, Deserialize)]
pub struct FsSolution {
    /// Final URL after any redirects FS followed. Kept on the struct
    /// for diagnostics even though no current caller reads it.
    #[serde(default)]
    #[allow(dead_code)]
    pub url: String,
    /// HTTP status code of the upstream response, post-challenge.
    pub status: u16,
    /// The post-challenge HTML body.
    pub response: String,
    /// The UA FlareSolverr's bundled Chromium used. Callers replaying
    /// `cf_clearance` on direct fetches MUST set this same UA, since
    /// Cloudflare binds the cookie to (IP, UA). Not yet honoured by
    /// the MuseScore source — kept here for the future fix.
    #[serde(rename = "userAgent")]
    #[allow(dead_code)]
    pub user_agent: String,
    /// All cookies the upstream set during the resolved navigation,
    /// including `cf_clearance` and `__cf_bm`.
    #[serde(default)]
    pub cookies: Vec<FsCookie>,
}

#[derive(Debug, Deserialize)]
pub struct FsCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub secure: bool,
    /// FlareSolverr emits `expires` as a Unix-epoch float (seconds). We
    /// don't enforce it ourselves — the reqwest cookie jar handles
    /// expiry. Field kept for diagnostic logging only.
    #[serde(default)]
    #[allow(dead_code)]
    pub expires: f64,
}

#[derive(Serialize)]
struct FsRequest<'a> {
    cmd: &'a str,
    url: &'a str,
    #[serde(rename = "maxTimeout")]
    max_timeout: u64,
}

impl FlareSolverr {
    pub fn new(base_url: String) -> anyhow::Result<Self> {
        let http = Client::builder().timeout(FS_TIMEOUT).build()?;
        let base_url = base_url.trim_end_matches('/').to_string();
        Ok(Self { http, base_url })
    }

    /// Issue a GET through FlareSolverr. Bubbles up the FS error on
    /// non-`ok` status so the caller's error path treats CF failures
    /// uniformly with direct-fetch failures.
    pub async fn get(&self, url: &str) -> anyhow::Result<FsSolution> {
        let endpoint = format!("{}/v1", self.base_url);
        let body = FsRequest {
            cmd: "request.get",
            url,
            max_timeout: FS_MAX_TIMEOUT_MS,
        };
        let env: FsEnvelope = self
            .http
            .post(&endpoint)
            .json(&body)
            .send()
            .await
            .context("flaresolverr request")?
            .error_for_status()
            .context("flaresolverr HTTP status")?
            .json()
            .await
            .context("flaresolverr response json")?;
        if env.status != "ok" {
            anyhow::bail!(
                "flaresolverr returned status={} message={:?}",
                env.status,
                env.message
            );
        }
        env.solution
            .ok_or_else(|| anyhow::anyhow!("flaresolverr returned no solution"))
    }
}
