//! Muse Code (Meta's coding plan, dev.meta.ai). Usage comes from the
//! endpoint the `muse` CLI itself calls, authenticated with the device-code
//! login it saved in `~/.config/muse/auth.json` (or `MUSE_AUTH_PATH`).
//! Only the `dca:` device token carries subscription usage — the `LLM|…`
//! inference key stored next to it does not.
//!
//! The answer holds the rolling 5-hour window and the week as used
//! percentages. While the 5-hour window is idle Meta leaves `subs_usage`
//! out entirely; the card then shows the plan without bars instead of an
//! error. auth.meta.com and api.meta.ai bounce requests that carry no
//! User-Agent, which the shared client always sets.

use super::{http, json_body, read_small_text, Metric, Snapshot};
use serde_json::Value;
use std::path::PathBuf;

const ID: &str = "muse";
const NAME: &str = "Muse Code";
const USAGE_URL: &str = "https://api.meta.ai/muse-code/key";
const MAX_CRED_BYTES: u64 = 64 * 1024;
const MAX_BODY_BYTES: usize = 256 * 1024;
const WEEK_MS: i64 = 7 * 24 * 3_600_000;

pub async fn snapshot() -> Snapshot {
    match fetch().await {
        Ok(s) => s,
        Err(e) => Snapshot::error(ID, NAME, e),
    }
}

fn auth_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("MUSE_AUTH_PATH") {
        let p = p.trim();
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    Some(dirs::home_dir()?.join(".config").join("muse").join("auth.json"))
}

/// muse 1.4+ nests the login under `providers.meta`; older builds kept it
/// under `meta` or at the top level.
fn token_from(doc: &Value) -> Option<String> {
    ["/providers/meta/access_token", "/meta/access_token", "/access_token"]
        .iter()
        .find_map(|p| doc.pointer(p).and_then(Value::as_str))
        .map(str::trim)
        .filter(|t| t.len() > 4 && t.starts_with("dca:"))
        .map(str::to_string)
}

async fn fetch() -> Result<Snapshot, String> {
    let token = auth_path()
        .and_then(|p| read_small_text(&p, MAX_CRED_BYTES, "Muse auth").ok())
        .and_then(|raw| serde_json::from_str::<Value>(raw.trim_start_matches('\u{feff}')).ok())
        .and_then(|doc| token_from(&doc));
    let Some(token) = token else {
        return Ok(Snapshot::no_credentials(
            ID,
            NAME,
            "Sign in with `muse login` (Muse Code CLI).",
        ));
    };

    let resp = http()
        .post(USAGE_URL)
        .bearer_auth(&token)
        .header("x-api-version", "1.0.0")
        .header("accept", "application/json")
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|e| format!("usage request: {e}"))?;
    let status = resp.status();
    if matches!(status.as_u16(), 401 | 403) {
        return Err("Muse login expired — run `muse login` again".into());
    }
    if !status.is_success() {
        return Err(format!("usage endpoint: HTTP {status}"));
    }
    let doc = json_body(resp, MAX_BODY_BYTES, "usage").await?;
    parse(&doc)
}

fn percent(v: Option<&Value>) -> Option<f64> {
    v?.as_f64()
        .filter(|p| p.is_finite())
        .map(|p| p.clamp(0.0, 100.0))
}

/// `resets_at` is epoch seconds; accept ms or an ISO date too.
fn reset_ms(v: Option<&Value>) -> Option<i64> {
    let v = v?;
    if let Some(n) = v.as_f64().filter(|n| n.is_finite()) {
        if n <= 0.0 {
            return None;
        }
        return Some(if n > 10_000_000_000.0 { n as i64 } else { (n * 1000.0) as i64 });
    }
    chrono::DateTime::parse_from_rfc3339(v.as_str()?.trim())
        .ok()
        .map(|d| d.timestamp_millis())
}

