//! Command Code (commandcode.ai). Plan quota comes from the billing
//! endpoints the dashboard reads, authenticated with the API key the CLI
//! saved at `commandcode login` (`~/.commandcode/auth.json`), a pasted key,
//! or `COMMAND_CODE_API_KEY`.
//!
//! Plans meter dollars over three windows: a rolling 5 hours, a week, and
//! the monthly credit grant. The credits endpoint reports what is LEFT of
//! the monthly grant, not its size, so the grant comes from
//! `monthlyCreditsGranted` when the response has it and otherwise from the
//! published price list below. Plan ids gain version suffixes when pricing
//! changes (`individual-go` → `individual-go-v1`), so an unlisted id never
//! fails the card — it just shows the remaining dollars as text.

use super::{http, json_body, read_small_text, stored_api_key, Metric, Snapshot};
use serde_json::Value;

const ID: &str = "commandcode";
const NAME: &str = "Command Code";
const API: &str = "https://api.commandcode.ai/alpha";
const MAX_CRED_BYTES: u64 = 64 * 1024;
const MAX_BODY_BYTES: usize = 256 * 1024;
const HOUR_MS: i64 = 3_600_000;

/// Monthly credit grant in USD per plan id (commandcode.ai/pricing; same
/// table CodexBar ships).
const PLAN_GRANTS: &[(&str, f64)] = &[
    ("individual-go", 10.0),
    ("individual-go-v1", 10.0),
    ("individual-goat", 70.0),
    ("individual-pro", 30.0),
    ("individual-pro-v1", 80.0),
    ("individual-max", 150.0),
    ("individual-ultra", 300.0),
];

pub async fn snapshot() -> Snapshot {
    match fetch().await {
        Ok(s) => s,
        Err(e) => Snapshot::error(ID, NAME, e),
    }
}

/// `commandcode login` writes { "apiKey": "user_…", "userId": …, … }.
fn cli_key() -> Option<String> {
    let path = dirs::home_dir()?.join(".commandcode").join("auth.json");
    let raw = read_small_text(&path, MAX_CRED_BYTES, "Command Code auth").ok()?;
    let doc: Value = serde_json::from_str(raw.trim_start_matches('\u{feff}')).ok()?;
    doc.get("apiKey")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string)
}

async fn fetch() -> Result<Snapshot, String> {
    let Some(key) = stored_api_key(ID, &["COMMAND_CODE_API_KEY"]).or_else(cli_key) else {
        return Ok(Snapshot::no_credentials(
            ID,
            NAME,
            "Sign in with `commandcode login` or paste an API key in Settings (gear icon).",
        ));
    };

    let credits_req = http()
        .get(format!("{API}/billing/credits"))
        .bearer_auth(&key)
        .header("accept", "application/json")
        .send();
    let sub_req = http()
        .get(format!("{API}/billing/subscriptions"))
        .bearer_auth(&key)
        .header("accept", "application/json")
        .send();
    let (credits_resp, sub_resp) = tokio::join!(credits_req, sub_req);

    let credits_resp = credits_resp.map_err(|e| format!("credits request: {e}"))?;
    let status = credits_resp.status();
    if matches!(status.as_u16(), 401 | 403) {
        return Err("API key was rejected — run `commandcode login` again or paste a new key".into());
    }
    if !status.is_success() {
        return Err(format!("credits endpoint: HTTP {status}"));
    }
    let credits = json_body(credits_resp, MAX_BODY_BYTES, "credits").await?;

    // The subscription only names the plan and sizes the monthly grant —
    // best-effort, the 5-hour and weekly rows stand on their own.
    let subscription = match sub_resp {
        Ok(resp) if resp.status().is_success() => {
            json_body(resp, MAX_BODY_BYTES, "subscription").await.ok()
        }
        _ => None,
    };
    build(&credits, subscription.as_ref())
}

/// Numbers arrive as JSON numbers or numeric strings; take either.
fn num(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
        .filter(|n| n.is_finite())
}

/// Epoch seconds, epoch ms, or an ISO date → epoch ms. Zero means "no
/// active window" (an idle 5-hour limit reports `resetAt: 0`).
fn epoch_ms(v: Option<&Value>) -> Option<i64> {
    let v = v?;
    if let Some(n) = num(Some(v)) {
        if n <= 0.0 {
            return None;
        }
        return Some(if n > 10_000_000_000.0 { n as i64 } else { (n * 1000.0) as i64 });
    }
    chrono::DateTime::parse_from_rfc3339(v.as_str()?.trim())
        .ok()
        .map(|d| d.timestamp_millis())
}

fn grant_for(plan_id: &str) -> Option<f64> {
    let id = plan_id.to_ascii_lowercase();
    PLAN_GRANTS.iter().find(|(p, _)| *p == id).map(|(_, usd)| *usd)
}

fn is_version_tag(part: &str) -> bool {
    part.len() > 1 && part.starts_with('v') && part[1..].bytes().all(|b| b.is_ascii_digit())
}

