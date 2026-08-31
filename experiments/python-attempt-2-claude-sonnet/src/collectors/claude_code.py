"""Claude Code usage collector — parses local JSONL session logs.

No API key needed. Reads ~/.claude/projects/**/*.jsonl, sums token usage
in the last rolling 5h and 7d windows, and estimates % of plan limit.

Plan limits are NOT published by Anthropic for Pro/Max — these are
Wesley-calibrated placeholders (see config.yaml) until real /usage
output is captured to calibrate against.
"""
import json
import glob
from pathlib import Path
from datetime import datetime, timedelta, timezone

CLAUDE_PROJECTS_DIR = Path.home() / ".claude" / "projects"


def _parse_ts(entry):
    ts = entry.get("timestamp")
    if not ts:
        return None
    try:
        return datetime.fromisoformat(ts.replace("Z", "+00:00"))
    except (ValueError, TypeError):
        return None


def iter_usage_events(since=None):
    """Yield (timestamp, input_tokens, output_tokens, cache_read, cache_creation)
    for every message with usage info across all session JSONL files."""
    if not CLAUDE_PROJECTS_DIR.exists():
        return
    for path in glob.glob(str(CLAUDE_PROJECTS_DIR / "**" / "*.jsonl"), recursive=True):
        try:
            with open(path, "r", errors="ignore") as f:
                for line in f:
                    line = line.strip()
                    if not line:
                        continue
                    try:
                        entry = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    ts = _parse_ts(entry)
                    if ts is None:
                        continue
                    if since and ts < since:
                        continue
                    usage = (entry.get("message") or {}).get("usage")
                    if not usage:
                        continue
                    yield (
                        ts,
                        usage.get("input_tokens", 0) or 0,
                        usage.get("output_tokens", 0) or 0,
                        usage.get("cache_read_input_tokens", 0) or 0,
                        usage.get("cache_creation_input_tokens", 0) or 0,
                    )
        except (OSError, PermissionError):
            continue


def window_totals(hours):
    since = datetime.now(timezone.utc) - timedelta(hours=hours)
    total_in = total_out = total_cache_read = total_cache_create = 0
    for ts, inp, out, cr, cc in iter_usage_events(since=since):
        total_in += inp
        total_out += out
        total_cache_read += cr
        total_cache_create += cc
    return {
        "input_tokens": total_in,
        "output_tokens": total_out,
        "cache_read_tokens": total_cache_read,
        "cache_creation_tokens": total_cache_create,
        "total_tokens": total_in + total_out + total_cache_read + total_cache_create,
    }


def collect(config):
    """Returns list of snapshot dicts ready for db.insert_snapshot (window_type, pct_used, ...)."""
    limits = config.get("claude_code", {}).get("estimated_limits", {})
    results = []

    five_h = window_totals(5)
    limit_5h = limits.get("5h_tokens")
    pct_5h = round(100 * five_h["total_tokens"] / limit_5h, 1) if limit_5h else None
    results.append({
        "window_type": "5h",
        "pct_used": pct_5h,
        "raw_used": five_h["total_tokens"],
        "raw_limit": limit_5h,
        "resets_at": None,  # rolling window, not fixed reset
        "source": "jsonl",
    })

    weekly = window_totals(24 * 7)
    limit_week = limits.get("weekly_tokens")
    pct_week = round(100 * weekly["total_tokens"] / limit_week, 1) if limit_week else None
    results.append({
        "window_type": "weekly",
        "pct_used": pct_week,
        "raw_used": weekly["total_tokens"],
        "raw_limit": limit_week,
        "resets_at": None,
        "source": "jsonl",
    })

    return results


if __name__ == "__main__":
    print("Last 5h:", window_totals(5))
    print("Last 7d:", window_totals(24 * 7))
