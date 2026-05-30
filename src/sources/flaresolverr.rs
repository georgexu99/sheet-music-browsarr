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

/// Wall-clock cap on the FlareSolverr HTTP call itself. Observed FS
/// solve times under modest load: 5–30 s with sessions, 30–60 s
/// without. 45 s threads the needle: enough headroom for a slow
/// solve to land, but short enough that a truly wedged FS surfaces
/// as a failure within a minute rather than dragging out a search.
const FS_TIMEOUT: Duration = Duration::from_secs(45);

/// `maxTimeout` field passed to FlareSolverr — the budget *it* applies to
/// solving the challenge. Match the HTTP-side timeout so the failure
/// mode is a single clean error rather than a layered double-timeout.
const FS_MAX_TIMEOUT_MS: u64 = 45_000;

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
    /// Cloudflare binds the cookie to (IP, UA). The MuseScore source
    /// stashes this (`fs_ua`) and sends it on its direct-clearance fast
    /// path so the replayed cookie validates.
    #[serde(rename = "userAgent")]
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

/// Request body for `request.get`. `session` is omitted when None so
/// FlareSolverr falls back to its default ephemeral browser context.
#[derive(Serialize)]
struct FsRequest<'a> {
    cmd: &'a str,
    url: &'a str,
    #[serde(rename = "maxTimeout")]
    max_timeout: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<&'a str>,
}

/// Request body for `sessions.create` / `sessions.destroy`. Both commands
/// take the same shape (just a session ID), so the struct is shared.
#[derive(Serialize)]
struct FsSessionCmd<'a> {
    cmd: &'a str,
    session: &'a str,
}

/// Typed error for FlareSolverr `request.get` calls. The caller cares
/// about two cases: (a) the session it referenced no longer exists
/// (FS restarted, nightly refresh destroyed it, or a session leaked
/// memory and was reaped), so it should recreate and retry; (b)
/// everything else, which is just bubbled up.
#[derive(Debug)]
pub enum FsError {
    /// FS reported the session ID is unknown. The string is the
    /// session ID that was rejected so the caller can null out the
    /// matching pool slot before retrying.
    SessionMissing { session: String },
    /// FS returned a non-`ok` envelope status that wasn't a missing
    /// session — e.g., challenge timeout, browser crash, malformed URL.
    Status {
        status: String,
        message: Option<String>,
    },
    /// FS returned `ok` but no solution payload. Should never happen
    /// in practice; kept distinct so the operator sees the exact
    /// failure mode in logs.
    NoSolution,
    /// HTTP/transport failure talking to FS itself (connect refused,
    /// timeout, bad JSON). Wraps the underlying error.
    Other(anyhow::Error),
}

impl std::fmt::Display for FsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionMissing { session } => {
                write!(f, "flaresolverr session missing: {session}")
            }
            Self::Status { status, message } => {
                write!(
                    f,
                    "flaresolverr returned status={status} message={message:?}"
                )
            }
            Self::NoSolution => write!(f, "flaresolverr returned no solution"),
            Self::Other(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for FsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Other(e) => e.source(),
            _ => None,
        }
    }
}

impl FlareSolverr {
    pub fn new(base_url: String) -> anyhow::Result<Self> {
        let http = Client::builder().timeout(FS_TIMEOUT).build()?;
        let base_url = base_url.trim_end_matches('/').to_string();
        Ok(Self { http, base_url })
    }

    /// Create a persistent FlareSolverr session. The session keeps a
    /// Chromium browser context alive between calls so subsequent
    /// `request.get` calls referencing the same `session` skip the
    /// cold-start cost and reuse cookies (including `cf_clearance`).
    /// Without sessions, MuseScore's 4-variant fan-out spins up 4
    /// parallel cold Chromium instances per query, which routinely
    /// overloads FS and times out.
    pub async fn create_session(&self, session: &str) -> anyhow::Result<()> {
        let endpoint = format!("{}/v1", self.base_url);
        let body = FsSessionCmd {
            cmd: "sessions.create",
            session,
        };
        let env: FsEnvelope = self
            .http
            .post(&endpoint)
            .json(&body)
            .send()
            .await
            .context("flaresolverr sessions.create request")?
            .error_for_status()
            .context("flaresolverr sessions.create HTTP status")?
            .json()
            .await
            .context("flaresolverr sessions.create json")?;
        if env.status != "ok" {
            anyhow::bail!(
                "flaresolverr sessions.create status={} message={:?}",
                env.status,
                env.message
            );
        }
        Ok(())
    }

