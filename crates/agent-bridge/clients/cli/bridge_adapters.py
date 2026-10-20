#!/usr/bin/env python3
"""Native client adapters for bridge_agents (standard library only).

An adapter owns everything that differs between the native CLIs: how to build the
command line and environment (including the temporary agent_bridge MCP server), and
how to read the CLI's streamed JSON events.  Shared protocol, journal, identity and
state handling stay in bridge_agents.py.

Deliberately small: two concrete adapters behind one explicit interface, plus a
compatibility ``build_launch`` function.  No plugin registry.
"""
import json
import os
import subprocess
from pathlib import Path
from urllib.parse import urlsplit


# Explicit opt-in for this local task channel; human decision/asked tools stay gated.
APPROVED_BRIDGE_TOOLS = (
    'bridge_identity', 'bridge_tasks', 'bridge_show', 'bridge_send', 'bridge_post',
    'bridge_inbox', 'bridge_ack', 'bridge_pending', 'bridge_presence', 'bridge_health',
)


def approved_bridge_tools(args):
    if not getattr(args, 'approve_bridge_tools', False):
        return ()
    if args.client != 'codex':
        raise ValueError('--approve-bridge-tools applies only to Codex')
    endpoint = urlsplit(args.url)
    if (endpoint.scheme not in ('http', 'https')
            or endpoint.hostname not in ('localhost', '127.0.0.1', '::1')
            or endpoint.username is not None or endpoint.password is not None
            or endpoint.query or endpoint.fragment):
        raise ValueError('--approve-bridge-tools requires a local loopback bridge URL')
    return APPROVED_BRIDGE_TOOLS


def mcp_config(binary, token_file, url, windows=False):
    binary = str(Path(binary).expanduser().resolve())
    token_file = str(Path(token_file).expanduser().resolve())
    env = {'AGENT_BRIDGE_URL': url, 'AGENT_BRIDGE_TOKEN_FILE': token_file,
           # This must shadow any parent token; Rust ignores an empty token.
           'AGENT_BRIDGE_TOKEN': ''}
    if windows:
        return {'command': 'wsl.exe', 'args': ['-d', os.environ.get('WSL_DISTRO_NAME', 'Ubuntu'),
            '-e', 'env', *[k + '=' + v for k, v in env.items()], binary, 'mcp'], 'env': {}}
    return {'command': binary, 'args': ['mcp'], 'env': env}


def merged(left, right):
    result = dict(left)
    for key, value in right.items():
        result[key] = merged(result[key], value) if isinstance(value, dict) and isinstance(result.get(key), dict) else value
    return result


class StreamResult:
    """What a finished CLI run tells us: the session id it used and whether it failed.

    ``approval_denied`` is separate because that failure means the client refused the
    MCP tools and no connector flag may override it.
    """
    __slots__ = ('session', 'approval_denied', 'failed', 'error')

    def __init__(self, session=None, approval_denied=False, failed=False, error=None):
        self.session = session
        self.approval_denied = approval_denied
        self.failed = failed
        self.error = error


class Adapter:
    """Explicit interface for one native client."""

    client = None

    def __init__(self, args):
        self.args = args

    def _env(self):
        env = os.environ.copy()
        env.pop('AGENT_BRIDGE_TOKEN', None)
        env.update(AGENT_BRIDGE_TOKEN_FILE=self.args.token_file, AGENT_BRIDGE_URL=self.args.url)
        return env

    def _exe(self):
        return self.args.client_bin or self.args.client

    def build_launch(self, prompt, session=None):
        """Return (cmd, env, stdin); stdin is a string for clients that read the prompt
        on stdin and None for clients that pass the prompt as an argument."""
        raise NotImplementedError

    def parse_stream(self, stdout, session=None):
        """Parse streamed JSON events.  Returns a StreamResult.  `session` is the prior
        session id, kept when the run did not report a new one."""
        events = []
        for line in (stdout or '').splitlines():
            try:
                events.append(json.loads(line))
            except json.JSONDecodeError:
                continue
        approval_denied = any(
            e.get('item', {}).get('type') == 'mcp_tool_call'
            and e.get('item', {}).get('server') == 'agent_bridge'
            and 'requires approval' in str(e.get('item', {}).get('error', {}))
            for e in events)
        failed = approval_denied or any(e.get('type') in ('error', 'turn.failed') for e in events)
        found = self.session_from(events)
        return StreamResult(found or session, approval_denied, failed,
                            'approval required' if approval_denied else None)

    def session_from(self, events):
        return None


