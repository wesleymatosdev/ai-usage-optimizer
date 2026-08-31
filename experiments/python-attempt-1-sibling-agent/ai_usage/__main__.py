import argparse
import json
import os
import sqlite3
import sys
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

HOME = Path.home()
DEFAULT_CONFIG = HOME / '.config' / 'ai-usage-optimizer' / 'config.json'
DEFAULT_DB = HOME / '.local' / 'share' / 'ai-usage-optimizer' / 'usage.sqlite3'
PROVIDERS = ('claude-pro', 'zai-codeplus', 'chatgpt-plus', 'ollama-pro')


def now():
    return datetime.now(timezone.utc).isoformat()


def defaults():
    return {
        'thresholds': {'warning': 90, 'critical': 95},
        'rotation_order': ['claude-pro', 'zai-codeplus', 'chatgpt-plus', 'ollama-pro'],
        'providers': {
            'claude-pro': {'kind': 'claude_local', 'five_hour_token_budget': 225000},
            'zai-codeplus': {'kind': 'zai_quota', 'api_key_env': 'ZAI_API_KEY', 'endpoint': 'https://api.z.ai/api/coding/paas/v4/api/monitor/usage/quota/limit'},
            'chatgpt-plus': {'kind': 'manual', 'note': 'ChatGPT consumer subscriptions have no supported usage API.'},
            'ollama-pro': {'kind': 'manual', 'note': 'Ollama cloud subscription usage has no documented quota endpoint.'},
        },
    }


def load_config(path):
    if not path.exists():
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(defaults(), indent=2) + '\n')
    return json.loads(path.read_text())


def db_open(path):
    path.parent.mkdir(parents=True, exist_ok=True)
    con = sqlite3.connect(path)
    con.execute('CREATE TABLE IF NOT EXISTS observations (id INTEGER PRIMARY KEY, provider TEXT NOT NULL, percent REAL, source TEXT NOT NULL, note TEXT, observed_at TEXT NOT NULL)')
    con.execute('CREATE TABLE IF NOT EXISTS alerts (id INTEGER PRIMARY KEY, provider TEXT NOT NULL, level TEXT NOT NULL, percent REAL NOT NULL, message TEXT NOT NULL, fired_at TEXT NOT NULL)')
    return con


def observe(con, provider, percent, source, note=''):
    if provider not in PROVIDERS:
        raise ValueError('unknown provider: ' + provider)
    if percent is not None and not 0 <= percent <= 100:
        raise ValueError('percent must be between 0 and 100')
    con.execute('INSERT INTO observations(provider,percent,source,note,observed_at) VALUES(?,?,?,?,?)', (provider, percent, source, note, now()))
    con.commit()


def latest(con):
    rows = con.execute('SELECT o.provider,o.percent,o.source,o.note,o.observed_at FROM observations o JOIN (SELECT provider,max(id) id FROM observations GROUP BY provider) x ON o.id=x.id').fetchall()
    return {r[0]: {'percent': r[1], 'source': r[2], 'note': r[3], 'at': r[4]} for r in rows}


def collect_zai(config):
    item = config['providers']['zai-codeplus']
    key = os.environ.get(item.get('api_key_env', 'ZAI_API_KEY'))
    if not key:
        return None, 'ZAI_API_KEY is not set'
    request = urllib.request.Request(item['endpoint'], headers={'Authorization': key, 'Accept-Language': 'en-US,en;q=0.9'})
    try:
        with urllib.request.urlopen(request, timeout=15) as response:
            data = json.load(response)
    except (urllib.error.URLError, urllib.error.HTTPError, json.JSONDecodeError) as exc:
        return None, 'Z.ai quota request failed: ' + str(exc)
    raw = data.get('data', data)
    values = []
    for key_name in ('tokenUsage5Hour', 'mcpUsage1Month'):
        value = raw.get(key_name)
        if isinstance(value, (int, float)):
            values.append(float(value))
    if not values:
        return None, 'Z.ai response did not contain a recognized quota percentage'
    return max(values), 'Z.ai direct quota endpoint'


