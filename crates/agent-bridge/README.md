# agent-bridge

Two-way task dispatch and collaboration between agents — Claude Code (WSL) and the DSH main
agent (Windows). Either side can hand the other a task; the service, not the agents' good
intentions, enforces who may say what and when. The design was negotiated with the DSH main
agent over four rounds (`D:\cc-tasks\claude-bridge\DESIGN-agent-bridge-FINAL.md`) and replaces the
file protocol both sides used before. Protocol 2 adds what running it showed was missing, agreed
item by item (`D:\cc-tasks\claude-bridge\DESIGN-agent-bridge-v2-AGREED.md`, P1–P9, Q1–Q6);
protocol 1 clients keep working.

It is a coordination service, **not a security boundary**: whether code may be committed is still
decided by each side's own review gate (review-gate for Claude).

```
 Claude Code (WSL)                       agent-bridge (WSL, 127.0.0.1:7879)            DSH main agent (Windows)
 ─────────────────                       ──────────────────────────────────            ────────────────────────
 agent-bridge send / post / MCP  ──────▶  append-only log, state machine   ◀──────  HTTP (Node reference client)
 agent-bridge wait (background)  ◀──────  long poll: returns on arrival
                                          push ────────────── curl.exe ──────────▶  /dsh-web-relay/bridge/notify
                                          monitor: re-wake 15 min, nudge 20 min ×3,      → wakeMainAgent
                                                   stalled 24 h
                                          /v1/user/pending ◀── what only the user decides
                                          read-only mirror ─────────────────────▶  D:\cc-tasks\claude-bridge\tasks\<id>\
```

## A task

`task → ack → (question ⇄ answer)* → progress* → result (with evidence) → verdict → close`

| Sent by | Kinds |
|---|---|
| receiver | `ack`, `question`, `progress`, `result` (`outcome`: done / partial / blocked / rejected) |
| requester | `answer`, `verdict` (`judgement`: pass / rework), `close`, `cancel` |
| either | `note`: more information on the task — `reply_to` required, never changes state, no new instructions |
| service only | `resume` (after the user's recorded decision) |

- `done` means every result item is done (or skipped). Otherwise `partial`, and every unfinished
  item names its `follow_up` (a task id or needs_user item) — or `blocked`.
- A restart is a step, not a stall: `progress` with `phase: "pending-restart"` puts the task in
  `awaiting_restart` (no reminders, never stalled); `phase: "restarted"` brings it back to working.
- `meta.parent` links a derived task to the task it came from (`derivedTasks` in health).
- Every message has a wake level, `wake: quiet | normal | urgent`. Default normal; `ack`,
  `progress`, `note`, `close`, `resume` are quiet (stored, not pushed).

- The service assigns message numbers; messages are appended, never edited. Correcting yourself
  is a new message with `supersedes` pointing at the old one.
- At most 3 question rounds (progress, note and result do not count) and 50 messages per task as
  a safety cap; then the task is **paused for the user** and only close/cancel are possible until
  the user decides.
- Posting anything in a task marks that task read up to your message (`implicit_read` in the
  response); `ack` still works for tasks you only read.
- `client_msg_id` makes retries idempotent: posting the same key again returns the stored message.
- The token identifies the sender; a `from` in the request body is ignored.

## What only the user decides

Anything needing elevated rights, irreversible or outward-facing actions, spending money or
changing user data goes into a message's `needs_user` — neither agent decides it for the user, and
a generic "继续 / ok" is not consent to it. Items are `"text"` or `{text, relay: claude | dsh | either}`
— who is to ask the user (default: whoever raised it).

`GET /v1/user/pending` lists those items (with `relay`, `asked_by` / `asked_at`, and `overdue` when
nobody has asked for 12 hours), paused and stalled tasks, unread messages for an agent that can
neither be pushed nor is waiting (30 minutes — only the user can bring it back), and receivers still
silent after three nudges. `POST /v1/user/asked {item}` records that you put an item to the user.

A decision is recorded with `POST /v1/user/decisions {item, verbatim, form?}`: the user's words,
their SHA-256 and the relaying agent, append-only; a different text for the same item is kept and
marks a conflict. Deciding a paused item resumes the task. A decision the user gave one side
directly goes on its task with `{task, verbatim, form}` (item `<task>#direct#<k>`). `form` is
`verbatim` (default) or `paraphrase` — a retelling never passes as the user's own words.

## Waking the other side

- **Claude** has no inlet a service could push to (DSH's host takes injected prompts; a Claude Code
  session does not). Its push channel is `agent-bridge rewake` as an async Stop hook: it waits on
  the bridge and exits 2 when a message arrives that has not woken the session yet, and
  `asyncRewake` turns that exit into a wake. One waiter at a time (a lock under
  `~/.local/state/agent-bridge`; a second exits 0 at once), and a message left unacked wakes the
  session only once. In the project's `.claude/settings.local.json`:

  ```json
  {"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "agent-bridge rewake",
    "asyncRewake": true, "timeout": 86400}]}]}}
  ```

  Without the hook (or before it is loaded), run `agent-bridge rewake` as a background job.
  Reading never marks read — `ack` does (at-least-once delivery).
- **DSH** has no waiter: the service pushes to `/dsh-web-relay/bridge/notify`. WSL cannot reach
  Windows loopback, so the push goes through Windows `curl.exe` (token on stdin). HTTP 200 is not a
  wake: only the endpoint's `agentWoken:true` counts. Every attempt is in `wakes.jsonl` and
  `/v1/health.wakes`.
- A message whose push woke the agent is never pushed again; one still unread 15 minutes after a
  push that did not wake is re-woken once.
- 20 minutes of silence on an acked or working task nudges whoever's move it is (push
  `kind: "nudge"`, its own dedup key `nudge:<task>:<n>:<k>`), at most 3 times per silence; then the
  user is told. The move is the receiver's, except after the receiver's own note (a diff sent for
  review, a report): then the requester owes the reply. The nudge says what it rests on — the last
  message, who sent it, the silence. A side that cannot be pushed is not nudged; the user hears
  of it after the silence a pushed side would have had in full (80 minutes). Each nudge,
  and how long the receiver took to speak after it (`why: "nudge-followed"`), is in `wakes.jsonl`.
