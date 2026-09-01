//! End-to-end CLI tests. Every test runs the real binary against isolated
//! AI_USAGE_CONFIG_PATH / AI_USAGE_DB_PATH locations so tests never touch the
//! user's real config or usage database.

#[path = "sandbox_guard.rs"]
mod tempfile_guard;

use std::process::Command;
use tempfile_guard::TempDir;

const EXE: &str = env!("CARGO_BIN_EXE_ai-usage");

struct Sandbox {
    _dir: TempDir,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        Self {
            _dir: TempDir::create(tag),
        }
    }
}

fn run(args: &[&str], config: &std::path::Path, db: &std::path::Path) -> (i32, String, String) {
    let out = Command::new(EXE)
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

fn sandbox_paths(tag: &str) -> (Sandbox, std::path::PathBuf, std::path::PathBuf) {
    let sandbox = Sandbox::new(tag);
    let root = sandbox._dir.path.clone();
    (
        sandbox,
        root.join("config.json"),
        root.join("usage.sqlite3"),
    )
}

fn recommend_json(cfg: &std::path::Path, db: &std::path::Path) -> serde_json::Value {
    let (code, stdout, stderr) = run(&["recommend", "--json"], cfg, db);
    assert_eq!(code, 0, "recommend --json failed: {stderr}");
    serde_json::from_str(&stdout).expect("recommend --json emits JSON")
}

#[test]
fn fresh_database_never_recommends_an_unknown_provider() {
    let (_s, cfg, db) = sandbox_paths("fresh");

    let (code, _, _) = run(&["status"], &cfg, &db);
    assert_eq!(code, 0);

    let parsed = recommend_json(&cfg, &db);
    assert_eq!(parsed["recommended"], serde_json::Value::Null);
}

#[test]
fn observe_and_recommend_round_trip_reports_verified_headroom() {
    let (_s, cfg, db) = sandbox_paths("roundtrip");

    run(&["status"], &cfg, &db);

    let (code, stdout, _) = run(
        &["observe", "chatgpt-plus", "30", "--note", "cli test"],
        &cfg,
        &db,
    );
    assert_eq!(code, 0);
    assert!(stdout.contains("recorded"));

    let parsed = recommend_json(&cfg, &db);
    assert_eq!(parsed["recommended"], "chatgpt-plus");
    assert_eq!(parsed["candidates"][0]["has_headroom"], true);
}

#[test]
fn limit_hit_marks_provider_and_recommendation_routes_around_it() {
    let (_s, cfg, db) = sandbox_paths("limithit");

    run(&["status"], &cfg, &db);
    run(
        &["observe", "claude-pro", "20", "--note", "cli test"],
        &cfg,
        &db,
    );

    let parsed = recommend_json(&cfg, &db);
    assert_eq!(parsed["recommended"], "claude-pro");

    let (code, _, _) = run(
        &["limit-hit", "claude-pro", "--note", "cli test 429"],
        &cfg,
        &db,
    );
    assert_eq!(code, 0);

    let parsed = recommend_json(&cfg, &db);
    assert_eq!(parsed["recommended"], serde_json::Value::Null);

    let status = run(&["status"], &cfg, &db).1;
    assert!(
        status.contains("claude-pro"),
        "status lists claude-pro: {status}"
    );
    assert!(
        status.contains("limit-hit"),
        "status shows source: {status}"
    );
}
