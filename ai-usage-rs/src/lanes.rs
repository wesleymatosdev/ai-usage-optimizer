//! Lane concurrency caps — admission control for parallel worker dispatches.
//!
//! A provider's `max_parallel_lanes` config caps how many concurrent
//! workers may claim a slot at once. Claims live in SQLite; a claim beyond
//! the cap is refused with an explanation instead of stacking another
//! worker on an already-saturated provider.

use crate::config::Config;
use rusqlite::{params, Connection};

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Active (un-expired) claims for a provider. Claims auto-expire after one
/// hour so a crashed worker cannot leak its slot forever.
const LANE_TTL_SECS: i64 = 3600;

pub fn active_lanes(conn: &Connection, provider: &str, now: i64) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM lanes WHERE provider = ?1 AND claimed_at > ?2",
        params![provider, now - LANE_TTL_SECS],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
}

pub fn claim(conn: &Connection, cfg: &Config, provider: &str, now: i64) -> Result<i64, String> {
    let cap = cfg
        .providers
        .get(provider)
        .and_then(|p| p.max_parallel_lanes)
        .unwrap_or(1);

    let active = active_lanes(conn, provider, now);
    if active >= cap as i64 {
        return Err(format!(
            "lane cap reached: {active}/{cap} active lanes on {provider} — release a lane or \
             wait for one to expire (TTL {LANE_TTL_SECS}s) before dispatching another worker"
        ));
    }
    conn.execute(
        "INSERT INTO lanes (provider, claimed_at) VALUES (?1, ?2)",
        rusqlite::params![provider, now],
    )
    .map_err(|e| format!("db error claiming lane: {e}"))?;
    Ok(active + 1)
}

pub fn release(conn: &Connection, provider: &str) -> Result<bool, String> {
    let removed = conn
        .execute(
            "DELETE FROM lanes WHERE id = (
                 SELECT id FROM lanes WHERE provider = ?1
                 ORDER BY claimed_at ASC, id ASC LIMIT 1
             )",
            rusqlite::params![provider],
        )
        .map_err(|e| format!("db error releasing lane: {e}"))?;
    Ok(removed > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderConfig;

    fn mem_db() -> Connection {
        let conn = Connection::open_in_memory().expect("memory db");
        conn.execute_batch(
            "CREATE TABLE lanes (id INTEGER PRIMARY KEY, provider TEXT NOT NULL,
             claimed_at INTEGER NOT NULL);",
        )
        .expect("schema");
        conn
    }

    fn cfg_with_lanes(n: u32) -> Config {
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "claude-pro".to_string(),
            ProviderConfig {
                kind: "claude_local".to_string(),
                five_hour_token_budget: None,
                api_key_env: None,
                endpoint: None,
                note: None,
                daily_token_budget: None,
                weekly_token_budget: None,
                max_parallel_lanes: Some(n),
            },
        );
        Config {
            thresholds: crate::config::Thresholds {
                warning: 90.0,
                critical: 95.0,
            },
            rotation_order: vec!["claude-pro".to_string()],
            providers,
            local_first: true,
        }
    }

    #[test]
    fn claims_up_to_the_cap_then_refuses() {
        let conn = mem_db();
        let cfg = cfg_with_lanes(2);
        assert_eq!(claim(&conn, &cfg, "claude-pro", 1000), Ok(1));
        assert_eq!(claim(&conn, &cfg, "claude-pro", 1010), Ok(2));
        let over = claim(&conn, &cfg, "claude-pro", 1020);
        assert!(over.is_err(), "third claim must hit the 2-lane cap");
        assert!(over.unwrap_err().contains("lane cap reached"));
    }

    #[test]
    fn expired_claims_free_their_lane() {
        let conn = mem_db();
        let cfg = cfg_with_lanes(1);
        assert!(claim(&conn, &cfg, "claude-pro", 1000).is_ok());
        // Same instant: cap hit.
        assert!(claim(&conn, &cfg, "claude-pro", 1000).is_err());
        // TTL later: the stale claim expired, so claiming works again.
        assert!(claim(&conn, &cfg, "claude-pro", 1000 + LANE_TTL_SECS + 1).is_ok());
    }

    #[test]
    fn release_frees_a_slot() {
        let conn = mem_db();
        let cfg = cfg_with_lanes(1);
        claim(&conn, &cfg, "claude-pro", 1000).unwrap();
        assert!(release(&conn, "claude-pro").unwrap());
        assert!(claim(&conn, &cfg, "claude-pro", 1005).is_ok());
    }

    #[test]
    fn providers_without_a_cap_default_to_one_lane() {
        let conn = mem_db();
        let cfg = Config::default_config();
        assert!(claim(&conn, &cfg, "zai-codeplus", 1000).is_ok());
        assert!(claim(&conn, &cfg, "zai-codeplus", 1000).is_err());
    }
}
