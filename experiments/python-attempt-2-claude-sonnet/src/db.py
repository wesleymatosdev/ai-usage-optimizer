"""SQLite storage for usage snapshots and alerts. Stdlib only."""
import sqlite3
from pathlib import Path
from datetime import datetime, timezone

DB_PATH = Path(__file__).parent.parent / "data" / "usage.db"


def connect():
    DB_PATH.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(DB_PATH)
    conn.execute("""
        CREATE TABLE IF NOT EXISTS snapshots (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            provider TEXT NOT NULL,
            window_type TEXT NOT NULL,       -- '5h', 'weekly', 'monthly', 'unknown'
            pct_used REAL,                   -- 0-100, NULL if not computable
            raw_used INTEGER,                -- tokens or other raw unit
            raw_limit INTEGER,
            resets_at TEXT,                  -- ISO8601, nullable
            source TEXT,                     -- 'jsonl', 'proxy_log', 'api', 'manual'
            captured_at TEXT NOT NULL
        )
    """)
    conn.execute("""
        CREATE TABLE IF NOT EXISTS alerts_sent (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            provider TEXT NOT NULL,
            window_type TEXT NOT NULL,
            threshold INTEGER NOT NULL,      -- 90 or 95
            sent_at TEXT NOT NULL,
            resets_at TEXT                   -- so we know when to re-arm
        )
    """)
    conn.execute("""
        CREATE TABLE IF NOT EXISTS manual_status (
            provider TEXT PRIMARY KEY,
            status TEXT NOT NULL,            -- 'available', 'exhausted'
            note TEXT,
            updated_at TEXT NOT NULL
        )
    """)
    conn.commit()
    return conn


def insert_snapshot(conn, provider, window_type, pct_used=None, raw_used=None,
                     raw_limit=None, resets_at=None, source="unknown"):
    conn.execute(
        "INSERT INTO snapshots (provider, window_type, pct_used, raw_used, raw_limit, "
        "resets_at, source, captured_at) VALUES (?,?,?,?,?,?,?,?)",
        (provider, window_type, pct_used, raw_used, raw_limit, resets_at, source,
         datetime.now(timezone.utc).isoformat()),
    )
    conn.commit()


def latest_snapshot(conn, provider, window_type=None):
    q = "SELECT * FROM snapshots WHERE provider=?"
    params = [provider]
    if window_type:
        q += " AND window_type=?"
        params.append(window_type)
    q += " ORDER BY captured_at DESC LIMIT 1"
    row = conn.execute(q, params).fetchone()
    if not row:
        return None
    cols = [d[0] for d in conn.execute(q, params).description]
    return dict(zip(cols, row))


def all_latest(conn):
    """Latest snapshot per (provider, window_type)."""
    rows = conn.execute("""
        SELECT s.* FROM snapshots s
        INNER JOIN (
            SELECT provider, window_type, MAX(captured_at) AS max_ts
            FROM snapshots GROUP BY provider, window_type
        ) m ON s.provider = m.provider AND s.window_type = m.window_type
              AND s.captured_at = m.max_ts
    """).fetchall()
    cols = ["id", "provider", "window_type", "pct_used", "raw_used", "raw_limit",
            "resets_at", "source", "captured_at"]
    return [dict(zip(cols, r)) for r in rows]


def already_alerted(conn, provider, window_type, threshold, resets_at):
    row = conn.execute(
        "SELECT 1 FROM alerts_sent WHERE provider=? AND window_type=? AND threshold=? "
        "AND (resets_at IS ? OR resets_at=?) LIMIT 1",
        (provider, window_type, threshold, resets_at, resets_at),
    ).fetchone()
    return row is not None


def record_alert(conn, provider, window_type, threshold, resets_at):
    conn.execute(
        "INSERT INTO alerts_sent (provider, window_type, threshold, sent_at, resets_at) "
        "VALUES (?,?,?,?,?)",
        (provider, window_type, threshold, datetime.now(timezone.utc).isoformat(), resets_at),
    )
    conn.commit()


def set_manual_status(conn, provider, status, note=""):
    conn.execute(
        "INSERT INTO manual_status (provider, status, note, updated_at) VALUES (?,?,?,?) "
        "ON CONFLICT(provider) DO UPDATE SET status=excluded.status, note=excluded.note, "
        "updated_at=excluded.updated_at",
        (provider, status, note, datetime.now(timezone.utc).isoformat()),
    )
    conn.commit()


def get_manual_status(conn, provider):
    row = conn.execute(
        "SELECT status, note, updated_at FROM manual_status WHERE provider=?", (provider,)
    ).fetchone()
    if not row:
        return None
    return {"status": row[0], "note": row[1], "updated_at": row[2]}
