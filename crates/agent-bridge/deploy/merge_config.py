#!/usr/bin/env python3
"""Merge legacy installer entries without deleting Codex/OpenCode identities or notify targets."""
import fcntl
import hashlib
import json
import os
import sys
from pathlib import Path


def merge(config):
    config = Path(config)
    with open(config / '.agents.lock', 'a') as gate:
        fcntl.flock(gate, fcntl.LOCK_EX)
        agents_file, notify_file = config / 'agents.json', config / 'notify.json'
        agents = json.loads(agents_file.read_text()) if agents_file.exists() else {}
        notify = json.loads(notify_file.read_text()) if notify_file.exists() else {}
        if not isinstance(agents, dict) or not isinstance(notify, dict):
            raise ValueError('agents.json and notify.json must be objects')
        for agent, name in [('claude', 'token'), ('dsh', 'dsh.token')]:
            token = (config / name).read_text().strip()
            if len(token) < 32:
                raise ValueError('token too short: ' + name)
            digest = hashlib.sha256(token.encode()).hexdigest()
            if agent in agents and agents[agent] != digest:
                raise ValueError('token mismatch for ' + agent + '; refusing identity rotation')
            agents[agent] = digest
        notify.setdefault('dsh', {'url': 'http://127.0.0.1:3080/dsh-web-relay/bridge/notify',
            'token_file': str(config / 'dsh-notify.token'), 'transport': 'auto'})
        for path, value in [(agents_file, agents), (notify_file, notify)]:
            tmp = path.with_name('.' + path.name + '.tmp')
            fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
            with os.fdopen(fd, 'w') as f:
                os.fchmod(f.fileno(), 0o600)
                json.dump(value, f, indent=2)
                f.flush()
                os.fsync(f.fileno())
            os.replace(tmp, path)
        fd = os.open(config, os.O_RDONLY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)


if __name__ == '__main__':
    merge(sys.argv[1])
