use reqwest::Client;

const VERIFY_URL: &str = "https://challenges.cloudflare.com/turnstile/v0/siteverify";

/// Verify a Cloudflare Turnstile token. Returns true on success.
/// Silent on network/JSON errors — they're treated as failed verification,
/// which is the safe default for an abuse-prevention layer.
pub async fn verify(http: &Client, secret: &str, token: &str, ip: Option<&str>) -> bool {
    if secret.is_empty() || token.is_empty() {
        return false;
    }

    let mut form: Vec<(&str, &str)> = vec![("secret", secret), ("response", token)];
    if let Some(ip) = ip {
        form.push(("remoteip", ip));
    }

    let resp = match http.post(VERIFY_URL).form(&form).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "turnstile verify request failed");
            return false;
        }
    };

    if !resp.status().is_success() {
        tracing::warn!(status = %resp.status(), "turnstile verify non-2xx");
        return false;
    }

    let json: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "turnstile verify json parse");
            return false;
        }
    };

    json.get("success")
        .and_then(|s| s.as_bool())
        .unwrap_or(false)
}
