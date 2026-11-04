//! Muse Code (Meta's coding plan, dev.meta.ai). Usage comes from the
//! endpoint the `muse` CLI itself calls, authenticated with the device-code
//! login it saved in `~/.config/muse/auth.json` (or `MUSE_AUTH_PATH`).
//! Only the `dca:` device token carries subscription usage — the `LLM|…`
//! inference key stored next to it does not.
//!
//! The answer holds the rolling 5-hour window and the week as used
//! percentages. While the 5-hour window is idle Meta leaves `subs_usage`
//! out entirely — `muse usage` reports "server_sent_no_usage" then too.
//! An idle window has nothing used, and the week cannot grow without one,
//! so the card shows the session at 0% and the last weekly reading taken
//! while a window was active (kept in `%APPDATA%\Pane\muse_last_usage.json`,
//! per Meta account, rolled over at its weekly reset). auth.meta.com and
//! api.meta.ai bounce requests that carry no User-Agent, which the shared
//! client always sets.

use super::{config_dir, http, json_body, key_fingerprint, read_small_text, Metric, Snapshot};
use serde_json::Value;
use std::path::{Path, PathBuf};

const ID: &str = "muse";
const NAME: &str = "Muse Code";
const USAGE_URL: &str = "https://api.meta.ai/muse-code/key";
const MAX_CRED_BYTES: u64 = 64 * 1024;
const MAX_BODY_BYTES: usize = 256 * 1024;
const SESSION_MS: i64 = 5 * 3_600_000;
const WEEK_MS: i64 = 7 * 24 * 3_600_000;
const LAST_READING_FILE: &str = "muse_last_usage.json";

/// The weekly reading from the last answer that carried one.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct WeeklyReading {
    /// Fingerprint of the account (see `account_of`), never the address.
    account: String,
    used_percent: f64,
    /// Epoch ms.
    resets_at: i64,
    /// Epoch ms.
    observed_at: i64,
}

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

fn load_reading_in(dir: &Path) -> Option<WeeklyReading> {
    let raw = std::fs::read_to_string(dir.join(LAST_READING_FILE)).ok()?;
    serde_json::from_str(&raw).ok()
}

fn save_reading_in(dir: &Path, reading: &WeeklyReading) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create config dir: {e}"))?;
    let raw = serde_json::to_string_pretty(reading).map_err(|e| format!("encode reading: {e}"))?;
    std::fs::write(dir.join(LAST_READING_FILE), raw).map_err(|e| format!("write reading: {e}"))
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

    let dir = config_dir();
    let last = load_reading_in(&dir);
    let (snap, reading) = parse(&doc, chrono::Utc::now().timestamp_millis(), last.as_ref())?;
    if let Some(reading) = reading {
        // Best-effort: a failed write only costs the idle-time weekly row.
        let _ = save_reading_in(&dir, &reading);
    }
    Ok(snap)
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

