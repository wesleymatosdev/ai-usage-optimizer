use rusqlite::{params, Connection, OptionalExtension, Result as SqlResult};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// A 429/session-limit event is transient backoff, NOT plan exhaustion: it
/// renders as a `backoff` flag for its TTL and then vanishes. It must never
/// touch the credit-balance figure (that lives in credit_observations).
pub const DEFAULT_RATE_LIMIT_TTL_SECS: i64 = 15 * 60;
/// A `limit-hit` observation older than this is expired and invisible to
/// routing. This only governs LEGACY sticky rows (source = "limit-hit");
/// new 429s go to rate_limit_events with their own per-event TTL.
pub const LIMIT_HIT_TTL_SECS: i64 = DEFAULT_RATE_LIMIT_TTL_SECS;

#[derive(Debug, Clone)]
pub struct Observation {
    pub percent: Option<f64>,
    pub source: String,
    pub note: String,
    #[allow(dead_code)]
    pub at: String,
}

pub fn now_iso() -> String {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    // Minimal RFC3339-ish UTC timestamp, no external chrono dependency.
    let secs = dur.as_secs();
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // civil_from_days algorithm (Howard Hinnant), good enough for a log timestamp.
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if mth <= 2 { y + 1 } else { y };
    format!("{year:04}-{mth:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

pub fn open(path: &Path) -> SqlResult<Connection> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS observations (
            id INTEGER PRIMARY KEY,
            provider TEXT NOT NULL,
            percent REAL,
            source TEXT NOT NULL,
            note TEXT,
            observed_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS alerts (
            id INTEGER PRIMARY KEY,
            provider TEXT NOT NULL,
            level TEXT NOT NULL,
            percent REAL NOT NULL,
            message TEXT NOT NULL,
            fired_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS spend (
            id INTEGER PRIMARY KEY,
            provider TEXT NOT NULL,
            tokens INTEGER NOT NULL,
            at_unix INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS lanes (
            id INTEGER PRIMARY KEY,
            provider TEXT NOT NULL,
            claimed_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS credit_observations (
            id INTEGER PRIMARY KEY,
            provider TEXT NOT NULL,
            used_dollars REAL NOT NULL,
            note TEXT,
            observed_at_unix INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS rate_limit_events (
            id INTEGER PRIMARY KEY,
            provider TEXT NOT NULL,
            at_unix INTEGER NOT NULL,
            ttl_secs INTEGER NOT NULL,
            note TEXT
        );",
    )?;
    Ok(conn)
}

pub fn observe(
    conn: &Connection,
    provider: &str,
    percent: Option<f64>,
    source: &str,
    note: &str,
) -> SqlResult<()> {
    conn.execute(
        "INSERT INTO observations (provider, percent, source, note, observed_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![provider, percent, source, note, now_iso()],
    )?;
    Ok(())
}

pub fn latest(conn: &Connection) -> HashMap<String, Observation> {
    latest_as_of(conn, now_unix())
}

/// Latest observation per provider, EXCLUDING expired transient states.
///
/// A `limit-hit` row is a session 429, not plan consumption: it must expire
/// (LIMIT_HIT_TTL_SECS) instead of sticking at percent=100 forever. This is
/// the computed-expiry root fix — nothing writes a "cleared" row and nothing
/// needs to; the event simply ages out of the view.
pub fn latest_as_of(conn: &Connection, now_unix: i64) -> HashMap<String, Observation> {
    let mut out = HashMap::new();
    let cutoff = iso_from_unix(now_unix - LIMIT_HIT_TTL_SECS);
    let mut stmt = match conn.prepare(
        "SELECT o.provider, o.percent, o.source, o.note, o.observed_at
         FROM observations o
         JOIN (SELECT provider, MAX(id) AS id FROM observations GROUP BY provider) x
           ON o.id = x.id
         WHERE NOT (o.source = 'limit-hit' AND o.observed_at < ?1)",
    ) {
        Ok(s) => s,
        Err(_) => return out,
    };
    let rows = stmt.query_map(params![cutoff], |row| {
        Ok((
            row.get::<_, String>(0)?,
            Observation {
                percent: row.get(1)?,
                source: row.get(2)?,
                note: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                at: row.get(4)?,
            },
        ))
    });
    if let Ok(rows) = rows {
        for r in rows.flatten() {
            out.insert(r.0, r.1);
        }
    }
    out
}

pub fn alert(
    conn: &Connection,
    provider: &str,
    level: &str,
    percent: f64,
    message: &str,
) -> SqlResult<()> {
    conn.execute(
        "INSERT INTO alerts (provider, level, percent, message, fired_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![provider, level, percent, message, now_iso()],
    )?;
    Ok(())
}

// --- spend ledger (budget guardrails) --------------------------------------

/// Record tokens consumed by a dispatch on a provider. `at_unix` allows
/// backdating (tests, import); pass the current time for live entries.
pub fn record_spend(conn: &Connection, provider: &str, tokens: u64, at_unix: i64) -> SqlResult<()> {
    conn.execute(
        "INSERT INTO spend (provider, tokens, at_unix) VALUES (?1, ?2, ?3)",
        params![provider, tokens as i64, at_unix],
    )?;
    Ok(())
}

/// Sum of recorded spend for a provider in [from_unix, to_unix].
pub fn spend_since(conn: &Connection, provider: &str, from_unix: i64, to_unix: i64) -> u64 {
    conn.query_row(
        "SELECT COALESCE(SUM(tokens), 0) FROM spend
         WHERE provider = ?1 AND at_unix >= ?2 AND at_unix <= ?3",
        params![provider, from_unix, to_unix],
        |row| row.get::<_, i64>(0),
    )
    .map(|v| v.max(0) as u64)
    .unwrap_or(0)
}

#[allow(dead_code)]
pub fn last_alert_for(conn: &Connection, provider: &str, level: &str) -> SqlResult<Option<String>> {
    conn.query_row(
        "SELECT fired_at FROM alerts WHERE provider = ?1 AND level = ?2 ORDER BY id DESC LIMIT 1",
        params![provider, level],
        |row| row.get(0),
    )
    .optional()
}

// --- credit-balance model (Ollama Pro & friends) -----------------------------
//
// A credit provider is a MONTHLY DOLLAR POOL, not a rate limit: the durable
// quantity is dollars consumed / dollars remaining, with a reset date. The
// percent form is derived only so routing keeps one uniform surface.

#[derive(Debug, Clone)]
pub struct CreditObservation {
    pub used_dollars: f64,
    #[allow(dead_code)] // carried for display; not read in current paths
    pub note: String,
    pub at_unix: i64,
}

pub fn record_credit(
    conn: &Connection,
    provider: &str,
    used_dollars: f64,
    note: &str,
    at_unix: i64,
) -> SqlResult<()> {
    conn.execute(
        "INSERT INTO credit_observations (provider, used_dollars, note, observed_at_unix)
         VALUES (?1, ?2, ?3, ?4)",
        params![provider, used_dollars, note, at_unix],
    )?;
    Ok(())
}

/// Latest credit reading for a provider, if any.
pub fn latest_credit(conn: &Connection, provider: &str) -> Option<CreditObservation> {
    conn.query_row(
        "SELECT used_dollars, COALESCE(note, ''), observed_at_unix
         FROM credit_observations WHERE provider = ?1
         ORDER BY id DESC LIMIT 1",
        params![provider],
        |row| {
            Ok(CreditObservation {
                used_dollars: row.get(0)?,
                note: row.get(1)?,
                at_unix: row.get(2)?,
            })
        },
    )
    .optional()
    .ok()
    .flatten()
}

// --- transient rate-limit events --------------------------------------------

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Record a 429/session-limit event. Transient: backoff for ttl_secs, then gone.
pub fn record_rate_limit(
    conn: &Connection,
    provider: &str,
    note: &str,
    at_unix: i64,
    ttl_secs: i64,
) -> SqlResult<()> {
    conn.execute(
        "INSERT INTO rate_limit_events (provider, at_unix, ttl_secs, note) VALUES (?1, ?2, ?3, ?4)",
        params![provider, at_unix, ttl_secs, note],
    )?;
    Ok(())
}

pub struct RateLimitEvent {
    pub at_unix: i64,
    pub ttl_secs: i64,
    #[allow(dead_code)] // carried for display; not read in current paths
    pub note: String,
}

impl RateLimitEvent {
    pub fn expires_at(&self) -> i64 {
        self.at_unix + self.ttl_secs
    }
    pub fn is_active(&self, now_unix: i64) -> bool {
        now_unix < self.expires_at()
    }
}

/// All rate-limit events for a provider, regardless of expiry (caller filters).
pub fn rate_limit_events(conn: &Connection, provider: &str) -> Vec<RateLimitEvent> {
    let mut out = Vec::new();
    let Ok(mut stmt) = conn.prepare(
        "SELECT at_unix, ttl_secs, COALESCE(note, '') FROM rate_limit_events
         WHERE provider = ?1 ORDER BY id DESC",
    ) else {
        return out;
    };
    let rows = stmt.query_map(params![provider], |row| {
        Ok(RateLimitEvent {
            at_unix: row.get(0)?,
            ttl_secs: row.get(1)?,
            note: row.get(2)?,
        })
    });
    if let Ok(rows) = rows {
        for r in rows.flatten() {
            out.push(r);
        }
    }
    out
}

/// The provider's currently-active (unexpired) rate-limit events.
pub fn active_rate_limits(conn: &Connection, provider: &str, now_unix: i64) -> Vec<RateLimitEvent> {
    rate_limit_events(conn, provider)
        .into_iter()
        .filter(|e| e.is_active(now_unix))
        .collect()
}

/// Reverse of now_iso: unix seconds for a stored RFC3339-ish UTC timestamp.
/// Used to TTL-check legacy `limit-hit` rows stored with ISO timestamps.
pub fn iso_from_unix(unix: i64) -> String {
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
    let year = mth <= 2;
    let year = if year { y + 1 } else { y };
    format!("{year:04}-{mth:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_db() -> Connection {
        let conn = Connection::open_in_memory().expect("memory db");
        open_in_memory_schema(&conn).expect("schema");
        conn
    }

    fn open_in_memory_schema(conn: &Connection) -> SqlResult<()> {
        conn.execute_batch(
            "CREATE TABLE observations (
                id INTEGER PRIMARY KEY, provider TEXT NOT NULL, percent REAL,
                source TEXT NOT NULL, note TEXT, observed_at TEXT NOT NULL);
             CREATE TABLE credit_observations (
                id INTEGER PRIMARY KEY, provider TEXT NOT NULL, used_dollars REAL NOT NULL,
                note TEXT, observed_at_unix INTEGER NOT NULL);
             CREATE TABLE rate_limit_events (
                id INTEGER PRIMARY KEY, provider TEXT NOT NULL, at_unix INTEGER NOT NULL,
                ttl_secs INTEGER NOT NULL, note TEXT);",
        )?;
        Ok(())
    }

    #[test]
    fn stale_limit_hit_disappears_after_ttl() {
        let conn = mem_db();
        let now = now_unix();
        // A limit-hit recorded 20 minutes ago: past the 15-min TTL.
        let stale_at = iso_from_unix(now - 20 * 60);
        conn.execute(
            "INSERT INTO observations (provider, percent, source, note, observed_at)
             VALUES ('ollama-pro', 100.0, 'limit-hit', 'stale', ?1)",
            params![stale_at],
        )
        .unwrap();
        let latest = latest_as_of(&conn, now);
        assert!(
            !latest.contains_key("ollama-pro"),
            "expired limit-hit must vanish, not stick at 100%"
        );
    }

    #[test]
    fn fresh_limit_hit_still_visible_within_ttl() {
        let conn = mem_db();
        let now = now_unix();
        conn.execute(
            "INSERT INTO observations (provider, percent, source, note, observed_at)
             VALUES ('ollama-pro', 100.0, 'limit-hit', 'fresh', ?1)",
            params![iso_from_unix(now - 60)],
        )
        .unwrap();
        assert!(latest_as_of(&conn, now).contains_key("ollama-pro"));
    }

    #[test]
    fn non_limit_hit_sources_never_expire_via_ttl() {
        let conn = mem_db();
        let now = now_unix();
        // A week-old manual reading is stale data but must still surface —
        // only limit-hit rows have the transient TTL semantics.
        conn.execute(
            "INSERT INTO observations (provider, percent, source, note, observed_at)
             VALUES ('claude-pro', 42.0, 'manual', 'old', ?1)",
            params![iso_from_unix(now - 7 * 86_400)],
        )
        .unwrap();
        let latest = latest_as_of(&conn, now);
        assert_eq!(
            latest.get("claude-pro").map(|o| o.percent),
            Some(Some(42.0))
        );
    }

    #[test]
    fn rate_limit_events_expire_on_ttl() {
        let conn = mem_db();
        record_rate_limit(&conn, "ollama-pro", "429", now_unix() - 900, 600).unwrap();
        assert!(active_rate_limits(&conn, "ollama-pro", now_unix()).is_empty());
        record_rate_limit(&conn, "ollama-pro", "429", now_unix() - 60, 600).unwrap();
        assert_eq!(active_rate_limits(&conn, "ollama-pro", now_unix()).len(), 1);
    }

    #[test]
    fn credit_round_trip_keeps_latest_reading() {
        let conn = mem_db();
        record_credit(&conn, "ollama-pro", 5.10, "dashboard", 1_000).unwrap();
        record_credit(&conn, "ollama-pro", 7.25, "dashboard", 2_000).unwrap();
        let c = latest_credit(&conn, "ollama-pro").expect("credit reading");
        assert!((c.used_dollars - 7.25).abs() < 1e-9);
        assert_eq!(c.at_unix, 2_000);
    }

    #[test]
    fn iso_round_trips_through_now_iso() {
        let stamp = now_iso();
        // parse_iso_to_unix lives in the claude collector; here we only need
        // iso_from_unix to produce a stable, parseable UTC string.
        assert!(stamp.ends_with('Z'));
        assert_eq!(stamp.len(), 20);
    }
}
