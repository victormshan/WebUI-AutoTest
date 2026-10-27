# review-gate

The independent review gate for **auto-iterate**, aligned with the dsh-web-relay three-party
protocol: Claude implements, a model from **another vendor** reviews every version, and the user
gives final acceptance. The gate, not the implementer, decides whether a version may be committed.

```
 Claude Code (implementer, user account)            reviewgate (system user)
 ─────────────────────────────────────────          ─────────────────────────────────────────
 edit code                                          review-gate serve  (127.0.0.1:7878)
 review-gate review run  ── tree+base ─────────▶    diff base..tree (read-only)
   (or MCP gate_review_start)                       ask external AI: gemini-api | openai-compatible
                                                      | web-gemini (dsh-web-gemini-ext bridge)
                         ◀──── review record ─────  store record (private, single-use)
 git commit; git tag
 review-gate record      ── review id, commit, tag ▶ check tree / parent / tag / strength / streak
                         ◀── action + attestation ─  sign ed25519 attestation
 git note refs/notes/review-gate                    write <exprId>.gate.md trace
 git push (+ notes)  ───────────────▶  CI: review-gate verify-range with the pinned public key
```

## What the gate enforces

| Rule | Where |
|---|---|
| Verdicts only come from the gate's own reviewer calls (or a human via `admin manual`) | service; implementer has no write access to the store |
| A review covers exactly one staged tree on top of HEAD (`base`) | `review run` stages; the service diffs `base..tree` |
| Approved commit: tree == reviewed tree, exactly one parent == base, tag points at it | `record` |
| Reviewer strength ≥ task threshold (`manual 5 > external-api 4 > web-gemini 3 > claude-subagent 2 > self-review 1`, default web-gemini); weaker → pause for a human, never a silent downgrade | `record` |
| No external AI available → the review job reports `unavailable` (exit 3); no fallback to Claude | `review run` |
| 3 rejections in a row → circuit breaker, task paused | `record` |
| Every approved commit carries a signed attestation; CI verifies it with a pinned key | `record` → git note → `verify-range` |
| Only the gate writes "外部审核 / review-gate" trace entries | gate-owned `/var/lib/reviewgate/relay/traces/<exprId>.gate.md` (implementer can read, not write; other users cannot read); claude-step-relay merges it into the trace (`REVIEW_GATE_TRACE_DIR`) and rejects the reserved roles from anyone else |

The guarantees are as strong as the separation between the two system users: the implementer
must not have sudo to `reviewgate`, must not be in its group, and must not be able to edit the CI
workflow (or the branch protection that requires it).

## Identities and implementer neutrality

The service authenticates each request as a stable **principal** with a canonical model
**family**. Creating a task freezes both: later reads, background review jobs, submissions,
`record` and `check` are confined to that principal, and an external review must come from a
different family than the frozen implementer family.

Administrators configure this in a read-only file (`serve --identity-config`, default
`/etc/review-gate/identity.json` when it exists). Only **token hashes** are stored:

```json
{
  "version": 1,
  "principals": [
    { "id": "claude",   "family": "claude",   "credentials": [
        { "id": "claude-main",   "sha256": "<hex sha256 of the token>" } ] },
    { "id": "opencode", "family": "deepseek", "credentials": [
        { "id": "opencode-main", "sha256": "<hex sha256 of the token>" } ] },
    { "id": "codex",    "family": "openai",   "credentials": [
        { "id": "codex-main",    "sha256": "<hex sha256 of the token>" } ] }
  ]
}
```

- A token maps to exactly one principal. Rotating a token keeps the principal; removing an entry
  or `"enabled": false` revokes it.
- Unknown families, duplicate principals/credential ids/token hashes, unknown fields and a wrong
  `version` are configuration errors: the service refuses to start. There is **no fallback** to
  the legacy token when a configuration file is present but invalid — or when the default path
  exists but cannot be inspected (for example a permission error on `/etc/review-gate/identity.json`).
