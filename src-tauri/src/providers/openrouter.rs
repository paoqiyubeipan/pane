//! OpenRouter. Credits come from `/api/v1/credits`; `/api/v1/key` adds the
//! key's own spending cap and the account's free-model request counter
//! (`free_model_daily_requests`, per UTC day: 50 before $10 of lifetime
//! credit purchases, 1,000 after).
//!
//! Besides the main key (saved, env var, or the one OpenCode stores),
//! keys of other accounts can be added in Settings. Each gets its own
//! `openrouter@<fingerprint>` card, so the card follows the key rather
//! than its place in the list. They live in
//! `%APPDATA%\Pane\openrouter_keys.json`, owner-only like the other key
//! files, and go only to openrouter.ai.

use super::{config_dir, http, key_fingerprint, stored_api_key, Metric, Snapshot};
use serde_json::Value;
use std::path::Path;

const ID: &str = "openrouter";
const NAME: &str = "OpenRouter";
const KEYS_FILE: &str = "openrouter_keys.json";
const DAY_MS: i64 = 24 * 3_600_000;
const MAX_EXTRA_KEYS: usize = 20;
const MAX_NAME_CHARS: usize = 40;

pub async fn snapshot() -> Snapshot {
    // Saved key or env var first, then the key OpenCode stores if the user
    // connected OpenRouter there.
    let (key, source) = match stored_api_key("openrouter", &["OPENROUTER_API_KEY"]) {
        Some(key) => (key, "your saved key"),
        None => match super::opencode::auth_entry_key("openrouter") {
            Some(key) => (key, "the key stored by OpenCode"),
            None => {
                return Snapshot::no_credentials(
                    ID,
                    NAME,
                    "Paste an OpenRouter API key in Settings (gear icon).",
                );
            }
        },
    };
    match fetch(&key, source, ID, NAME).await {
        Ok(s) => s,
        Err(e) => Snapshot::error(ID, NAME, e),
    }
}

/// One extra key from Settings. `id` is a fingerprint of the key.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ExtraKey {
    pub id: String,
    pub name: String,
    #[serde(rename = "apiKey")]
    api_key: String,
}

/// What Settings may see of an extra key — never the key itself.
#[derive(serde::Serialize)]
pub struct ExtraKeyDto {
    pub id: String,
    pub name: String,
}

impl ExtraKey {
    pub fn card_id(&self) -> String {
        format!("{ID}@{}", self.id)
    }

    pub fn card_name(&self) -> String {
        format!("{NAME} · {}", self.name)
    }
}

pub async fn snapshot_key(card: ExtraKey) -> Snapshot {
    let (id, name) = (card.card_id(), card.card_name());
    match fetch(&card.api_key, "this key", &id, &name).await {
        Ok(s) => s,
        Err(e) => Snapshot::error(&id, &name, e),
    }
}

/// A missing file is an empty list; an unreadable one is an error, so a
/// broken file is never silently overwritten by the next add.
fn load_keys_in(dir: &Path) -> Result<Vec<ExtraKey>, String> {
    let raw = match std::fs::read_to_string(dir.join(KEYS_FILE)) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("read OpenRouter keys: {e}")),
    };
    serde_json::from_str(raw.trim_start_matches('\u{feff}'))
        .map_err(|e| format!("parse OpenRouter keys: {e}"))
}

fn save_keys_in(dir: &Path, keys: &[ExtraKey]) -> Result<(), String> {
    let raw = serde_json::to_string_pretty(keys).map_err(|e| format!("encode OpenRouter keys: {e}"))?;
    super::onenewapi::store::atomic_write(&dir.join(KEYS_FILE), &raw)
}

pub fn extra_keys() -> Vec<ExtraKey> {
    load_keys_in(&config_dir()).unwrap_or_default()
}

pub fn list_keys() -> Vec<ExtraKeyDto> {
    extra_keys()
        .into_iter()
        .map(|k| ExtraKeyDto { id: k.id, name: k.name })
        .collect()
}

pub fn add_key_in(dir: &Path, name: &str, key: &str) -> Result<ExtraKeyDto, String> {
    let key = key.trim();
    if key.is_empty() || key.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("paste a single OpenRouter API key".into());
    }
    let mut keys = load_keys_in(dir)?;
    let id = key_fingerprint(key);
    if keys.iter().any(|k| k.id == id) {
        return Err("this key is already added".into());
    }
    if keys.len() >= MAX_EXTRA_KEYS {
        return Err(format!("at most {MAX_EXTRA_KEYS} extra keys"));
    }
    let name: String = name
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME_CHARS)
        .collect();
    let name = if name.is_empty() {
        format!("Key {}", keys.len() + 2)
    } else {
        name
    };
    keys.push(ExtraKey {
        id: id.clone(),
        name: name.clone(),
        api_key: key.to_string(),
    });
    save_keys_in(dir, &keys)?;
    Ok(ExtraKeyDto { id, name })
}