/// `individual-go-v1` → "Go", `teams-pro` → "Teams Pro". Derived instead of
/// looked up so a new plan id still reads well.
fn plan_name(plan_id: &str) -> String {
    let name = plan_id
        .split('-')
        .filter(|p| !p.is_empty() && *p != "individual" && !is_version_tag(p))
        .map(|p| {
            if p.eq_ignore_ascii_case("goat") {
                return "GOAT".to_string();
            }
            let mut chars = p.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if name.is_empty() {
        plan_id.to_string()
    } else {
        name
    }
}

/// One dollar-metered window: `{ "used": 2.0, "cap": 6, "resetAt": ms }`.
fn window(label: &str, v: Option<&Value>, period_ms: i64) -> Option<Metric> {
    let w = v?;
    let cap = num(w.get("cap")).filter(|c| *c > 0.0)?;
    let used = num(w.get("used")).unwrap_or(0.0).max(0.0);
    Some(
        Metric::progress(
            label,
            (used / cap * 100.0).clamp(0.0, 100.0),
            Some(format!("${used:.2} of ${cap:.2} used")),
        )
        .with_reset(epoch_ms(w.get("resetAt")), Some(period_ms)),
    )
}

fn build(credits: &Value, subscription: Option<&Value>) -> Result<Snapshot, String> {
    let pot = credits
        .get("credits")
        .filter(|c| c.is_object())
        .ok_or("credits response has no credits block")?;
    let limits = credits
        .get("windowLimits")
        .or_else(|| pot.get("windowLimits"));

    let mut metrics: Vec<Metric> = [
        window("Session", limits.and_then(|l| l.get("fiveHour")), 5 * HOUR_MS),
        window("Weekly", limits.and_then(|l| l.get("weekly")), 7 * 24 * HOUR_MS),
    ]
    .into_iter()
    .flatten()
    .collect();

    // `data: null` on a successful answer is the free tier; a plan object
    // carries planId, status, and the billing period end.
    let sub = subscription
        .and_then(|s| s.get("data"))
        .filter(|d| d.is_object());
    let free_tier = subscription.is_some_and(|s| {
        s.get("success").and_then(Value::as_bool) == Some(true)
            && s.get("data").is_some_and(Value::is_null)
    });
    let plan_id = sub
        .and_then(|d| d.get("planId"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty());
    let period_end = sub.and_then(|d| epoch_ms(d.get("currentPeriodEnd")));

    let left = num(pot.get("monthlyCredits"));
    let granted = num(pot.get("monthlyCreditsGranted"))
        .filter(|g| *g > 0.0)
        .or_else(|| plan_id.and_then(grant_for));
    match (left, granted) {
        (Some(left), Some(total)) => {
            let used = (total - left).max(0.0);
            metrics.push(
                Metric::progress(
                    "Monthly",
                    (used / total * 100.0).clamp(0.0, 100.0),
                    Some(format!("${used:.2} of ${total:.2} used")),
                )
                .with_reset(period_end, Some(30 * 24 * HOUR_MS)),
            );
        }
        (Some(left), None) if left > 0.0 || plan_id.is_some() => {
            metrics.push(Metric::text("Monthly credits", format!("${left:.2} left")));
        }
        _ => {}
    }
    if let Some(bought) = num(pot.get("purchasedCredits")).filter(|p| *p > 0.0) {
        metrics.push(Metric::text("Purchased credits", format!("${bought:.2}")));
    }

    if metrics.is_empty() {
        return Err("no usage data in Command Code response".into());
    }

    let status = sub.and_then(|d| d.get("status")).and_then(Value::as_str);
    let plan = match plan_id {
        Some(id) => Some(match status {
            Some(s) if !s.eq_ignore_ascii_case("active") && !s.eq_ignore_ascii_case("trialing") => {
                format!("{} ({s})", plan_name(id))
            }
            _ => plan_name(id),
        }),
        None if free_tier => Some("Free".to_string()),
        None => None,
    };
    Ok(Snapshot::ok(ID, NAME, plan, metrics))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Shapes as `/alpha/billing/credits` and `/alpha/billing/subscriptions`
    /// return them for a Go plan (Oct 2026).
    fn go_credits() -> Value {
        json!({
            "credits": {
                "belowThreshold": false,
                "creditThreshold": 0,
                "monthlyCredits": 7.9947187075,
                "purchasedCredits": 0,
                "freeCredits": 0
            },
            "windowLimits": {
                "limited": true,
                "exceeded": null,
                "fiveHour": { "used": 0, "cap": 3, "exceeded": false, "resetAt": 0 },
                "weekly": { "used": 2.0052812925, "cap": 6, "exceeded": false, "resetAt": 1794038835580i64 }
            },
            "sandboxAccess": false,
            "sandboxMinutes": null
        })
    }

    fn subscription(plan_id: &str, status: &str) -> Value {
        json!({
            "success": true,
            "data": {
                "status": status,
                "planId": plan_id,
                "cancelAtPeriodEnd": false,
                "currentPeriodStart": "2026-10-28T16:38:10.000Z",
                "currentPeriodEnd": "2026-11-28T16:38:10.000Z"
            }
        })
    }

    fn metric<'a>(snap: &'a Snapshot, label: &str) -> &'a Metric {
        snap.metrics
            .iter()
            .find(|m| m.label == label)
            .unwrap_or_else(|| panic!("missing {label} row"))
    }

    #[test]
    fn go_v1_plan_fills_all_three_windows() {
        let snap = build(&go_credits(), Some(&subscription("individual-go-v1", "active")))
            .unwrap();
        assert_eq!(snap.plan.as_deref(), Some("Go"));
        let labels: Vec<&str> = snap.metrics.iter().map(|m| m.label.as_str()).collect();
        assert_eq!(labels, ["Session", "Weekly", "Monthly"]);

        let session = metric(&snap, "Session");
        assert_eq!(session.used_percent, Some(0.0));
        assert_eq!(session.resets_at, None, "resetAt 0 is an idle window");
        assert_eq!(session.period_ms, Some(5 * HOUR_MS));

        let weekly = metric(&snap, "Weekly");
        assert!((weekly.used_percent.unwrap() - 33.421).abs() < 0.01);
        assert_eq!(weekly.resets_at, Some(1794038835580));

        let monthly = metric(&snap, "Monthly");
        assert!((monthly.used_percent.unwrap() - 20.053).abs() < 0.01);
        assert_eq!(monthly.detail.as_deref(), Some("$2.01 of $10.00 used"));
        let end = chrono::DateTime::parse_from_rfc3339("2026-11-28T16:38:10.000Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(monthly.resets_at, Some(end));
    }

    #[test]
    fn unknown_plan_id_keeps_the_card_alive() {
        let snap = build(&go_credits(), Some(&subscription("individual-mega-v3", "active")))
            .unwrap();
        assert_eq!(snap.plan.as_deref(), Some("Mega"));
        let monthly = metric(&snap, "Monthly credits");
        assert_eq!(monthly.kind, "text");
        assert_eq!(monthly.value.as_deref(), Some("$7.99 left"));
    }

    #[test]
    fn granted_amount_from_response_beats_the_price_list() {
        let mut credits = go_credits();
        credits["credits"]["monthlyCreditsGranted"] = json!(20);
        let snap = build(&credits, Some(&subscription("individual-go-v1", "active"))).unwrap();
        assert_eq!(
            metric(&snap, "Monthly").detail.as_deref(),
            Some("$12.01 of $20.00 used")
        );
    }

    #[test]
    fn missing_subscription_still_shows_windows() {
        let snap = build(&go_credits(), None).unwrap();
        assert_eq!(snap.plan, None);
        let labels: Vec<&str> = snap.metrics.iter().map(|m| m.label.as_str()).collect();
        assert_eq!(labels, ["Session", "Weekly", "Monthly credits"]);
    }

    #[test]
    fn null_subscription_is_the_free_tier() {
        let credits = json!({ "credits": { "monthlyCredits": 0, "purchasedCredits": 5 } });
        let snap = build(&credits, Some(&json!({ "success": true, "data": null }))).unwrap();
        assert_eq!(snap.plan.as_deref(), Some("Free"));
        assert_eq!(
            metric(&snap, "Purchased credits").value.as_deref(),
            Some("$5.00")
        );
    }

    #[test]
    fn inactive_status_is_named_in_the_plan() {
        let snap = build(&go_credits(), Some(&subscription("individual-pro-v1", "past_due")))
            .unwrap();
        assert_eq!(snap.plan.as_deref(), Some("Pro (past_due)"));
    }

    #[test]
    fn response_without_credits_block_is_an_error() {
        assert!(build(&json!({ "error": "nope" }), None).is_err());
    }

    #[test]
    fn plan_names_are_derived_from_ids() {
        assert_eq!(plan_name("individual-go"), "Go");
        assert_eq!(plan_name("individual-go-v1"), "Go");
        assert_eq!(plan_name("individual-goat"), "GOAT");
        assert_eq!(plan_name("teams-pro"), "Teams Pro");
        assert_eq!(plan_name("individual"), "individual");
    }

    #[test]
    fn epoch_ms_accepts_seconds_ms_and_iso() {
        assert_eq!(epoch_ms(Some(&json!(1_794_038_835))), Some(1_794_038_835_000));
        assert_eq!(epoch_ms(Some(&json!(1_794_038_835_580i64))), Some(1_794_038_835_580));
        assert_eq!(epoch_ms(Some(&json!("1794038835580"))), Some(1_794_038_835_580));
        assert_eq!(epoch_ms(Some(&json!(0))), None);
        assert_eq!(
            epoch_ms(Some(&json!("2026-11-28T16:38:10Z"))),
            chrono::DateTime::parse_from_rfc3339("2026-11-28T16:38:10Z")
                .ok()
                .map(|d| d.timestamp_millis())
        );
    }
}