- A principal with `"admin": true` may list and read every task over HTTP, but it has **no write
  exemption**: it changes only the tasks it owns, and never milestones unless it is a listed
  writer. Human `manual` reviews are accepted only by the local `admin manual` command, never
  over HTTP.
- Without any configuration file, `/etc/review-gate/client.token` remains an explicit legacy
  compatibility entry: principal `legacy-claude`, family `claude`. That file is still plaintext;
  "only hashes" describes the new administrator mapping.
- Tasks written by older versions have no principal and therefore **no known owner** (see
  Ownership below); they do not belong to `legacy-claude`.
- The family of an OpenAI-compatible endpoint is **not guessed** from its URL or model name. Set
  `REVIEW_GATE_OPENAI_FAMILY` (`openai` | `deepseek` | `gemini` | `claude`); official
  `api.openai.com` / `api.deepseek.com` endpoints have those defaults. An endpoint with no
  configured family cannot act as an independent external reviewer.
- `external-api` and `web-gemini` require a different, known family; `claude-subagent` (2) and
  `self-review` (1) stay in the implementer family; `manual` is administrator-only. The rule is
  applied when choosing a provider, before each retry, for cached and every chunk answer, and
  again in `record`/`check`.
- A task's `POST /tasks` may include `implementer_family`, but it is only checked against the
  authenticated identity and refused on mismatch; it never overrides the frozen family.

### Ownership, the 2026-10-22 agents file, milestone writers (M2 step 8 integration)

- **Owner.** A task created by a principal belongs to it. A task with no principal (created by a
  version without principals) has **no known owner**: only the administrator can read it, every
  write is refused, and so is every `/check` on its repository. Nothing assigns one automatically.
  `review-gate admin task --state … unowned` lists them; `admin task --state … assign --id T
  --principal P --reason R --expect-updated-at <as listed> [--yes]` gives one its first owner
  (dry run without `--yes`; once only; with `--yes` it is refused while a service holds
  `state/service.lock`, drained or not: stop the service first). The owner and its audit
  (`owner_assigned`) are one write of the task file.
- **`/check`** refuses a commit when any running task of the repository is not the caller's,
  even if the caller also runs one there.
- **`--agents-file`** (the token → family file deployed on 2026-10-22) is still accepted on the
  command line, but it creates no identity: it needs `--identity-config`, and every entry must
  be a token that configuration maps, to an enabled principal of the same family. Duplicates, the
  legacy client token, unknown families and anything not in the configuration stop the start.
  Only `--agents-file` without `--identity-config` also stops the start: an operational change.
- **Administrator files** (`identity.json`, the agents file) are opened without following a
  symlink and checked on the same open file: regular, owned by root or the service's user, no
  write for group or others, nothing for others (0600 or 0640), in a directory others cannot
  change. Errors never echo content.
- **Migration.** `review-gate admin identity-from-agents --credential P=FAMILY=TOKENFILE …
  [--legacy client.token] [--agents agents.json] --out identity.json` writes the configuration
  (hashes of the token as a client sends it, i.e. trimmed; legacy as the explicit
  `legacy-claude` entry), checks it against the agents file and prints no token.
- **Milestone writers.** `milestones.json` must list `writers`: gate principals allowed to
  apply, request, cancel and report published over HTTP (others get 403, before any effect;
  no administrator exemption). They must be enabled principals of the identity configuration.
  `implementers` stays the set of bridge agent names used by the sources check.
- **Rollback.** Once this version has written any task, do not start a binary without
  principals on that state: it ignores `principal` and drops it when it writes the task.

## Install (once, as root)

```bash
cargo build --release -p review-gate
sudo bash crates/review-gate/deploy/install.sh --implementer "$USER"
sudoedit /etc/review-gate/env          # reviewer API keys / web-gemini bridge address
sudo systemctl restart review-gate
review-gate pubkey                      # pin this in CI
```

