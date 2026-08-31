//! Exact ChatGPT and Anthropic subscription quotas through Hermes's local,
//! authenticated AI Usage Monitor API.
//!
//! Hermes owns OAuth refresh and provider-specific credential resolution. This
//! collector only reads normalized quota snapshots from the loopback Dashboard,
//! so credentials never enter this process or its SQLite database.

use serde_json::Value;
use std::time::Duration;

const DASHBOARD_BASE_URL: &str = "http://localhost:9119";
const TOKEN_MARKER: &str = "window.__HERMES_SESSION_TOKEN__";

#[derive(Debug, PartialEq)]
pub struct AccountSnapshot {
    pub percent: f64,
    pub source: String,
    pub note: String,
}

fn safe_text(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(max_chars)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn extract_session_token(html: &str) -> Option<String> {
    let marker_start = html.find(TOKEN_MARKER)? + TOKEN_MARKER.len();
    let assignment = html.get(marker_start..)?;
    let value = assignment.split_once('=')?.1.trim_start();
    let quote = value.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let remainder = value.get(quote.len_utf8()..)?;
    let end = remainder.find(quote)?;
    let token = remainder.get(..end)?;
    if token.is_empty() || token.len() > 512 || token.chars().any(char::is_control) {
        return None;
    }
    Some(token.to_string())
}

pub(crate) fn parse_account_snapshot(body: &str) -> Result<AccountSnapshot, String> {
    let payload: Value = serde_json::from_str(body)
        .map_err(|_| "Hermes returned an invalid account-quota response".to_string())?;
    let account = payload
        .get("account")
        .and_then(Value::as_object)
        .ok_or_else(|| "Hermes account-quota response was missing account data".to_string())?;

    if account.get("available").and_then(Value::as_bool) != Some(true) {
        let reason = account
            .get("reason")
            .and_then(Value::as_str)
            .map(|text| safe_text(text, 200))
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| "Hermes account quota is unavailable".to_string());
        return Err(reason);
    }

    let windows = account
        .get("windows")
        .and_then(Value::as_array)
        .ok_or_else(|| "Hermes account quota did not include usage windows".to_string())?;
    let mut max_percent: Option<f64> = None;
    let mut parts = Vec::new();
    for window in windows.iter().take(8) {
        let Some(percent) = window.get("used_percent").and_then(Value::as_f64) else {
            continue;
        };
        if !percent.is_finite() {
            continue;
        }
        let percent = percent.clamp(0.0, 100.0);
        max_percent = Some(max_percent.map_or(percent, |current| current.max(percent)));
        let label = window
            .get("label")
            .and_then(Value::as_str)
            .map(|text| safe_text(text, 80))
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| "Quota".to_string());
        let mut part = format!("{label} {percent:.0}%");
        if let Some(reset) = window.get("reset_at").and_then(Value::as_str) {
            let reset = safe_text(reset, 80);
            if !reset.is_empty() {
                part.push_str(&format!(" (resets {reset})"));
            }
        }
        parts.push(part);
    }

    let percent = max_percent
        .ok_or_else(|| "Hermes account quota did not include numeric usage".to_string())?;
    if let Some(plan) = account.get("plan").and_then(Value::as_str) {
        let plan = safe_text(plan, 80);
        if !plan.is_empty() {
            parts.push(format!("plan {plan}"));
        }
    }
    if let Some(details) = account.get("details").and_then(Value::as_array) {
        parts.extend(
            details
                .iter()
                .take(8)
                .filter_map(Value::as_str)
                .filter_map(|text| {
                    let text = safe_text(text, 160);
                    (!text.is_empty()).then_some(text)
                }),
        );
    }

    Ok(AccountSnapshot {
        percent,
        source: "hermes-usage-api".to_string(),
        note: parts.join("; "),
    })
}

pub fn collect(provider: &str) -> Result<AccountSnapshot, String> {
    if !matches!(provider, "openai-codex" | "anthropic") {
        return Err("unsupported Hermes account-quota provider".to_string());
    }

    let html = ureq::get(&format!("{DASHBOARD_BASE_URL}/"))
        .timeout(Duration::from_secs(10))
        .call()
        .map_err(|_| "Hermes Dashboard is unavailable on localhost:9119".to_string())?
        .into_string()
        .map_err(|_| "Hermes Dashboard returned an unreadable page".to_string())?;
    let token = extract_session_token(&html)
        .ok_or_else(|| "Hermes Dashboard did not expose a session token".to_string())?;

    let body = ureq::get(&format!(
        "{DASHBOARD_BASE_URL}/api/plugins/ai-usage-monitor/snapshot?provider={provider}"
    ))
    .set("X-Hermes-Session-Token", &token)
    .timeout(Duration::from_secs(45))
    .call()
    .map_err(|_| "Hermes AI Usage Monitor snapshot request failed".to_string())?
    .into_string()
    .map_err(|_| "Hermes AI Usage Monitor returned an unreadable snapshot".to_string())?;

    parse_account_snapshot(&body)
}

#[cfg(test)]
mod tests {
    use super::{extract_session_token, parse_account_snapshot};

    #[test]
    fn extracts_ephemeral_dashboard_token_without_exposing_it() {
        let html = r#"<script>window.__HERMES_SESSION_TOKEN__ = "session-secret";</script>"#;
        assert_eq!(
            extract_session_token(html).as_deref(),
            Some("session-secret")
        );

        let single_quoted = "window.__HERMES_SESSION_TOKEN__='other-secret'";
        assert_eq!(
            extract_session_token(single_quoted).as_deref(),
            Some("other-secret")
        );
    }

    #[test]
    fn rejects_missing_or_empty_dashboard_tokens() {
        assert_eq!(extract_session_token("<html></html>"), None);
        assert_eq!(
            extract_session_token("window.__HERMES_SESSION_TOKEN__ = \"\""),
            None
        );
    }

    #[test]
    fn normalizes_the_most_constrained_account_window() {
        let body = r#"{
          "ok": true,
          "account": {
            "available": true,
            "provider": "openai-codex",
            "source": "usage_api",
            "plan": "Plus",
            "windows": [
              {"label":"Session","used_percent":27,"remaining_percent":73,"reset_at":"2026-09-01T00:54:04+00:00"},
              {"label":"Weekly","used_percent":38,"remaining_percent":62,"reset_at":"2026-09-07T02:52:05+00:00"}
            ],
            "details": ["You have 1 reset banked"]
          }
        }"#;

        let snapshot = parse_account_snapshot(body).expect("valid account snapshot");
        assert_eq!(snapshot.percent, 38.0);
        assert_eq!(snapshot.source, "hermes-usage-api");
        assert!(snapshot.note.contains("Session 27%"));
        assert!(snapshot.note.contains("Weekly 38%"));
        assert!(snapshot.note.contains("Plus"));
        assert!(snapshot.note.contains("1 reset banked"));
    }

    #[test]
    fn unavailable_account_snapshot_is_not_reported_as_zero_usage() {
        let body = r#"{
          "ok": true,
          "account": {
            "available": false,
            "provider": "openrouter",
            "reason": "The provider did not expose an account-quota snapshot.",
            "windows": [],
            "details": []
          }
        }"#;

        let error = parse_account_snapshot(body).expect_err("unavailable must stay unavailable");
        assert!(error.contains("did not expose"));
    }
}