class CodexAdapter(Adapter):
    client = 'codex'

    def build_launch(self, prompt, session=None):
        args = self.args
        env = self._env()
        exe = self._exe()
        windows = exe.lower().endswith('.exe')
        cfg = mcp_config(args.bridge_bin, args.token_file, args.url, windows)
        cmd = [exe, 'exec']
        if session:
            cmd += ['resume', session]
        cmd += ['--json', '--skip-git-repo-check']
        if not session:
            workdir = str(Path(args.workdir).resolve())
            if windows:
                if not workdir.startswith('/mnt/'):
                    raise ValueError('Windows Codex requires a workdir on a mounted Windows drive; otherwise use Linux Codex')
                workdir = subprocess.check_output(['wslpath', '-w', workdir], text=True).strip()
            cmd += ['-C', workdir]
        for key, value in [('command', cfg['command']), ('args', cfg['args']), ('required', True)]:
            literal = 'true' if value is True else json.dumps(value)
            cmd += ['-c', 'mcp_servers.agent_bridge.' + key + '=' + literal]
        for key, value in cfg['env'].items():
            cmd += ['-c', 'mcp_servers.agent_bridge.env.' + key + '=' + json.dumps(value)]
        for tool in approved_bridge_tools(args):
            cmd += ['-c', 'mcp_servers.agent_bridge.tools.' + tool + '.approval_mode="approve"']
        if args.model:
            cmd += ['--model', args.model]
        cmd.append('-')
        return cmd, env, prompt

    def session_from(self, events):
        sid = None
        for e in events:
            if e.get('type') == 'thread.started':
                sid = e.get('thread_id') or sid
            if e.get('sessionID'):
                sid = e['sessionID']
        return sid


class OpenCodeAdapter(Adapter):
    client = 'opencode'

    def build_launch(self, prompt, session=None):
        args = self.args
        env = self._env()
        exe = self._exe()
        windows = exe.lower().endswith('.exe')
        if windows:
            raise ValueError('Use OpenCode in WSL for this connector')
        cfg = mcp_config(args.bridge_bin, args.token_file, args.url, windows)
        existing = json.loads(env.get('OPENCODE_CONFIG_CONTENT', '{}'))
        entry = {'type': 'local', 'command': [cfg['command'], *cfg['args']],
                 'environment': cfg['env'], 'enabled': True, 'timeout': 30000}
        env['OPENCODE_CONFIG_CONTENT'] = json.dumps(merged(existing, {'mcp': {'agent_bridge': entry}}))
        cmd = [exe, 'run', '--format', 'json']
        if session:
            cmd += ['--session', session]
        if args.model:
            cmd += ['--model', args.model]
        cmd.append(prompt)
        return cmd, env, None

    def session_from(self, events):
        sid = None
        for e in events:
            if e.get('sessionID'):
                sid = e['sessionID']
        return sid


ADAPTERS = {'codex': CodexAdapter, 'opencode': OpenCodeAdapter}


def adapter(args):
    approved_bridge_tools(args)
    try:
        cls = ADAPTERS[args.client]
    except KeyError:
        raise ValueError('unknown client ' + str(getattr(args, 'client', None)))
    return cls(args)


def build_launch(args, prompt, session=None):
    """Compatibility wrapper matching the previous bridge_agents.build_launch."""
    return adapter(args).build_launch(prompt, session)
