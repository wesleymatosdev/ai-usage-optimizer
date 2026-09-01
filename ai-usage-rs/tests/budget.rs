//! Budget module tests: hard daily/weekly ceilings with a spend ledger.
//!
//! The guarantee under test: a plan with a nominal 10k tokens/day budget
//! cannot silently reach 18k of recorded spend — the hard stop fires when
//! projected usage (recorded + requested) would cross the ceiling.

#[path = "sandbox_guard.rs"]
mod tempfile_guard;
use tempfile_guard::TempDir;

const EXE: &str = env!("CARGO_BIN_EXE_ai-usage");

fn write_config(path: &std::path::Path, daily: u64, weekly: u64) {
    std::fs::write(
        path,
        format!(
            r#"{{
          "thresholds": {{"warning": 90, "critical": 95}},
          "rotation_order": ["claude-pro", "zai-codeplus", "chatgpt-plus", "ollama-pro", "ollama-local"],
          "providers": {{
            "claude-pro": {{
              "kind": "claude_local",
              "five_hour_token_budget": 225000,
              "daily_token_budget": {daily},
              "weekly_token_budget": {weekly}
            }},
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

fn budget_check(config: &std::path::Path, db: &std::path::Path, args: &[&str]) -> (i32, String) {
    let out = std::process::Command::new(EXE)
        .args([&["budget", "check"], args].concat())
        .env("AI_USAGE_CONFIG_PATH", config)
        .env("AI_USAGE_DB_PATH", db)
        .output()
        .expect("binary runs");
    // Refusal messages go to stderr; capture both so assertions can match.
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), text)
}

#[test]
fn spend_within_budget_is_allowed() {
    let dir = TempDir::create("budget-ok");
    let config = dir.path.join("config.json");
    let db = dir.path.join("usage.sqlite3");
    write_config(&config, 10_000, 50_000);

    // Ledger 6k spent today for claude-pro.
    let out = std::process::Command::new(EXE)
        .args(["budget", "record", "claude-pro", "6000"])
        .env("AI_USAGE_CONFIG_PATH", &config)
        .env("AI_USAGE_DB_PATH", &db)
        .output()
        .expect("binary runs");
    assert_eq!(out.status.code(), Some(0));

    // A 3k dispatch: 6k + 3k = 9k <= 10k → allowed.
    let (code, stdout) = budget_check(&config, &db, &["claude-pro", "3000"]);
    assert_eq!(code, 0, "within budget must pass: {stdout}");
}

#[test]
fn dispatch_that_would_cross_the_daily_ceiling_is_rejected() {
    let dir = TempDir::create("budget-over");
    let config = dir.path.join("config.json");
    let db = dir.path.join("usage.sqlite3");
    write_config(&config, 10_000, 50_000);

    // 9k spent today; a 3k dispatch would reach 12k — the silent-18k failure.
    std::process::Command::new(EXE)
        .args(["budget", "record", "claude-pro", "9000"])
        .env("AI_USAGE_CONFIG_PATH", &config)
        .env("AI_USAGE_DB_PATH", &db)
        .output()
        .expect("binary runs");

    let (code, stdout) = budget_check(&config, &db, &["claude-pro", "3000"]);
    assert_eq!(code, 1, "over-budget dispatch must be refused: {stdout}");
    assert!(
        stdout.contains("budget breach"),
        "refusal explains the ceiling: {stdout}"
    );

    // But a 1k dispatch (9k + 1k = 10k exactly) is still allowed.
    let (code, stdout) = budget_check(&config, &db, &["claude-pro", "1000"]);
    assert_eq!(code, 0, "exact-fit dispatch allowed: {stdout}");
}

#[test]
fn unestimated_cost_requires_explicit_estimate() {
    let dir = TempDir::create("budget-estimate");
    let config = dir.path.join("config.json");
    let db = dir.path.join("usage.sqlite3");
    write_config(&config, 10_000, 50_000);

    // No estimate given → refuse (dispatch must declare its cost class).
    let (code, stdout) = budget_check(&config, &db, &["claude-pro"]);
    assert_eq!(code, 1, "missing estimate must refuse: {stdout}");
    assert!(stdout.contains("estimate"), "explains: {stdout}");
}

#[test]
fn weekly_ceiling_gates_even_when_daily_has_room() {
    let dir = TempDir::create("budget-week");
    let config = dir.path.join("config.json");
    let db = dir.path.join("usage.sqlite3");
    write_config(&config, 10_000, 12_000);

    // 11k recorded but backdated 25h ago: outside the rolling daily window
    // (shows 0), fully inside the rolling 7-day week (carries all 11k).
    let twenty_five_hours_ago = {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        now - 25 * 3600
    };
    std::process::Command::new(EXE)
        .args([
            "budget",
            "record",
            "claude-pro",
            "11000",
            "--at-unix",
            &twenty_five_hours_ago.to_string(),
        ])
        .env("AI_USAGE_CONFIG_PATH", &config)
        .env("AI_USAGE_DB_PATH", &db)
        .output()
        .expect("binary runs");

    // Daily shows only 1k used, but the week is nearly spent.
    let (code, stdout) = budget_check(&config, &db, &["claude-pro", "1000"]);
    assert_eq!(code, 0, "1k exactly fills the week: {stdout}");

    let (code, stdout) = budget_check(&config, &db, &["claude-pro", "3000"]);
    assert_eq!(code, 1, "weekly ceiling blocks: {stdout}");
    assert!(
        stdout.contains("weekly"),
        "names the weekly ceiling: {stdout}"
    );
}

#[test]
fn old_spend_rolls_out_of_the_daily_window() {
    let dir = TempDir::create("budget-roll");
    let config = dir.path.join("config.json");
    let db = dir.path.join("usage.sqlite3");
    write_config(&config, 10_000, 100_000);

    // Backdate a 9k spend record to 3 days ago.
    let three_days_ago = {
        // now - 3 days, epoch seconds
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        now - 3 * 86400
    };
    // spend entries go through the CLI: budget record --at-epoch
    let out = std::process::Command::new(EXE)
        .args([
            "budget",
            "record",
            "claude-pro",
            "9000",
            "--at-unix",
            &three_days_ago.to_string(),
        ])
        .env("AI_USAGE_CONFIG_PATH", &config)
        .env("AI_USAGE_DB_PATH", &db)
        .output()
        .expect("binary runs");
    assert_eq!(out.status.code(), Some(0));

    // The 9k is outside the daily window: a full 10k dispatch is allowed.
    let (code, stdout) = budget_check(&config, &db, &["claude-pro", "10000"]);
    assert_eq!(code, 0, "stale spend must not gate today: {stdout}");
}
