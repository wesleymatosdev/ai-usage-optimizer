//! Credit-balance model integration tests — the correction card's acceptance
//! criteria, proven end-to-end against the real binary in a sandbox:
//!
//! 1. A stale (expired) limit-hit must not render as 100%/exhausted; the real
//!    credit reading (~8%) surfaces and routing uses it.
//! 2. A live 429 produces a transient backoff that clears via TTL expiry —
//!    proven by two identical DBs differing only in the event's age — while
//!    monthly consumption stays untouched.
//! 3. `budget check` refuses a dispatch whose dollars would cross the pool.
//! 4. Burn rate + projection are computed from real recorded readings.

#[path = "sandbox_guard.rs"]
mod tempfile_guard;

use tempfile_guard::TempDir;

const EXE: &str = env!("CARGO_BIN_EXE_ai-usage");

fn run(args: &[&str], config: &std::path::Path, db: &std::path::Path) -> (i32, String, String) {
    let out = std::process::Command::new(EXE)
        .args(args)
        .env("AI_USAGE_CONFIG_PATH", config)
        .env("AI_USAGE_DB_PATH", db)
        .env_remove("ZAI_API_KEY")
        .env_remove("GLM_API_KEY")
        .output()
        .expect("binary runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn recommend_json(cfg: &std::path::Path, db: &std::path::Path) -> serde_json::Value {
    let (code, stdout, stderr) = run(&["recommend", "--json"], cfg, db);
    assert_eq!(code, 0, "recommend --json failed: {stderr}");
    serde_json::from_str(&stdout).expect("recommend --json emits JSON")
}

#[allow(dead_code)] // helper for future assertions
fn candidate_of<'a>(parsed: &'a serde_json::Value, provider: &str) -> &'a serde_json::Value {
    parsed["candidates"]
        .as_array()
        .expect("candidates array")
        .iter()
        .find(|c| c["provider"] == provider)
        .unwrap_or_else(|| panic!("provider {provider} among candidates: {parsed}"))
}

/// RFC3339 UTC for unix seconds (local copy — tests can't link the bin crate).
fn iso(unix: i64) -> String {
    iso_from_unix(unix)
}