`install.sh` creates the `reviewgate` user, `/var/lib/reviewgate/{state (0700), relay/traces (0750, group = the implementer's)}`,
the client token `/etc/review-gate/client.token` (gate + implementer may read it, neither may
write it), the reviewer env file (gate only) and the systemd unit `review-gate.service`.
The gate reads the implementer's repositories, so they must be world-readable (`chmod -R o+rX`).

## Using it

CLI (implementer side; token from `REVIEW_GATE_TOKEN[_FILE]`, `/etc/review-gate/client.token`
or `~/.config/review-gate/token`):

```bash
review-gate task init --id my-task --goal "…" --acceptance "…" --iterations 3 --repo .
review-gate task link --id my-task --expr-id <claude-step-relay exprId>
# per version:
review-gate review run --id my-task --evidence test-output.txt   # 0 reviewed · 3 no external AI · 1 failed
git commit -m "…" && git tag auto-iterate/my-task/v1             # only when approved
review-gate record --id my-task --review v1-1 --commit HEAD --tag auto-iterate/my-task/v1
git push origin HEAD refs/notes/review-gate
```

MCP (Claude Code), `.mcp.json`:

```json
{ "mcpServers": { "review-gate": { "command": "review-gate", "args": ["mcp"] } } }
```

Tools: `gate_task_list/show/init/link`, `gate_review_start` + `gate_job`, `gate_review_submit`,
`gate_record`, `gate_pubkey`.

Hook (Claude Code, project `.claude/settings.json`; sample in `deploy/`) — blocks `git commit` without an approved
review of the staged tree and `git push` of unattested commits, in repositories with a task. It
fails closed when the service is unreachable, so enable it per project rather than globally:

```json
{ "hooks": { "PreToolUse": [ { "matcher": "Bash",
    "hooks": [ { "type": "command", "command": "review-gate hook" } ] } ] } }
```

Asking an external AI directly (e.g. webtest's cross-review): `review-gate ask` (prompt on stdin).

Human verdict (the only way to use the `manual` channel), as the reviewgate user:

```bash
review-gate stage --id my-task                          # implementer prints {tree, base}
sudo -u reviewgate review-gate admin manual --id my-task --tree … --base … --file verdict.txt \
     --relay-dir /var/lib/reviewgate/relay
```

## CI

`ci/review-gate.yml` is a workflow template: it builds the verifier from a **pinned** commit (never
from the commits under test), fetches `refs/notes/review-gate` and runs
`review-gate verify-range --pubkey <pinned> <since>..<head>`. A human copies it to
`.github/workflows/`.

## HTTP API (for DeepSeek harness and other clients)

All routes except `/health` and `/pubkey` need `Authorization: Bearer <token>`; the token is
resolved to a principal and family (unknown/disabled/foreign-task requests get `401`/`403`).

| Method & path | Body | Result |
|---|---|---|
| `GET /health` | | `{ok, service, version}` |
| `GET /pubkey` | | `{algorithm: "ed25519", key}` |
| `GET /tasks` · `GET /tasks/{id}` | | tasks / task |
| `POST /tasks` | `{id, goal, acceptance, iterations, repo, min_reviewer?, review_provider?, implementer_family?}` | task |
| `POST /tasks/{id}/link` | `{expr_id}` | task |
| `POST /tasks/{id}/reviews` | `{tree, base, evidence?, provider?}` | job `{id, status: running}` |
| `GET /jobs/{job}` | | `{status: running\|done\|failed, record?, error?, unavailable}` |
| `POST /tasks/{id}/reviews/submit` | `{tree, base, channel: claude-subagent\|self-review, text}` | record |
| `POST /tasks/{id}/record` | `{review, commit?, tag?}` | `{action: start_round\|retry_same_round\|finalize\|pause, task, attestation?}` |
| `POST /check` | `{repo, tree, base}` | `{allowed, task?, reason}` |

`tree`/`base` come from `git add -A && git write-tree` and `git rev-parse HEAD` in the repository.