    /// Destroy a previously-created session. Best-effort: a "session
    /// doesn't exist" reply is treated as success since the postcondition
    /// (no such session on FS) is what we wanted anyway. Used by the
    /// nightly refresh loop before recreating a slot with the same name,
    /// and as a hygiene measure if we ever wire in graceful shutdown.
    pub async fn destroy_session(&self, session: &str) -> anyhow::Result<()> {
        let endpoint = format!("{}/v1", self.base_url);
        let body = FsSessionCmd {
            cmd: "sessions.destroy",
            session,
        };
        let env: FsEnvelope = self
            .http
            .post(&endpoint)
            .json(&body)
            .send()
            .await
            .context("flaresolverr sessions.destroy request")?
            .error_for_status()
            .context("flaresolverr sessions.destroy HTTP status")?
            .json()
            .await
            .context("flaresolverr sessions.destroy json")?;
        if env.status == "ok" {
            return Ok(());
        }
        // FS replies with `error` + a message like "Session 'foo' doesn't
        // exist." when we destroy a session that was already gone. That's
        // not a real failure for our purposes — the slot is already in the
        // state we wanted.
        if let Some(msg) = &env.message {
            if is_missing_session_message(msg) {
                return Ok(());
            }
        }
        anyhow::bail!(
            "flaresolverr sessions.destroy status={} message={:?}",
            env.status,
            env.message
        );
    }

    /// Issue a GET through FlareSolverr. Bubbles up the FS error on
    /// non-`ok` status so the caller's error path treats CF failures
    /// uniformly with direct-fetch failures. When `session` is Some,
    /// FS reuses the persistent browser context created earlier via
    /// `create_session` — drastically faster than the default
    /// ephemeral mode for repeated calls.
    pub async fn get(&self, url: &str, session: Option<&str>) -> Result<FsSolution, FsError> {
        let endpoint = format!("{}/v1", self.base_url);
        let body = FsRequest {
            cmd: "request.get",
            url,
            max_timeout: FS_MAX_TIMEOUT_MS,
            session,
        };
        let env: FsEnvelope = self
            .http
            .post(&endpoint)
            .json(&body)
            .send()
            .await
            .context("flaresolverr request")
            .map_err(FsError::Other)?
            .error_for_status()
            .context("flaresolverr HTTP status")
            .map_err(FsError::Other)?
            .json()
            .await
            .context("flaresolverr response json")
            .map_err(FsError::Other)?;
        if env.status != "ok" {
            // The most actionable failure mode is "session doesn't
            // exist": the nightly refresh just deleted our slot, or FS
            // restarted, or our session leaked. Surface it distinctly
            // so the caller can null the slot and retry with a fresh
            // session ID — much faster than failing the user request.
            if let (Some(session_id), Some(msg)) = (session, &env.message) {
                if is_missing_session_message(msg) {
                    return Err(FsError::SessionMissing {
                        session: session_id.to_string(),
                    });
                }
            }
            return Err(FsError::Status {
                status: env.status,
                message: env.message,
            });
        }
        env.solution.ok_or(FsError::NoSolution)
    }
}

/// FlareSolverr's reply for an unknown session looks like
/// `"Error: This session does not exist."` (with minor wording variation
/// across versions — "doesn't exist", "not found", etc.). Match
/// case-insensitively on the family rather than the exact string so a
/// future FS rewording doesn't silently turn a recoverable failure into
/// a hard error.
fn is_missing_session_message(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("session")
        && (m.contains("does not exist")
            || m.contains("doesn't exist")
            || m.contains("not found")
            || m.contains("no such session"))
}
