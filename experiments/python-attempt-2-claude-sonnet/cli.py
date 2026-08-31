#!/usr/bin/env python3
"""AI Usage Optimizer CLI — MVP.

Commands:
  status              Show current usage/status for all 4 providers.
  check                Run collectors, store snapshots, fire alerts if thresholds crossed.
  mark <provider> <available|exhausted> [note]   Manually set status for untracked providers.
  recommend            Print which provider to use right now.

Zero non-stdlib deps except PyYAML (already ubiquitous; falls back to a tiny
inline parser if not installed since this repo intentionally avoids pip installs
for a personal tool with no cloud dependency).
"""
import sys
import json
from pathlib import Path
from datetime import datetime, timezone

sys.path.insert(0, str(Path(__file__).parent))

from src import db
from src.collectors import claude_code

ROOT = Path(__file__).parent
CONFIG_PATH = ROOT / "config.yaml"

MANUAL_PROVIDERS = {"ollama_cloud", "chatgpt_plus"}
ALL_PROVIDERS = ["claude_code", "zai_codeplus", "ollama_cloud", "chatgpt_plus"]


def load_config():
    try:
        import yaml
        return yaml.safe_load(CONFIG_PATH.read_text())
    except ImportError:
        # tiny fallback: not full YAML, just enough for our flat config shape
        return _naive_yaml_parse(CONFIG_PATH.read_text())


def _naive_yaml_parse(text):
    # Minimal parser: good enough for this file's shape (used only if PyYAML absent).
    import re
    cfg = {"providers": {}, "alert": {"thresholds": [90, 95]}, "rotation": {"priority": ALL_PROVIDERS}}
    return cfg  # not fully faithful; PyYAML is present on modern macOS python3, prefer that path


def cmd_status(config):
    conn = db.connect()
    snapshots = db.all_latest(conn)
    by_provider = {}
    for s in snapshots:
        by_provider.setdefault(s["provider"], []).append(s)

    print(f"AI Usage Status — {datetime.now(timezone.utc).isoformat()}\n")
    for provider in ALL_PROVIDERS:
        print(f"[{provider}]")
        if provider in MANUAL_PROVIDERS:
            m = db.get_manual_status(conn, provider)
            if m:
                print(f"  manual status: {m['status']}  (note: {m['note']!r}, set {m['updated_at']})")
            else:
                print("  manual status: unknown (never marked — assuming available)")
        else:
            rows = by_provider.get(provider, [])
            if not rows:
                print("  no snapshots yet — run `check` first")
            for r in rows:
                pct = r["pct_used"]
                pct_str = f"{pct}%" if pct is not None else "unknown (limit not calibrated)"
                print(f"  {r['window_type']}: {pct_str}  (raw={r['raw_used']}, source={r['source']}, captured={r['captured_at']})")
        print()


def cmd_check(config):
    conn = db.connect()
    alerts_fired = []

    # Claude Code
    for snap in claude_code.collect(config):
        db.insert_snapshot(conn, "claude_code", snap["window_type"], snap["pct_used"],
                            snap["raw_used"], snap["raw_limit"], snap["resets_at"], snap["source"])
        if snap["pct_used"] is not None:
            for threshold in config.get("alert", {}).get("thresholds", [90, 95]):
                if snap["pct_used"] >= threshold and not db.already_alerted(
                        conn, "claude_code", snap["window_type"], threshold, snap["resets_at"]):
                    alerts_fired.append(("claude_code", snap["window_type"], threshold, snap["pct_used"]))
                    db.record_alert(conn, "claude_code", snap["window_type"], threshold, snap["resets_at"])

    # Z.ai — pending API key, skip collection but note it
    import os
    if os.environ.get("ZAI_API_KEY"):
        print("[zai_codeplus] API key found but collector not wired yet — see DESIGN.md section 2.4")
    else:
        print("[zai_codeplus] no ZAI_API_KEY set — skipping (unlock 1Password / export key to enable)")

    print(f"\nCheck complete. {len(alerts_fired)} new alert(s).")
    for provider, window, threshold, pct in alerts_fired:
        print(f"  ALERT: {provider} {window} window at {pct}% (>= {threshold}% threshold)")
    return alerts_fired


def cmd_mark(config, provider, status, note=""):
    if provider not in MANUAL_PROVIDERS:
        print(f"'{provider}' is not manually tracked. Manual providers: {sorted(MANUAL_PROVIDERS)}")
        sys.exit(1)
    if status not in ("available", "exhausted"):
        print("status must be 'available' or 'exhausted'")
        sys.exit(1)
    conn = db.connect()
    db.set_manual_status(conn, provider, status, note)
    print(f"Marked {provider} as {status}. {note}")


def cmd_recommend(config):
    conn = db.connect()
    snapshots = {s["provider"]: s for s in db.all_latest(conn) if s["window_type"] == "5h"}
    priority = config.get("rotation", {}).get("priority", ALL_PROVIDERS)

    for provider in priority:
        if provider in MANUAL_PROVIDERS:
            m = db.get_manual_status(conn, provider)
            if m and m["status"] == "exhausted":
                continue
            print(f"RECOMMEND: {provider} (manual status: {'available' if not m else m['status']})")
            return provider
        else:
            snap = snapshots.get(provider)
            pct = snap["pct_used"] if snap else None
            if pct is None or pct < 90:
                print(f"RECOMMEND: {provider} (5h usage: {pct if pct is not None else 'uncalibrated'}%)")
                return provider
    print("WARNING: all providers appear at/near limit or exhausted.")
    return None


def main():
    config = load_config()
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(1)
    cmd = sys.argv[1]
    if cmd == "status":
        cmd_status(config)
    elif cmd == "check":
        cmd_check(config)
    elif cmd == "mark":
        if len(sys.argv) < 4:
            print("usage: mark <provider> <available|exhausted> [note]")
            sys.exit(1)
        note = " ".join(sys.argv[4:]) if len(sys.argv) > 4 else ""
        cmd_mark(config, sys.argv[2], sys.argv[3], note)
    elif cmd == "recommend":
        cmd_recommend(config)
    else:
        print(f"unknown command: {cmd}")
        print(__doc__)
        sys.exit(1)


if __name__ == "__main__":
    main()