fn iso_from_unix(unix: i64) -> String {
    let secs = unix.max(0);
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if mth <= 2 { y + 1 } else { y };
    format!("{year:04}-{mth:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

#[test]
fn credit_observe_reports_dollars_not_a_sticky_limit() {
    let dir = TempDir::create("credit-observe");
    let cfg = dir.path.join("config.json");
    let db = dir.path.join("usage.sqlite3");

    let (code, _, _) = run(&["status"], &cfg, &db);
    assert_eq!(code, 0);

    let (code, stdout, stderr) = run(&["credit", "observe", "ollama-pro", "5.10"], &cfg, &db);
    assert_eq!(code, 0, "credit observe failed: {stderr}");
    assert!(stdout.contains("recorded"));

    // The corrected reading: $5.10 of $60 = 8.5%, NOT 100%.
    let parsed = recommend_json(&cfg, &db);
    let p = parsed["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["provider"] == "ollama-pro")
        .unwrap()
        .clone();
    assert_eq!(p["verdict"], "eligible", "{parsed}");
    assert_eq!(p["percent"], 8.5, "5.10/60 = 8.5% — dollars over pool");
    assert_eq!(parsed["recommended"], "ollama-pro");
}

#[test]
fn stale_limit_hit_expires_and_credit_reading_surfaces() {
    let dir = TempDir::create("credit-stale-limithit");
    let cfg = dir.path.join("config.json");
    let db = dir.path.join("usage.sqlite3");

    let (_, _, _) = run(&["status"], &cfg, &db);
    run(&["credit", "observe", "ollama-pro", "5.10"], &cfg, &db);

    // THE incident, replayed: the 429 happened ~20 minutes ago and its
    // 15-minute TTL has already passed — nothing may stick.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let (code, _, stderr) = run(
        &[
            "limit-hit",
            "ollama-pro",
            "--note",
            "stale session 429",
            "--ttl-secs",
            "900",
            "--at-unix",
            &(now - 20 * 60).to_string(),
        ],
        &cfg,
        &db,
    );
    assert_eq!(code, 0, "limit-hit records: {stderr}");

    let parsed = recommend_json(&cfg, &db);
    let p = parsed["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["provider"] == "ollama-pro")
        .unwrap()
        .clone();
    assert_eq!(
        p["verdict"], "eligible",
        "stale limit-hit must not render as exhaustion: {parsed}"
    );
    assert_eq!(p["percent"], 8.5, "the real ~8% consumption surfaces");
    assert_eq!(parsed["recommended"], "ollama-pro");
}

#[test]
fn simulated_429_backoff_clears_while_consumption_untouched() {
    let dir = TempDir::create("credit-429");
    let cfg = dir.path.join("config.json");
    let db_live = dir.path.join("live.sqlite3");
    let db_expired = dir.path.join("expired.sqlite3");

    let (_, _, _) = run(&["status"], &cfg, &db_live);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    // Identical setup in two DBs: $5.10 consumed, one 429 event each. The
    // ONLY difference is the event's age: fresh (live DB) vs 20 min old
    // (past the 15-min TTL). This isolates TTL expiry as the clearing
    // mechanism — no new data, nothing written to undo the backoff.
    for db in [&db_live, &db_expired] {
        run(&["credit", "observe", "ollama-pro", "5.10"], &cfg, db);
    }
    run(
        &[
            "limit-hit",
            "ollama-pro",
            "--ttl-secs",
            "3600",
            "--at-unix",
            &now.to_string(),
        ],
        &cfg,
        &db_live,
    );
    run(
        &[
            "limit-hit",
            "ollama-pro",
            "--ttl-secs",
            "900",
            "--at-unix",
            &(now - 20 * 60).to_string(),
        ],
        &cfg,
        &db_expired,
    );

    // Live event → backoff; the balance is untouched.
    let parsed = recommend_json(&cfg, &db_live);
    let p = parsed["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["provider"] == "ollama-pro")
        .unwrap()
        .clone();
    assert_eq!(p["verdict"], "backoff", "{parsed}");
    assert_eq!(p["percent"], 8.5, "429 must never overwrite the balance");

    let (_, stdout, _) = run(&["credit", "status", "ollama-pro"], &cfg, &db_live);
    assert!(stdout.contains("$5.10 used"), "{stdout}");
    assert!(stdout.contains("transient 429 backoff active"), "{stdout}");
    assert!(
        !stdout.contains("100.0%"),
        "the 429 must never render as 100% of the balance: {stdout}"
    );

    // Expired event → backoff cleared, same balance, zero new data.
    let parsed = recommend_json(&cfg, &db_expired);
    let p = parsed["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["provider"] == "ollama-pro")
        .unwrap()
        .clone();
    assert_eq!(p["verdict"], "eligible", "expired 429 must clear: {parsed}");
    assert_eq!(p["percent"], 8.5, "consumption unchanged by the 429");
}

#[test]
fn budget_check_refuses_a_dispatch_crossing_the_credit_pool() {
    let dir = TempDir::create("credit-budget");
    let cfg = dir.path.join("config.json");
    let db = dir.path.join("usage.sqlite3");

    let (_, _, _) = run(&["status"], &cfg, &db);
    run(&["credit", "observe", "ollama-pro", "58.90"], &cfg, &db);

    // A $3 dispatch would take us to $61.90 > $60 pool → REFUSED, exit 1.
    let (code, _, stderr) = run(&["budget", "check", "ollama-pro", "3.0"], &cfg, &db);
    assert_eq!(code, 1, "crossing the pool must refuse");
    assert!(stderr.contains("credit budget breach"), "{stderr}");
    assert!(stderr.contains("$61.90"), "{stderr}");
    assert!(stderr.contains("$60.00"), "{stderr}");

    // Within the pool → allowed ($58.90 + $1.00 = $59.90 ≤ $60).
    let (code, stdout, _) = run(&["budget", "check", "ollama-pro", "1.00"], &cfg, &db);
    assert_eq!(code, 0, "59.90 stays inside the $60 pool");
    assert!(stdout.contains("within credit budget"), "{stdout}");

    // Exactly at the pool → still allowed (the ceiling is inclusive).
    let (code, stdout, _) = run(&["budget", "check", "ollama-pro", "1.10"], &cfg, &db);
    assert_eq!(code, 0, "$60.00 of $60.00 is exactly at the ceiling");
    assert!(stdout.contains("within credit budget"), "{stdout}");
}

#[test]
fn credit_status_computes_burn_rate_and_projection_from_real_readings() {
    let dir = TempDir::create("credit-burn");
    let cfg = dir.path.join("config.json");
    let db = dir.path.join("usage.sqlite3");

    let (_, _, _) = run(&["status"], &cfg, &db);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    // Reset 3 hours from now.
    std::fs::write(
        &cfg,
        format!(
            r#"{{
          "thresholds": {{"warning": 90, "critical": 95}},
          "rotation_order": ["ollama-pro"],
          "providers": {{
            "ollama-pro": {{"kind": "credit_balance", "monthly_pool_dollars": 60.0, "reset_at": "{}"}}
          }}
        }}"#,
            iso(now + 3 * 3600)
        ),
    )
    .expect("sandbox config");

    // Two real readings an hour apart: $3 → $5, i.e. $2/h burn.
    run(
        &[
            "credit",
            "observe",
            "ollama-pro",
            "3.0",
            "--at-unix",
            &(now - 3600).to_string(),
        ],
        &cfg,
        &db,
    );
    run(
        &[
            "credit",
            "observe",
            "ollama-pro",
            "5.0",
            "--at-unix",
            &now.to_string(),
        ],
        &cfg,
        &db,
    );

    let (_, stdout, _) = run(&["credit", "status", "ollama-pro"], &cfg, &db);
    assert!(
        stdout.contains("$5.00 used of $60.00 pool (8.3%)"),
        "{stdout}"
    );
    assert!(stdout.contains("$2.00/h"), "{stdout}");
    // 5 + 2/h * 3h = 11 at reset.
    assert!(stdout.contains("projected at reset"), "{stdout}");
    assert!(stdout.contains("$11.00"), "{stdout}");
}

#[test]
fn credit_and_percent_observes_coexist_in_routing() {
    let dir = TempDir::create("credit-mixed");
    let cfg = dir.path.join("config.json");
    let db = dir.path.join("usage.sqlite3");

    let (_, _, _) = run(&["status"], &cfg, &db);
    run(
        &["observe", "claude-pro", "20", "--note", "manual"],
        &cfg,
        &db,
    );
    run(&["credit", "observe", "ollama-pro", "5.10"], &cfg, &db);

    let parsed = recommend_json(&cfg, &db);
    // ollama-pro: 8.5% used (91.5% headroom) beats claude-pro 20%.
    assert_eq!(parsed["recommended"], "ollama-pro");
    assert_eq!(parsed["excluded"]["ollama-pro"], serde_json::Value::Null);
}