pub fn remove_key_in(dir: &Path, id: &str) -> Result<(), String> {
    let mut keys = load_keys_in(dir)?;
    let before = keys.len();
    keys.retain(|k| k.id != id);
    if keys.len() == before {
        return Err("key not found".into());
    }
    save_keys_in(dir, &keys)
}

async fn fetch(key: &str, source: &str, id: &str, name: &str) -> Result<Snapshot, String> {
    let credits_req = http()
        .get("https://openrouter.ai/api/v1/credits")
        .bearer_auth(key)
        .send();
    let key_req = http()
        .get("https://openrouter.ai/api/v1/key")
        .bearer_auth(key)
        .send();
    let (credits_resp, key_resp) = tokio::join!(credits_req, key_req);

    let credits_resp = credits_resp.map_err(|e| format!("credits request: {e}"))?;
    if credits_resp.status().as_u16() == 401 {
        return Err(format!(
            "{source} was rejected — paste a fresh key in Settings (gear icon)"
        ));
    }
    if !credits_resp.status().is_success() {
        return Err(format!("credits endpoint: HTTP {}", credits_resp.status()));
    }
    let credits: Value = credits_resp.json().await.map_err(|e| format!("credits parse: {e}"))?;
    let key_info = match key_resp {
        Ok(resp) if resp.status().is_success() => resp.json::<Value>().await.ok(),
        _ => None,
    };

    let (plan, metrics) = build(&credits, key_info.as_ref(), chrono::Utc::now().timestamp_millis());
    if metrics.is_empty() {
        return Err("no credit data in response".into());
    }
    Ok(Snapshot::ok(id, name, plan, metrics))
}

/// Free-model requests today (UTC). Older answers may omit `used`;
/// `limit - remaining` stands in.
fn free_requests(data: &Value, now_ms: i64) -> Option<Metric> {
    let free = data.get("free_model_daily_requests")?;
    let limit = free
        .get("limit")
        .and_then(Value::as_f64)
        .filter(|l| l.is_finite() && *l > 0.0)?;
    let used = free
        .get("used")
        .and_then(Value::as_f64)
        .or_else(|| free.get("remaining").and_then(Value::as_f64).map(|r| limit - r))
        .filter(|u| u.is_finite())?
        .max(0.0);
    let next_utc_midnight = (now_ms.div_euclid(DAY_MS) + 1) * DAY_MS;
    Some(
        Metric::progress(
            "Free requests",
            (used / limit * 100.0).clamp(0.0, 100.0),
            Some(format!("{used:.0} of {limit:.0} used")),
        )
        .with_reset(Some(next_utc_midnight), Some(DAY_MS)),
    )
}

