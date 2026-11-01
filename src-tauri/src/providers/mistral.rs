//! Mistral Vibe — the Vibe coding allowance of a Le Chat / Mistral plan.
//! Mistral exposes that allowance only to its web console (the Vibe CLI's
//! own key or OIDC login can read the plan name, not the quota), so this
//! card needs the console session: the user pastes the `Cookie:` header of
//! a console.mistral.ai request in Settings. Only the `csrftoken` and
//! `ory_session_*` pairs are kept, and they go only to console.mistral.ai
//! with redirects off — an expired session answers 302 to auth.mistral.ai,
//! and the cookie must not follow it there. Same tRPC call CodexBar's
//! Mistral provider falls back to.

use super::{http_no_redirect, json_body, stored_api_key, Metric, Snapshot};
use serde_json::Value;

const ID: &str = "mistral";
const NAME: &str = "Mistral Vibe";
/// `billing.vibeUsage` with the empty batch input the console itself sends.
const VIBE_USAGE_URL: &str = "https://console.mistral.ai/api-ui/trpc/billing.vibeUsage?batch=1&input=%7B%220%22%3A%7B%22json%22%3Anull%2C%22meta%22%3A%7B%22values%22%3A%5B%22undefined%22%5D%2C%22v%22%3A1%7D%7D%7D";
const MAX_BODY_BYTES: usize = 256 * 1024;

pub async fn snapshot() -> Snapshot {
    match fetch().await {
        Ok(s) => s,
        Err(e) => Snapshot::error(ID, NAME, e),
    }
}

/// The console cookie pairs from a pasted `Cookie:` header (or a bare
/// `name=value; …` list) plus the CSRF token the request must echo.
/// `None` unless both a csrftoken and an ory_session_* pair are present.
fn console_cookie(raw: &str) -> Option<(String, String)> {
    let raw = raw.trim();
    let raw = match raw.get(..7) {
        Some(prefix) if prefix.eq_ignore_ascii_case("cookie:") => &raw[7..],
        _ => raw,
    };
    let mut csrf = None;
    let mut pairs = Vec::new();
    for part in raw.split(';') {
        let Some((name, value)) = part.split_once('=') else {
            continue;
        };
        let (name, value) = (name.trim(), value.trim());
        if value.is_empty() || value.contains(|c: char| c == ',' || c.is_control() || c.is_whitespace()) {
            continue;
        }
        if name == "csrftoken" {
            csrf = Some(value.to_string());
            pairs.push(format!("{name}={value}"));
        } else if name.starts_with("ory_session_") {
            pairs.push(format!("{name}={value}"));
        }
    }
    let csrf = csrf?;
    if !pairs.iter().any(|p| p.starts_with("ory_session_")) {
        return None;
    }
    Some((pairs.join("; "), csrf))
}

async fn fetch() -> Result<Snapshot, String> {
    let Some(raw) = stored_api_key(ID, &[]) else {
        return Ok(Snapshot::no_credentials(
            ID,
            NAME,
            "Paste the Cookie header of a console.mistral.ai request in Settings (gear icon) — Mistral shows the Vibe allowance only to its web console.",
        ));
    };
    let Some((cookie, csrf)) = console_cookie(&raw) else {
        return Err("saved cookie needs both csrftoken and ory_session_… — copy the whole Cookie header from console.mistral.ai".into());
    };

    let resp = http_no_redirect()
        .get(VIBE_USAGE_URL)
        .header("cookie", cookie)
        .header("x-csrftoken", csrf)
        .header("accept", "*/*")
        .send()
        .await
        .map_err(|e| format!("Vibe usage request: {}", e.without_url()))?;
    let status = resp.status();
    if status.is_redirection() || matches!(status.as_u16(), 401 | 403) {
        return Err("console session expired — paste a fresh Cookie header from console.mistral.ai".into());
    }
    if !status.is_success() {
        return Err(format!("Vibe usage endpoint: HTTP {status}"));
    }
    let doc = json_body(resp, MAX_BODY_BYTES, "Vibe usage").await?;
    parse(&doc)
}

/// `[{"result":{"data":{"json":{"usage_percentage":12.5,"reset_at":"…"}}}}]`
fn parse(doc: &Value) -> Result<Snapshot, String> {
    let json = doc
        .pointer("/0/result/data/json")
        .filter(|j| j.is_object())
        .ok_or("Vibe usage response has no data")?;
    let pct = json
        .get("usage_percentage")
        .and_then(Value::as_f64)
        .filter(|p| p.is_finite() && (0.0..=100.0).contains(p))
        .ok_or("Vibe usage response has no usage percentage")?;
    let resets_at = json
        .get("reset_at")
        .and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s.trim()).ok())
        .map(|d| d.timestamp_millis());
    Ok(Snapshot::ok(
        ID,
        NAME,
        None,
        vec![Metric::progress("Vibe", pct, None).with_reset(resets_at, None)],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn usage_percentage_becomes_one_row() {
        let doc = json!([{ "result": { "data": { "json": {
            "usage_percentage": 12.5, "reset_at": "2026-12-01T00:00:00Z"
        } } } }]);
        let snap = parse(&doc).ok().unwrap();
        assert_eq!(snap.metrics.len(), 1);
        let row = &snap.metrics[0];
        assert_eq!(row.label, "Vibe");
        assert_eq!(row.used_percent, Some(12.5));
        assert_eq!(
            row.resets_at,
            chrono::DateTime::parse_from_rfc3339("2026-12-01T00:00:00Z")
                .ok()
                .map(|d| d.timestamp_millis())
        );
    }

    #[test]
    fn missing_or_bad_percentage_is_an_error() {
        assert!(parse(&json!([])).is_err());
        let over = json!([{ "result": { "data": { "json": { "usage_percentage": 140 } } } }]);
        assert!(parse(&over).is_err());
    }

    #[test]
    fn cookie_keeps_only_session_and_csrf_pairs() {
        let pasted = "Cookie: _ga=GA1.1.1; csrftoken=abc123; ory_session_xyz=s3cr3t; theme=dark";
        let (cookie, csrf) = console_cookie(pasted).unwrap();
        assert_eq!(cookie, "csrftoken=abc123; ory_session_xyz=s3cr3t");
        assert_eq!(csrf, "abc123");
        // A bare pair list works too.
        assert!(console_cookie("ory_session_a=1; csrftoken=2").is_some());
    }

    #[test]
    fn cookie_without_both_parts_is_refused() {
        assert!(console_cookie("csrftoken=abc").is_none());
        assert!(console_cookie("ory_session_a=1").is_none());
        assert!(console_cookie("").is_none());
        // Header-splitting characters never reach the request.
        assert!(console_cookie("csrftoken=a\r\nX: y; ory_session_a=1").is_none());
    }
}
