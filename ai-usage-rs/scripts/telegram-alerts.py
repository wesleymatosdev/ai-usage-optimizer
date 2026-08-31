#!/usr/bin/env python3
"""ai-usage Telegram alerter — pushes only on level TRANSITIONS per provider.

Reads the latest observations from the ai-usage SQLite db, computes each
provider's alert level (ok / warning 90%+ / critical 95%+), compares with the
stored previous state, and sends a Telegram message only for changes
(including recovery). First run is a silent baseline. Token comes from
TELEGRAM_BOT_TOKEN in the environment (sourced from ~/.hermes/.env by the
launchd wrapper) — never hardcoded.
"""

import json
import os
import sqlite3
import sys
import urllib.request
from urllib.parse import urlencode

DB = os.path.expanduser("~/.local/share/ai-usage-optimizer/usage.sqlite3")
STATE = os.path.expanduser("~/.local/share/ai-usage-optimizer/alert-state.json")
WARNING, CRITICAL = 90.0, 95.0


def level_for(pct):
    if pct is None:
        return "unknown"
    if pct >= CRITICAL:
        return "critical"
    if pct >= WARNING:
        return "warning"
    return "ok"


def latest_levels():
    conn = sqlite3.connect(DB)
    rows = conn.execute(
        "SELECT o.provider, o.percent FROM observations o "
        "JOIN (SELECT provider, MAX(id) AS id FROM observations GROUP BY provider) x "
        "ON o.id = x.id"
    ).fetchall()
    conn.close()
    return {p: level_for(pct) for p, pct in rows}


def send_telegram(text):
    token = os.environ.get("TELEGRAM_BOT_TOKEN")
    chat = os.environ.get("TELEGRAM_HOME_CHANNEL") or os.environ.get("TELEGRAM_ALLOWED_USERS")
    if not token or not chat:
        print("telegram: missing token/chat, skipping push", file=sys.stderr)
        return False
    url = f"https://api.telegram.org/bot{token}/sendMessage"
    data = urlencode({"chat_id": chat, "text": text}).encode()
    try:
        with urllib.request.urlopen(url, data=data, timeout=15) as r:
            return r.status == 200
    except Exception as e:
        print(f"telegram send failed: {e}", file=sys.stderr)
        return False


def main():
    current = latest_levels()
    try:
        with open(STATE) as f:
            previous = json.load(f)
    except (OSError, ValueError):
        previous = None

    if previous is None:
        with open(STATE, "w") as f:
            json.dump(current, f, indent=2)
        print("baseline recorded:", current)
        return

    changes = {
        p: lvl for p, lvl in current.items() if previous.get(p) != lvl and lvl != "unknown"
    }
    if not changes:
        print("no level changes")
        return

    # Pull latest percent + note per provider so pushes are self-explanatory.
    conn = sqlite3.connect(DB)
    details = dict(
        conn.execute(
            "SELECT o.provider, o.percent || '% — ' || COALESCE(o.note, '') FROM observations o "
            "JOIN (SELECT provider, MAX(id) AS id FROM observations GROUP BY provider) x "
            "ON o.id = x.id"
        ).fetchall()
    )
    conn.close()

    for provider, lvl in sorted(changes.items()):
        old = previous.get(provider, "unknown")
        detail = details.get(provider, "")
        if lvl == "ok":
            msg = f"ai-usage: {provider} back under {WARNING:.0f}% (was {old}). Now: {detail}"
        else:
            msg = f"ai-usage: {provider} hit {lvl.upper()} threshold (was {old}). Now: {detail}"
        if send_telegram(msg):
            print(f"pushed: {msg}")
        previous[provider] = lvl

    with open(STATE, "w") as f:
        json.dump(previous, f, indent=2)


if __name__ == "__main__":
    main()