def collect_claude(config):
    since = datetime.now(timezone.utc).timestamp() - 5 * 3600
    total = 0
    root = HOME / '.claude' / 'projects'
    for file in root.glob('**/*.jsonl'):
        try:
            for line in file.read_text(errors='ignore').splitlines():
                record = json.loads(line)
                message = record.get('message', {})
                usage = message.get('usage', {})
                stamp = record.get('timestamp') or message.get('timestamp')
                if not usage or not stamp:
                    continue
                parsed = datetime.fromisoformat(stamp.replace('Z', '+00:00')).timestamp()
                if parsed >= since:
                    total += sum(int(usage.get(k, 0) or 0) for k in ('input_tokens', 'output_tokens', 'cache_creation_input_tokens', 'cache_read_input_tokens'))
        except (OSError, ValueError, json.JSONDecodeError):
            continue
    budget = config['providers']['claude-pro']['five_hour_token_budget']
    return min(100.0, total * 100 / budget), f'local Claude JSONL estimate: {total}/{budget} tokens (may undercount other devices)'


def recommendation(config, states):
    ordered = config['rotation_order']
    candidates = [(states[p]['percent'], p) for p in ordered if p in states and states[p]['percent'] is not None and states[p]['percent'] < 90]
    if not candidates:
        return 'No provider has verified headroom. Record a current consumer-subscription observation before switching.'
    percent, provider = min(candidates)
    return f'Recommended next: {provider} ({100-percent:.0f}% verified headroom).'


def alert(con, config, states):
    thresholds = config['thresholds']
    for provider, state in states.items():
        pct = state['percent']
        if pct is None or pct < thresholds['warning']:
            continue
        level = 'critical' if pct >= thresholds['critical'] else 'warning'
        message = f'{provider} at {pct:.0f}% — {recommendation(config, states)}'
        con.execute('INSERT INTO alerts(provider,level,percent,message,fired_at) VALUES(?,?,?,?,?)', (provider, level, pct, message, now()))
        print('ALERT:', message)
    con.commit()


def main(argv=None):
    parser = argparse.ArgumentParser(prog='ai-usage')
    parser.add_argument('--config', type=Path, default=DEFAULT_CONFIG)
    parser.add_argument('--db', type=Path, default=DEFAULT_DB)
    sub = parser.add_subparsers(dest='cmd', required=True)
    sub.add_parser('init')
    sub.add_parser('status')
    sub.add_parser('collect')
    obs = sub.add_parser('observe'); obs.add_argument('provider', choices=PROVIDERS); obs.add_argument('percent', type=float); obs.add_argument('--note', default='manual observation')
    hit = sub.add_parser('limit-hit'); hit.add_argument('provider', choices=PROVIDERS); hit.add_argument('--note', default='provider reported limit/rate exhaustion')
    args = parser.parse_args(argv)
    cfg = load_config(args.config); con = db_open(args.db)
    if args.cmd == 'init':
        print(args.config); return 0
    if args.cmd == 'observe':
        observe(con, args.provider, args.percent, 'manual', args.note); print('recorded'); return 0
    if args.cmd == 'limit-hit':
        observe(con, args.provider, 100, 'limit-hit', args.note); print('recorded'); return 0
    if args.cmd == 'collect':
        pct, note = collect_claude(cfg); observe(con, 'claude-pro', pct, 'local-jsonl', note)
        pct, note = collect_zai(cfg)
        if pct is not None: observe(con, 'zai-codeplus', pct, 'direct-api', note)
        else: print(note, file=sys.stderr)
    states = latest(con)
    if args.cmd in ('collect', 'status'):
        for provider in PROVIDERS:
            state = states.get(provider)
            print(f"{provider:14} " + (f"{state['percent']:5.1f}%  {state['source']}  {state['note']}" if state else 'unknown — no observation'))
        print(recommendation(cfg, states)); alert(con, cfg, states)
    return 0

if __name__ == '__main__':
    raise SystemExit(main())