/// Whose reading this is. The answer carries `user_email` but no user id
/// (Nov 2026); only a one-way fingerprint of it is ever stored.
fn account_of(doc: &Value) -> Option<String> {
    let id = ["user_email", "user_id"].iter().find_map(|k| match doc.get(*k)? {
        Value::String(s) => Some(s.trim().to_lowercase()).filter(|s| !s.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })?;
    Some(key_fingerprint(&id))
}

/// The idle-time weekly row: the last reading of this account, or 0% once
/// its weekly reset has passed (rolled forward to the current week).
fn weekly_from_reading(reading: &WeeklyReading, now_ms: i64) -> Metric {
    if reading.resets_at > now_ms {
        let seen = chrono::DateTime::from_timestamp_millis(reading.observed_at)
            .map(|d| d.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string())
            .unwrap_or_default();
        return Metric::progress(
            "Weekly",
            reading.used_percent,
            Some(format!("Last reading {seen}")),
        )
        .with_reset(Some(reading.resets_at), Some(WEEK_MS));
    }
    let weeks_passed = (now_ms - reading.resets_at).div_euclid(WEEK_MS) + 1;
    Metric::progress("Weekly", 0.0, None)
        .with_reset(Some(reading.resets_at + weeks_passed * WEEK_MS), Some(WEEK_MS))
}

/// The card, plus a fresh weekly reading to remember when the answer had one.
fn parse(
    doc: &Value,
    now_ms: i64,
    last: Option<&WeeklyReading>,
) -> Result<(Snapshot, Option<WeeklyReading>), String> {
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
    let account = account_of(doc);

    let mut metrics = Vec::new();
    let mut reading = None;
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
                let resets_at = reset_ms(w.get("resets_at"));
                metrics.push(
                    Metric::progress("Weekly", pct, None).with_reset(resets_at, Some(WEEK_MS)),
                );
                if let (Some(account), Some(resets_at)) = (account.clone(), resets_at) {
                    reading = Some(WeeklyReading {
                        account,
                        used_percent: pct,
                        resets_at,
                        observed_at: now_ms,
                    });
                }
            }
        }
    }

    if metrics.is_empty() {
        // Idle 5-hour window: Meta sends no quota at all.
        metrics.push(
            Metric::progress("Session", 0.0, Some("Idle — starts with your next request".into()))
                .with_reset(None, Some(SESSION_MS)),
        );
        let same_account = last.filter(|r| account.as_deref() == Some(r.account.as_str()));
        metrics.push(match same_account {
            Some(r) => weekly_from_reading(r, now_ms),
            None => Metric::text("Weekly", "Shown after your next request".into()),
        });
    }
    Ok((Snapshot::ok(ID, NAME, plan, metrics), reading))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 2026-11-01T12:00:00Z
    const NOW: i64 = 1_793_534_400_000;
    /// 2026-11-02T00:00:00Z, the Monday weekly reset.
    const WEEK_RESET: i64 = 1_793_577_600_000;

    /// The fields the endpoint really sends (Nov 2026): an e-mail, no user id.
    fn active(usage: Value) -> Value {
        json!({
            "require_payment": false,
            "is_subs_active": true,
            "has_payment_method": true,
            "subs_tier_id": "tier",
            "subs_tier_name": "Muse Code Everyday Usage",
            "user_full_name": "Someone",
            "user_email": "someone@example.com",
            "subs_usage": usage
        })
    }

    fn idle() -> Value {
        let mut doc = active(Value::Null);
        doc.as_object_mut().unwrap().remove("subs_usage");
        doc
    }

    fn me() -> String {
        key_fingerprint("someone@example.com")
    }

    fn reading(account: &str) -> WeeklyReading {
        WeeklyReading {
            account: account.into(),
            used_percent: 57.4,
            resets_at: WEEK_RESET,
            observed_at: NOW - 3_600_000,
        }
    }

    #[test]
    fn both_windows_become_rows_and_are_remembered() {
        let doc = active(json!({
            "window": { "used_percent": 6, "window_duration_mins": 300, "resets_at": 1_793_545_287 },
            "weekly": { "used_percent": 57.4, "resets_at": WEEK_RESET / 1000 }
        }));
        let (snap, saved) = parse(&doc, NOW, None).unwrap();
        assert_eq!(snap.plan.as_deref(), Some("Muse Code Everyday Usage"));
        let session = &snap.metrics[0];
        assert_eq!(session.label, "Session");
        assert_eq!(session.used_percent, Some(6.0));
        assert_eq!(session.resets_at, Some(1_793_545_287_000));
        assert_eq!(session.period_ms, Some(SESSION_MS));
        let weekly = &snap.metrics[1];
        assert_eq!(weekly.label, "Weekly");
        assert_eq!(weekly.used_percent, Some(57.4));
        assert_eq!(weekly.resets_at, Some(WEEK_RESET));
        assert_eq!(weekly.period_ms, Some(WEEK_MS));
        assert_eq!(
            saved,
            Some(WeeklyReading {
                account: me(),
                used_percent: 57.4,
                resets_at: WEEK_RESET,
                observed_at: NOW,
            })
        );
    }

    #[test]
    fn idle_window_shows_zero_session_and_last_weekly() {
        let last = reading(&me());
        let (snap, saved) = parse(&idle(), NOW, Some(&last)).unwrap();
        assert_eq!(snap.status, "ok");
        assert_eq!(saved, None, "an idle answer must not overwrite the reading");
        let session = &snap.metrics[0];
        assert_eq!(session.label, "Session");
        assert_eq!(session.used_percent, Some(0.0));
        assert_eq!(session.resets_at, None);
        let weekly = &snap.metrics[1];
        assert_eq!(weekly.kind, "progress");
        assert_eq!(weekly.used_percent, Some(57.4));
        assert_eq!(weekly.resets_at, Some(WEEK_RESET));
        assert!(weekly.detail.as_deref().unwrap_or("").starts_with("Last reading"));
    }

    #[test]
    fn idle_after_weekly_reset_rolls_to_zero() {
        let last = reading(&me());
        let later = WEEK_RESET + 8 * 24 * 3_600_000; // past two resets
        let (snap, _) = parse(&idle(), later, Some(&last)).unwrap();
        let weekly = &snap.metrics[1];
        assert_eq!(weekly.used_percent, Some(0.0));
        assert_eq!(weekly.resets_at, Some(WEEK_RESET + 2 * WEEK_MS));
        assert_eq!(weekly.detail, None);
    }

    #[test]
    fn idle_ignores_another_accounts_reading() {
        let last = reading(&key_fingerprint("other@example.com"));
        let (snap, _) = parse(&idle(), NOW, Some(&last)).unwrap();
        assert_eq!(snap.metrics[1].kind, "text");
        let (snap, _) = parse(&idle(), NOW, None).unwrap();
        assert_eq!(snap.metrics.len(), 2);
        assert_eq!(snap.metrics[1].label, "Weekly");
        assert_eq!(snap.metrics[1].kind, "text");
    }

    #[test]
    fn account_is_a_fingerprint_of_the_email() {
        let mut doc = idle();
        doc["user_email"] = json!(" SomeOne@Example.com ");
        assert_eq!(account_of(&doc), Some(me()));
        doc.as_object_mut().unwrap().remove("user_email");
        assert_eq!(account_of(&doc), None);
        doc["user_id"] = json!(42);
        assert_eq!(account_of(&doc), Some(key_fingerprint("42")));
        // The stored reading never holds the address itself.
        let raw = serde_json::to_string(&reading(&me())).unwrap();
        assert!(!raw.contains("example.com"));
    }

    #[test]
    fn percentages_are_clamped() {
        let doc = active(json!({
            "window": { "used_percent": 140, "window_duration_mins": 300 },
            "weekly": { "used_percent": -3 }
        }));
        let (snap, saved) = parse(&doc, NOW, None).unwrap();
        assert_eq!(snap.metrics[0].used_percent, Some(100.0));
        assert_eq!(snap.metrics[0].resets_at, None);
        assert_eq!(snap.metrics[1].used_percent, Some(0.0));
        assert_eq!(saved, None, "no reset time, nothing to remember");
    }

    #[test]
    fn billing_and_subscription_states_are_errors() {
        let mut unpaid = active(Value::Null);
        unpaid["require_payment"] = json!(true);
        // Snapshot has no Debug, so no unwrap_err.
        assert!(parse(&unpaid, NOW, None).err().unwrap().contains("payment"));
        let mut lapsed = active(Value::Null);
        lapsed["is_subs_active"] = json!(false);
        assert!(parse(&lapsed, NOW, None).err().unwrap().contains("subscription"));
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

    #[test]
    fn reading_round_trips_through_its_file() {
        let dir = std::env::temp_dir().join(format!(
            "pane-muse-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        assert_eq!(load_reading_in(&dir), None);
        save_reading_in(&dir, &reading(&me())).unwrap();
        assert_eq!(load_reading_in(&dir), Some(reading(&me())));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
