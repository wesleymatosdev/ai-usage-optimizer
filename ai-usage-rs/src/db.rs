use rusqlite::{params, Connection, OptionalExtension, Result as SqlResult};
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

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
        // Restrict the data directory to owner-only (0700), bypassing umask.
        if let Ok(meta) = fs::metadata(parent) {
            let perms = meta.permissions();
            if perms.mode() & 0o077 != 0 {
                let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
            }
        }
    }
    // Create the database file with restrictive permissions before opening.
    if !path.exists() {
        if let Ok(file) = fs::File::create(path) {
            let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
        }
    } else if let Ok(meta) = fs::metadata(path) {
        // Warn if the existing database is world-readable or group-readable.
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            eprintln!(
                "warning: database {} is group/other accessible (mode {:o}); recommend chmod 600",
                path.display(),
                mode & 0o777
            );
        }
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
    let mut out = HashMap::new();
    let mut stmt = match conn.prepare(
        "SELECT o.provider, o.percent, o.source, o.note, o.observed_at
         FROM observations o
         JOIN (SELECT provider, MAX(id) AS id FROM observations GROUP BY provider) x
           ON o.id = x.id",
    ) {
        Ok(s) => s,
        Err(_) => return out,
    };
    let rows = stmt.query_map([], |row| {
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

#[allow(dead_code)]
pub fn last_alert_for(conn: &Connection, provider: &str, level: &str) -> SqlResult<Option<String>> {
    conn.query_row(
        "SELECT fired_at FROM alerts WHERE provider = ?1 AND level = ?2 ORDER BY id DESC LIMIT 1",
        params![provider, level],
        |row| row.get(0),
    )
    .optional()
}
