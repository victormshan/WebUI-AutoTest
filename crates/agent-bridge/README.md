# agent-bridge

Two-way task dispatch and collaboration between agents — Claude Code (WSL) and the DSH main
agent (Windows). Either side can hand the other a task; the service, not the agents' good
intentions, enforces who may say what and when. The design was negotiated with the DSH main
agent over four rounds (`D:\cc-tasks\claude-bridge\DESIGN-agent-bridge-FINAL.md`) and replaces the
file protocol both sides used before.

It is a coordination service, **not a security boundary**: whether code may be committed is still
decided by each side's own review gate (review-gate for Claude).

```
 Claude Code (WSL)                       agent-bridge (WSL, 127.0.0.1:7879)            DSH main agent (Windows)
 ─────────────────                       ──────────────────────────────────            ────────────────────────
 agent-bridge send / post / MCP  ──────▶  append-only log, state machine   ◀──────  HTTP (Node reference client)
 agent-bridge wait (background)  ◀──────  long poll: returns on arrival
                                          push ────────────── curl.exe ──────────▶  /dsh-web-relay/bridge/notify
                                          monitor: re-wake 15 min, stalled 24 h          → wakeMainAgent
                                          /v1/user/pending ◀── what only the user decides
                                          read-only mirror ─────────────────────▶  D:\cc-tasks\claude-bridge\tasks\<id>\
```

## A task

`task → ack → (question ⇄ answer)* → progress* → result (with evidence) → verdict → close`

| Sent by | Kinds |
|---|---|
| receiver | `ack`, `question`, `progress`, `result` (`outcome`: done / blocked / rejected) |
| requester | `answer`, `verdict` (`judgement`: pass / rework), `close`, `cancel` |
| service only | `resume` (after the user's recorded decision) |

- The service assigns message numbers; messages are appended, never edited. Correcting yourself
  is a new message with `supersedes` pointing at the old one.
- At most 3 question rounds and 12 messages per task; then the task is **paused for the user**
  and only close/cancel are possible until the user decides.
- `client_msg_id` makes retries idempotent: posting the same key again returns the stored message.
- The token identifies the sender; a `from` in the request body is ignored.

## What only the user decides

Anything needing elevated rights, irreversible or outward-facing actions, spending money or
changing user data goes into a message's `needs_user` — neither agent decides it for the user, and
a generic "继续 / ok" is not consent to it. `GET /v1/user/pending` lists those items, paused tasks
and stalled tasks. A decision is recorded with `POST /v1/user/decisions {item, verbatim}`: the
user's words verbatim, their SHA-256 and the relaying agent, append-only; a different text for the
same item is kept and marks a conflict. Deciding a paused item resumes the task.

## Waking the other side

- **Claude** keeps `agent-bridge wait` running in the background; it exits when a message arrives,
  which wakes the session. Reading never marks read — `ack` does (at-least-once delivery).
- **DSH** has no waiter: the service pushes to `/dsh-web-relay/bridge/notify`. WSL cannot reach
  Windows loopback, so the push goes through Windows `curl.exe` (token on stdin). HTTP 200 is not a
  wake: only the endpoint's `agentWoken:true` counts. Every attempt is in `wakes.jsonl` and
  `/v1/health.wakes`.
- A blocking message still unread after 15 minutes is re-woken once; a task without progress for
  24 hours is stalled (user-visible, one notice per day).

## Install (once, as your user — no sudo)

```bash
cargo build --release -p agent-bridge
bash crates/agent-bridge/deploy/install.sh
```

It creates three random tokens in `~/.config/agent-bridge/` (0600), writes `agents.json` (hashes
only) and `notify.json`, sets the Windows user variables `DSH_AGENT_BRIDGE_URL`,
`DSH_AGENT_BRIDGE_TOKEN` and `DSH_RELAY_BRIDGE_NOTIFY_TOKEN` (values via stdin), imports finished
file-protocol tasks, starts the systemd user service `agent-bridge.service`, and registers a
Windows logon task that starts WSL. The DSH host sees the new variables only after it restarts,
which rotates the phone link — **you decide when**.

## Use

```bash
agent-bridge health
agent-bridge send --id fix-123 --to dsh --title "…" --body-file task.md
agent-bridge inbox            # unread for me (does not mark read)
agent-bridge post --task fix-123 --kind verdict --judgement pass
agent-bridge ack --task fix-123 --n 4
agent-bridge wait             # background: exits 0 when something arrives, 3 on timeout
```

MCP (Claude Code): `{"mcpServers": {"agent-bridge": {"command": "agent-bridge", "args": ["mcp"]}}}` —
tools `bridge_tasks`, `bridge_show`, `bridge_send`, `bridge_post`, `bridge_inbox`, `bridge_ack`.

Node (DSH): `crates/agent-bridge/clients/node/bridge-client.mjs` (reference client: built-in fetch,
4xx final, idempotent retries, explicit UTF-8).

## HTTP API `/v1`

All routes except `/v1/health` need `Authorization: Bearer <token>`.

| Route | |
|---|---|
| `GET /v1/health` | status, open tasks, live waiters, unread per agent, pending for user, stalled, wakes, mirror |
| `POST /v1/tasks` | `{id, kind:"task", protocol:"1", body, meta:{to, title, priority?, expr_id?}, client_msg_id?}` |
| `GET /v1/tasks[?open=true]` · `GET /v1/tasks/{id}` | tasks you take part in · one task with its messages |
| `POST /v1/tasks/{id}/messages` | `{kind, protocol:"1", body?, questions?, results?, needs_user?, outcome?, judgement?, reply_to?, supersedes?, client_msg_id?}` |
| `GET /v1/inbox?wait=N` · `POST /v1/inbox/ack {task, n}` | unread for you (long poll ≤120 s) · mark handled |
| `GET /v1/user/pending[?all=true]` · `POST /v1/user/decisions {item, verbatim}` | user items · record a decision |

Errors: 400 invalid, 401 token, 403 role/not a party, 404 no task, 409 state/exists/conflict.

## Storage

`~/.local/state/agent-bridge/` (0700): one append-only JSONL per task (fsync before replying; a bad
line is skipped and counted, never fatal), read cursors, `decisions.jsonl`, `wakes.jsonl`. The
mirror under `D:\cc-tasks\claude-bridge\tasks\` is written by one background thread and is **not a
data source**: files there that the store does not have are reported in `/v1/health`, not adopted.