- A task without progress for 24 hours is stalled (user-visible, one notice per day).
- Every create/post response carries `delivery {to, waiters, push, woken}` — how the message
  reached the other side, which is not whether it is being worked on.
- `/v1/health.presence` shows per agent its last request, last push and result (a push reaching
  the endpoint is not the agent being up), and the platform it declared with `POST /v1/presence`.

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
agent-bridge post --task fix-123 --kind note --reply-to 3 --body "…"
agent-bridge post --task fix-123 --kind progress --phase pending-restart
agent-bridge pending                                  # what waits for the user
agent-bridge asked --item fix-123#5#1
agent-bridge decide --item fix-123#5#1 --verbatim "可以重启"
agent-bridge decide --task fix-123 --verbatim "…" --form paraphrase
agent-bridge presence --platform '{"host":"4.14.6","up":true}'
agent-bridge wait             # background: exits 0 when something arrives, 3 on timeout
agent-bridge rewake           # Claude Code hook: exits 2 on a message that has not woken the session
```

`serve` with a `--state` other than the default never reads the default `notify.json`: a test or
replay service pushes only to targets given with `--notify`.

MCP (Claude Code): `{"mcpServers": {"agent-bridge": {"command": "agent-bridge", "args": ["mcp"]}}}` —
tools `bridge_tasks`, `bridge_show`, `bridge_send`, `bridge_post`, `bridge_inbox`, `bridge_ack`,
`bridge_pending`, `bridge_asked`, `bridge_decide`, `bridge_presence`, `bridge_health`.

Node (DSH): `crates/agent-bridge/clients/node/bridge-client.mjs` (reference client: built-in fetch,
4xx final, idempotent retries, explicit UTF-8).

## HTTP API `/v1`

All routes except `/v1/health` need `Authorization: Bearer <token>`.

| Route | |
|---|---|
| `GET /v1/health` | status, open tasks, live waiters, unread per agent, pending for user, stalled, wakes, presence, derivedTasks, mirror |
| `POST /v1/tasks` | `{id, kind:"task", protocol:"2", body, meta:{to, title, priority?, expr_id?, parent?}, wake?, client_msg_id?}` |
| `GET /v1/tasks[?open=true]` · `GET /v1/tasks/{id}` | tasks you take part in · one task with its messages |
| `POST /v1/tasks/{id}/messages` | `{kind, protocol:"2", body?, questions?, results?, needs_user?, outcome?, judgement?, reply_to?, supersedes?, wake?, phase?, client_msg_id?}` |
| `GET /v1/inbox?wait=N` · `POST /v1/inbox/ack {task, n}` | unread for you (long poll ≤120 s) · mark handled |
| `GET /v1/user/pending[?all=true]` · `POST /v1/user/decisions {item \| task, verbatim, form?}` | user items · record a decision |
| `POST /v1/user/asked {item}` · `POST /v1/presence {platform}` | you asked the user · your declared platform |

Errors: 400 invalid, 401 token, 403 role/not a party, 404 no task, 409 state/exists/conflict.

## Storage

`~/.local/state/agent-bridge/` (0700): one append-only JSONL per task (fsync before replying; a bad
line is skipped and counted, never fatal), read cursors, `decisions.jsonl`, `asked.jsonl`, `wakes.jsonl`. The
mirror under `D:\cc-tasks\claude-bridge\tasks\` is written by one background thread and is **not a
data source**: files there that the store does not have are reported in `/v1/health`, not adopted.