fn build(credits: &Value, key_info: Option<&Value>, now_ms: i64) -> (Option<String>, Vec<Metric>) {
    let data = credits.get("data").unwrap_or(credits);

    let mut metrics = Vec::new();
    let total = data.get("total_credits").and_then(Value::as_f64);
    let used = data.get("total_usage").and_then(Value::as_f64);
    if let (Some(total), Some(used)) = (total, used) {
        metrics.push(Metric::text("Balance", format!("${:.2}", (total - used).max(0.0))));
        if total > 0.0 {
            metrics.push(Metric::progress(
                "Credits",
                used / total * 100.0,
                Some(format!("${used:.2} of ${total:.2} used")),
            ));
        }
    }

    let mut plan = None;
    if let Some(info) = key_info {
        let data = info.get("data").unwrap_or(info);
        plan = data.get("is_free_tier").and_then(Value::as_bool).map(|free| {
            if free { "Free tier".to_string() } else { "Pay as you go".to_string() }
        });
        if let (Some(limit), Some(usage)) = (
            data.get("limit").and_then(Value::as_f64),
            data.get("usage").and_then(Value::as_f64),
        ) {
            if limit > 0.0 {
                metrics.push(Metric::progress(
                    "Key limit",
                    usage / limit * 100.0,
                    Some(format!("${usage:.2} of ${limit:.2}")),
                ));
            }
        }
        // Free-model requests lead the card: on a free account they are
        // the limit that actually runs out.
        if let Some(free) = free_requests(data, now_ms) {
            metrics.insert(0, free);
        }
    }
    (plan, metrics)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 2026-11-01T12:00:00Z
    const NOON_MS: i64 = 1_793_534_400_000;

    fn key_info(free: Value) -> Value {
        json!({ "data": {
            "label": "sk-or-v1-abc...xyz",
            "limit": null,
            "usage": 0,
            "is_free_tier": true,
            "free_model_daily_requests": free
        } })
    }

    #[test]
    fn free_requests_lead_the_card() {
        let credits = json!({ "data": { "total_credits": 0, "total_usage": 0 } });
        let info = key_info(json!({ "used": 12, "limit": 50, "remaining": 38 }));
        let (plan, metrics) = build(&credits, Some(&info), NOON_MS);
        assert_eq!(plan.as_deref(), Some("Free tier"));
        let labels: Vec<&str> = metrics.iter().map(|m| m.label.as_str()).collect();
        assert_eq!(labels, ["Free requests", "Balance"]);
        let free = &metrics[0];
        assert_eq!(free.used_percent, Some(24.0));
        assert_eq!(free.detail.as_deref(), Some("12 of 50 used"));
        // Resets at the next UTC midnight, 12 hours after noon.
        assert_eq!(free.resets_at, Some(NOON_MS + 12 * 3_600_000));
        assert_eq!(free.period_ms, Some(DAY_MS));
    }

    #[test]
    fn remaining_stands_in_for_missing_used() {
        let info = key_info(json!({ "limit": 1000, "remaining": 750 }));
        let (_, metrics) = build(&json!({}), Some(&info), NOON_MS);
        assert_eq!(metrics[0].label, "Free requests");
        assert_eq!(metrics[0].used_percent, Some(25.0));
    }

    #[test]
    fn missing_counter_adds_no_row() {
        let credits = json!({ "data": { "total_credits": 20, "total_usage": 5 } });
        let info = json!({ "data": { "is_free_tier": false, "limit": 10, "usage": 2.5 } });
        let (plan, metrics) = build(&credits, Some(&info), NOON_MS);
        assert_eq!(plan.as_deref(), Some("Pay as you go"));
        let labels: Vec<&str> = metrics.iter().map(|m| m.label.as_str()).collect();
        assert_eq!(labels, ["Balance", "Credits", "Key limit"]);
    }

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "pane-openrouter-{}-{stamp}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn extra_keys_round_trip_without_exposing_secrets() {
        let tmp = TempDir::new();
        let added = add_key_in(&tmp.0, "  Second account ", "sk-or-v1-one").unwrap();
        assert_eq!(added.name, "Second account");
        assert_eq!(added.id, key_fingerprint("sk-or-v1-one"));
        let unnamed = add_key_in(&tmp.0, "", "sk-or-v1-two").unwrap();
        assert_eq!(unnamed.name, "Key 3");

        let keys = load_keys_in(&tmp.0).unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].card_id(), format!("openrouter@{}", added.id));
        assert_eq!(keys[0].card_name(), "OpenRouter · Second account");
        let dto = serde_json::to_string(&ExtraKeyDto { id: keys[0].id.clone(), name: keys[0].name.clone() })
            .unwrap();
        assert!(!dto.contains("sk-or-v1"));

        remove_key_in(&tmp.0, &added.id).unwrap();
        let keys = load_keys_in(&tmp.0).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].name, "Key 3");
        assert!(remove_key_in(&tmp.0, &added.id).is_err());
    }

    #[test]
    fn duplicate_and_malformed_keys_are_refused() {
        let tmp = TempDir::new();
        add_key_in(&tmp.0, "a", "sk-or-v1-same").unwrap();
        assert!(add_key_in(&tmp.0, "b", " sk-or-v1-same ").is_err());
        assert!(add_key_in(&tmp.0, "c", "two keys here").is_err());
        assert!(add_key_in(&tmp.0, "d", "   ").is_err());
    }

    #[test]
    fn unreadable_key_file_is_never_overwritten() {
        let tmp = TempDir::new();
        std::fs::write(tmp.0.join(KEYS_FILE), "{not json").unwrap();
        assert!(add_key_in(&tmp.0, "a", "sk-or-v1-x").is_err());
        assert_eq!(
            std::fs::read_to_string(tmp.0.join(KEYS_FILE)).unwrap(),
            "{not json"
        );
    }
}
