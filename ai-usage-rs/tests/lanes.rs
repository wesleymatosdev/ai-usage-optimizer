//! Lane concurrency + reset-window tests.
//!
//! Guarantee 1: a provider's configured max_parallel_lanes caps how many
//! concurrent workers may be handed out; claiming beyond the cap is refused.
//! Guarantee 2: `recommend --json` carries each provider's reset window
//! (seconds until reset) parsed from collector notes, so callers can wait
//! for a near reset instead of burning a nearly-dead window.

#[path = "sandbox_guard.rs"]
mod tempfile_guard;
use tempfile_guard::TempDir;

const EXE: &str = env!("CARGO_BIN_EXE_ai-usage");

fn run(config: &std::path::Path, db: &std::path::Path, args: &[&str]) -> (i32, String, String) {
    let out = std::process::Command::new(EXE)
        .args(args)
        .env("AI_USAGE_CONFIG_PATH", config)
        .env("AI_USAGE_DB_PATH", db)
        .output()
        .expect("binary runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn config_with_lanes(path: &std::path::Path, lanes: u32) {
    std::fs::write(
        path,
        format!(
            r#"{{
          "thresholds": {{"warning": 90, "critical": 95}},
          "rotation_order": ["claude-pro", "zai-codeplus", "chatgpt-plus", "ollama-pro", "ollama-local"],
          "providers": {{
            "claude-pro": {{"kind": "claude_local", "five_hour_token_budget": 225000, "max_parallel_lanes": {lanes}}},
            "zai-codeplus": {{"kind": "zai_quota", "api_key_env": "ZAI_API_KEY", "endpoint": "https://api.z.ai/api/monitor/usage/quota/limit"}},
            "chatgpt-plus": {{"kind": "hermes_account_quota"}},
            "ollama-pro": {{"kind": "manual"}},
            "ollama-local": {{"kind": "ollama_local", "endpoint": "http://localhost:9/api/tags"}}
          }}
        }}"#
        ),
    )
    .expect("sandbox config");
}

#[test]
fn lane_claim_within_cap_succeeds_and_over_cap_is_refused() {
    let dir = TempDir::create("lanes-cap");
    let config = dir.path.join("config.json");
    let db = dir.path.join("usage.sqlite3");
    config_with_lanes(&config, 2);

    // Claim 1 and 2: both succeed.
    let (code, _, _) = run(&config, &db, &["lane", "claim", "claude-pro"]);
    assert_eq!(code, 0, "first claim within cap");
    let (code, _, _) = run(&config, &db, &["lane", "claim", "claude-pro"]);
    assert_eq!(code, 0, "second claim within cap");

    // Third claim: the cap is 2 → refused.
    let (code, stdout, stderr) = run(&config, &db, &["lane", "claim", "claude-pro"]);
    assert_eq!(code, 1, "over-cap claim refused: {stdout}{stderr}");

    // Release one lane, then claiming works again.
    let (code, _, _) = run(&config, &db, &["lane", "release", "claude-pro"]);
    assert_eq!(code, 0);
    let (code, _, _) = run(&config, &db, &["lane", "claim", "claude-pro"]);
    assert_eq!(code, 0, "claim succeeds again after release");
}

#[test]
fn unknown_provider_lane_claim_is_rejected() {
    let dir = TempDir::create("lanes-unknown");
    let config = dir.path.join("config.json");
    let db = dir.path.join("usage.sqlite3");
    config_with_lanes(&config, 1);

    let (code, _, _) = run(&config, &db, &["lane", "claim", "not-a-provider"]);
    assert_eq!(code, 1);
}

#[test]
fn recommend_json_reports_reset_windows_when_known() {
    let dir = TempDir::create("lanes-reset");
    let config = dir.path.join("config.json");
    let db = dir.path.join("usage.sqlite3");
    config_with_lanes(&config, 1);

    // A note shaped like the Hermes collector's (resets 2026-09-01T12:00:00+00:00).
    let future = {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let then = now + 3600; // 1h from now
                               // Format as RFC3339 UTC using a tiny civil-from-days conversion.
                               // BUG (fixed here): days must be derived from `then`, not `now` —
                               // using `now`'s day with `then`'s time-of-day silently produced a
                               // PAST timestamp whenever the 1h offset crossed a UTC midnight
                               // boundary, which the new window-reset staleness check (routing.rs)
                               // would then correctly — but wrongly for this test — classify as
                               // Unknown instead of Eligible.
        let days = then.div_euclid(86400);
        let secs = then.rem_euclid(86400);
        let z = days + 719_468;
        let era = z / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let yy = if m <= 2 { y + 1 } else { y };
        let (h, mi, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
        format!("{yy:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}+00:00")
    };

    let out = std::process::Command::new(EXE)
        .args([
            "observe",
            "claude-pro",
            "40",
            "--note",
            &format!("session 40% (resets {future})"),
        ])
        .env("AI_USAGE_CONFIG_PATH", &config)
        .env("AI_USAGE_DB_PATH", &db)
        .output()
        .expect("binary runs");
    assert_eq!(out.status.code(), Some(0));

    let (code, stdout, _) = run(&config, &db, &["recommend", "--json"]);
    assert_eq!(code, 0);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    let claude = parsed["candidates"]
        .as_array()
        .expect("candidates")
        .iter()
        .find(|c| c["provider"] == "claude-pro")
        .expect("claude-pro present")
        .clone();
    assert_eq!(claude["verdict"], "eligible");
    // Reset parsing: 1h in the future → reset_in_secs ~3600 (bounded 0..7200).
    let reset = claude["reset_in_secs"].as_i64().expect("reset_in_secs");
    assert!(
        (0..=7200).contains(&reset),
        "reset window parsed from note: {reset}"
    );
}