fn parse(doc: &Value) -> Result<Snapshot, String> {
    if doc.get("require_payment").and_then(Value::as_bool) == Some(true) {
        return Err("Muse Code needs a payment method — finish billing at dev.meta.ai".into());
    }
    if doc.get("is_subs_active").and_then(Value::as_bool) != Some(true) {
        return Err("no active Muse Code subscription on this login".into());
    }
    let plan = doc
        .get("subs_tier_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let mut metrics = Vec::new();
    if let Some(usage) = doc.get("subs_usage").filter(|u| u.is_object()) {
        if let Some(w) = usage.get("window") {
            if let Some(pct) = percent(w.get("used_percent")) {
                let period = w
                    .get("window_duration_mins")
                    .and_then(Value::as_f64)
                    .filter(|m| m.is_finite() && *m > 0.0)
                    .map(|m| (m * 60_000.0) as i64);
                metrics.push(
                    Metric::progress("Session", pct, None)
                        .with_reset(reset_ms(w.get("resets_at")), period),
                );
            }
        }
        if let Some(w) = usage.get("weekly") {
            if let Some(pct) = percent(w.get("used_percent")) {
                metrics.push(
                    Metric::progress("Weekly", pct, None)
                        .with_reset(reset_ms(w.get("resets_at")), Some(WEEK_MS)),
                );
            }
        }
    }
    if metrics.is_empty() {
        metrics.push(Metric::text(
            "Quota",
            "Shown while a 5-hour window is active".into(),
        ));
    }
    Ok(Snapshot::ok(ID, NAME, plan, metrics))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn active(usage: Value) -> Value {
        json!({
            "require_payment": false,
            "is_subs_active": true,
            "subs_tier_name": "Muse Code Everyday Usage",
            "user_email": "someone@example.com",
            "user_id": "1234",
            "subs_usage": usage
        })
    }

    #[test]
    fn both_windows_become_rows() {
        let doc = active(json!({
            "window": { "used_percent": 6, "window_duration_mins": 300, "resets_at": 1_793_965_287 },
            "weekly": { "used_percent": 57.4, "resets_at": 1_794_009_600 }
        }));
        let snap = parse(&doc).unwrap();
        assert_eq!(snap.plan.as_deref(), Some("Muse Code Everyday Usage"));
        let session = &snap.metrics[0];
        assert_eq!(session.label, "Session");
        assert_eq!(session.used_percent, Some(6.0));
        assert_eq!(session.resets_at, Some(1_793_965_287_000));
        assert_eq!(session.period_ms, Some(5 * 3_600_000));
        let weekly = &snap.metrics[1];
        assert_eq!(weekly.label, "Weekly");
        assert_eq!(weekly.used_percent, Some(57.4));
        assert_eq!(weekly.resets_at, Some(1_794_009_600_000));
        assert_eq!(weekly.period_ms, Some(WEEK_MS));
    }

    #[test]
    fn idle_window_shows_plan_without_bars() {
        let mut doc = active(Value::Null);
        doc.as_object_mut().unwrap().remove("subs_usage");
        let snap = parse(&doc).unwrap();
        assert_eq!(snap.status, "ok");
        assert_eq!(snap.metrics.len(), 1);
        assert_eq!(snap.metrics[0].kind, "text");
    }

    #[test]
    fn percentages_are_clamped() {
        let doc = active(json!({
            "window": { "used_percent": 140, "window_duration_mins": 300 },
            "weekly": { "used_percent": -3 }
        }));
        let snap = parse(&doc).unwrap();
        assert_eq!(snap.metrics[0].used_percent, Some(100.0));
        assert_eq!(snap.metrics[0].resets_at, None);
        assert_eq!(snap.metrics[1].used_percent, Some(0.0));
    }

    #[test]
    fn billing_and_subscription_states_are_errors() {
        let mut unpaid = active(Value::Null);
        unpaid["require_payment"] = json!(true);
        // Snapshot has no Debug, so no unwrap_err.
        assert!(parse(&unpaid).err().unwrap().contains("payment"));
        let mut lapsed = active(Value::Null);
        lapsed["is_subs_active"] = json!(false);
        assert!(parse(&lapsed).err().unwrap().contains("subscription"));
    }

    #[test]
    fn only_device_tokens_are_used() {
        let nested = json!({ "providers": { "meta": {
            "access_token": "dca:abc", "api_key": "LLM|xyz", "mechanism": "oauth"
        } } });
        assert_eq!(token_from(&nested).as_deref(), Some("dca:abc"));
        let legacy = json!({ "access_token": " dca:old " });
        assert_eq!(token_from(&legacy).as_deref(), Some("dca:old"));
        let inference_only = json!({ "providers": { "meta": { "api_key": "LLM|xyz" } } });
        assert_eq!(token_from(&inference_only), None);
        let wrong_kind = json!({ "providers": { "meta": { "access_token": "LLM|xyz" } } });
        assert_eq!(token_from(&wrong_kind), None);
    }
}
