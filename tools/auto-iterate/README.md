# Auto-iterate tooling

`SKILL.md` is the authoritative, vendor-neutral workflow (v3). The gate is still
`crates/review-gate`; this directory only holds the protocol text and the optional driver.

## Install (one source of truth)

Link the same files into each host; do not fork the text.

```bash
tools/auto-iterate/install.sh \
  --skill-dir ~/.claude/skills/auto-iterate \
  --bin-dir ~/.local/bin

tools/auto-iterate/install.sh --skill-dir ~/.claude/skills/auto-iterate --check
```

`--check` only verifies existing links; `--dry-run` prints the plan; `--force`
replaces a regular file or a different symlink. `--upgrade` migrates a symlink
from another `tools/auto-iterate/` checkout but never touches foreign files.
`--uninstall` removes only links that point at this checkout and leaves regular
files or other installations alone. Run `tools/auto-iterate/test-install.sh` to
test all lifecycle paths in a temporary directory.

Manual equivalent:

```bash
ln -sfn <repo>/tools/auto-iterate/SKILL.md ~/.claude/skills/auto-iterate/SKILL.md
ln -sfn <repo>/tools/auto-iterate/auto-iterate.sh ~/.local/bin/auto-iterate.sh
```

opencode / other skill-aware hosts use their own skill directory, pointing at the
same file. Hosts without a skill mechanism use the driver below or reference the
file from AGENTS.md. Check the installed version with
`head -5 tools/auto-iterate/SKILL.md` (`version: 3`).

## Driver (L2)

`auto-iterate.sh` loops: read the task -> run one agent round -> stage -> review ->
commit/tag/record -> next version. It never writes a verdict.

```bash
export REVIEW_GATE_URL=http://127.0.0.1:7878
export REVIEW_GATE_TOKEN_FILE=~/.config/review-gate/token

tools/auto-iterate/auto-iterate.sh \
  --task my-task \
  --agent-cmd 'opencode run "implement the next version; run the tests"' \
  --message 'auto-iterate: implement the next version' \
  --evidence /tmp/evidence.txt
```

The agent sees `AUTO_ITERATE_REPO`, `AUTO_ITERATE_ITERATION`,
`AUTO_ITERATE_FEEDBACK` (previous review text; cleared after an approved version),
`AUTO_ITERATE_GOAL`, `AUTO_ITERATE_ACCEPTANCE`, `AUTO_ITERATE_TASK`,
`AUTO_ITERATE_EVIDENCE` (an absolute `--evidence` path; when `--version-plan` or
`--verify-cmd` is used without `--evidence`, a temporary evidence file is created
automatically) and
`AUTO_ITERATE_VERSION_ACCEPTANCE` (the per-version plan entry, empty if none).
Exit codes: `0` done/finalized, `2` lock held or pending state, `3` no external AI,
`4` paused by the gate, `5` stopped on a rejection, `1` error.

`--plan` (alias `--dry-run`) validates the same prerequisites as `--check` and
prints the next round without writing state, logs, manifests or commits. A live
run also writes `<gitdir>/auto-iterate/status/<task>.json` atomically on every
phase change (task, phase, iteration, timestamps, exit code, review, commit, tag).
The file is telemetry, not recovery state; the durable recovery file remains
`<gitdir>/auto-iterate/<task>.json`.

CI runs the four test scripts via `.github/workflows/auto-iterate-tests.yml`.

## Manifest integrity

The approved-version manifest is append-only JSONL. `manifest-tool.sh` inspects and
maintains it without touching the gate:

```bash
tools/auto-iterate/manifest-tool.sh validate <gitdir>/auto-iterate/versions.ndjson
tools/auto-iterate/manifest-tool.sh repair   <gitdir>/auto-iterate/versions.ndjson
tools/auto-iterate/manifest-tool.sh export   <gitdir>/auto-iterate/versions.ndjson csv
```

`validate` checks required fields, full commit ids, `iteration == version` and
duplicate `task/version` pairs. `repair` atomically keeps valid lines and writes
bad or duplicate lines to `versions.ndjson.corrupt.<timestamp>.<pid>`. `export`
refuses a corrupt manifest. The driver lock is repo-wide; a second driver targeting
the same worktree exits `2` before changing state, and this overlap is covered by
`test-driver.sh`.

## Codex adapter

`codex-agent.sh` is a thin host adapter; the driver and the gate are unchanged.
Use the native Linux CLI for repositories on the WSL filesystem:

```bash
npm i -g @openai/codex
codex login status            # or: codex login
tools/auto-iterate/auto-iterate.sh --task <task-id> \
  --agent-cmd tools/auto-iterate/codex-agent.sh \
  --evidence /tmp/codex-evidence.txt
```

`CODEX_BIN` overrides the binary. Without it, a repository under `/mnt/<drive>/`
prefers the Windows CLI (`CODEX_WIN_BIN` overrides its path); a WSL repository
prefers `codex` on PATH, then `~/.npm-global/bin/codex`, then the Windows CLI.
`CODEX_FLAGS` defaults to `--sandbox workspace-write --ephemeral --color never`;
Codex 0.162/0.163 does not accept `--full-auto`, so do not copy older examples.
`CODEX_MODEL` and `CODEX_PROFILE` are optional.

The adapter builds the version prompt from the task, goal, acceptance, per-version
acceptance and previous review; it receives the prompt on stdin and appends
stdout/stderr to the driver's `--evidence` path. It refuses to run when the evidence path is inside the repo, when
Codex changed `HEAD`, changed a tag name or tag object id, made no change to the
worktree/index/untracked-file snapshot (including the index tree and NUL-delimited
untracked names), or when the binary is a Windows `.exe` and
the repo is not under `/mnt/<drive>/`. The snapshot comparison also catches a no-op
when the worktree was already dirty. It also unsets
every exported `REVIEW_GATE_*` variable before starting Codex. The driver still stages, reviews, commits,
tags and records; the adapter never does.

For a Windows Codex CLI, use only a Windows-drive worktree (for example `/mnt/e/...`);
it cannot safely edit a WSL-only repository. Its PowerShell sandbox may fail and fall
back to `cmd.exe`, so native Linux Codex is more reliable for test-heavy versions.

`E:\Tools\npm` is a Windows npm prefix. Its `codex` and `codex.cmd` shims are not
usable directly from WSL: the Unix shim needs the missing `@openai/codex-linux-x64`
optional package, and the `.cmd` shim needs `cmd.exe`. Use the native Linux CLI, or
point `CODEX_BIN` at the actual `...\@openai\codex-win32-x64\...\codex.exe` for a
Windows-drive worktree.

Run the adapter test without calling a model:

```bash
tools/auto-iterate/test-codex-agent.sh
```

## Run log, timeout and tests

`--agent-timeout S` bounds a hung agent command (0 disables it); the driver exits 1
and leaves the task running so the round can be retried.

`--report FILE` writes a Markdown summary incrementally: task, repo, one row per
reviewed round (version, review, verdict, action, commit, tag, reviewer channel),
duration of each round, finish time and exit
code. A crash still leaves a usable file.

`--events FILE` appends one JSON object per line (`ts`, `event`, plus event fields)
for machine consumption: start, round, review, approved, rejected, record, stop.

`--notify-events LIST` restricts the exit hook to terminal events (`finalized`,
`paused`, `failed`, `stopped`, `done`, `blocked`). `--notify-retries N` retries a
non-zero hook, and `--notify-retry-delay S` sets the base for exponential backoff
(1s, 2s, 4s, ... capped at 30s). Notification failure never changes the driver exit
status.

`--review-timeout S` bounds the `review run` client (0 disables it). On timeout the
driver exits 3 and leaves the task and staged tree intact; the service may still
finish the review in the background.

Each round's elapsed seconds are logged, emitted as `round_done` and added to the
report row.

`--provider NAME` is forwarded to `review run`, selecting the reviewer provider.
The verdict log line and the `review` event also carry the channel and provider the
service used, so the report records which reviewer decided each version.

Every approved version is appended to `auto-iterate/versions.ndjson` in the worktree's
git dir by default; `--manifest FILE` selects another absolute or relative path.
The file is rewritten atomically and is idempotent by task and iteration, so a crash
after `record` can be resumed without duplicating an entry. Each line records the
task, version, review id, commit, tag, channel, provider, message, tree, per-version
acceptance and verify command. Rejected versions are not added. The selected path is
stored in the round state/retry metadata and restored by `--resume`; a different
`--manifest` on resume is rejected before commit or record.

`--notify-cmd CMD` runs once when the driver exits, with `AUTO_ITERATE_EVENT`
(`finalized`, `paused`, `failed`, `stopped`, `done` or `blocked`), the exit code,
reason, task, repo, iteration, commit, tag, review, verdict, channel and message.
It is best-effort: a notification failure is reported but does not change the
driver's exit status. `--notify-timeout S` bounds the notification command; exit 124
is reported as a notification timeout. `--verify-range` runs
`review-gate verify-range` after a finalize record, using the gate pubkey and the
range starting at the first approved task commit (or the repository root when task
history is empty). `--verify-range-timeout S` bounds that call. A failed or timed-out
range verification exits 1 after the approved commit/tag are already recorded.

The agent also receives `AUTO_ITERATE_GOAL`, `AUTO_ITERATE_ACCEPTANCE` and
`AUTO_ITERATE_TASK`. The driver refuses to start on a dirty worktree unless
`--allow-dirty` is given, so pre-existing edits cannot silently enter version 1.

### Concurrency, recovery and acceptance

The driver holds a kernel `flock` on `auto-iterate/lock` in the worktree's git dir;
`auto-iterate/lock.meta` records the owner, host, PID, task and timestamps for
diagnostics. A second driver fails fast with exit 2; use `--lock-timeout S` to wait.
The kernel releases the lock when the driver exits, so a crashed run never leaves a
stale lease. Child agent, verify, review and fallback processes get FD 9 closed so
they cannot keep the repository lease alive. A separate inherited FD 8 holds
`auto-iterate/worker.lock` while an agent or verifier runs; the parent acquires it
before forking, so the interval before `worker.pid` appears is covered. If the
driver dies, `--resume` sees the still-held worker lease and exits 2 until that
worker (and any descendant that inherited FD 8) exits. After a wait, the parent
closes FD 8 and tries to take the lease again; if a background descendant still
holds it, the driver stops before staging or committing instead of racing it. The
same lease covers review and fallback workers, so a driver killed during review
cannot be resumed until that review process has exited. The metadata is refreshed at each round and removed on release.

Each round writes `agent -> staged -> reviewing -> reviewed -> committed` (or
`rejected`) to `auto-iterate/<task>.json`. `--resume` also handles an interrupted
`agent` phase, preserving partial edits and saved feedback before restarting, a
`verify`/`staged`/`reviewing` tree that can be verified and staged again without
discarding the edits, and a `rejected` record that still needs `record`/reset.
The state carries the absolute evidence path, verification command and timeout, the
absolute version-plan path, the selected acceptance, and the intended tag. A resume without those options
restores them, while adding or changing an evidence path, command, timeout, plan or
acceptance is rejected before any recovery commit, tag or record. A custom tag is
therefore reused even if the resume uses a different `--tag-prefix`. Feedback is carried through
`agent`, `verify`, `staged` and `reviewing` writes. After approval the durable
feedback is removed before the state file, so a crash between those operations
cannot leak rejection text into the next version. A
successful `record` removes the state file. Before replaying a pending
`committed`/`rejected` record, the driver checks the task history and skips the
record when that review id is already present; an already recorded `rejected` or
`insufficient-reviewer` outcome is completed without replay, including when an
approved weak-channel review paused after its commit, and the durable feedback is
preserved. The rejected state is the authoritative checkpoint: it is written before
the feedback/retry files, `record`, and index cleanup; recovery re-derives those files
from the state before recording. Reviewed recovery persists `committed`
before calling `record`, so a crash during that call is replayable, and records the
approved tree ID: it refuses to commit a changed index or a child commit with a
different tree. Recovery checks every state/feedback/retry persistence operation and
stops before recording or cleanup if one fails. Appending per-version acceptance or
verification output to evidence is fail-closed: the round stops rather than letting a
reviewer approve without the required evidence. Rejected recovery allows the recovered tracked edits through the
dirty-worktree guard, resets the index, cleans only the agent-created untracked paths
recorded when the round was rejected, merges saved protections with untracked or newly
staged files that already existed when an interrupted agent,
verifier/staged/reviewing round, or rejected-round retry was resumed, and leaves files
a user created later untouched, then removes the state file; the
feedback is kept in `auto-iterate/<task>.feedback` and retry metadata in
`auto-iterate/<task>.retry`, so a paused or stopped task cannot leave a pending JSON
that blocks other tasks; `--resume` uses the metadata to preserve the cleanup policy,
the verify command/timeout, version plan/acceptance and evidence path, and allow the
retained tracked edits. Agent, verify and stage failures also keep the
round state and its feedback text, so a later `--resume` can retry instead of
losing the rejection context. A rejection is retried on the same version; only an
approval advances the iteration. The driver reloads
the task and the durable feedback after acquiring the lock, clearing the temporary
feedback when the durable file is absent, so an iteration advanced by another
driver while it waited cannot inherit stale review text. A pending state belonging to any other task in the same git
dir blocks startup instead of letting a second task stage the first task's tree. After an approved version the feedback
file is cleared, so the next version never inherits stale review text. After a
rejected version the driver unstages the tree and, when the run started clean,
removes the untracked files the agent created; tracked edits are kept for the fix,
and the rejection text is also written to `auto-iterate/<task>.feedback`. That file
is cleared after approval.

`--verify-cmd CMD` runs in the repository after the agent and before staging. A
resumed `staged`/`reviewing` tree also reruns verification before it is staged again,
so edits made after the interruption cannot bypass it. A non-zero exit stops the
round with exit 1, keeps the changes for inspection, and leaves the task running;
`--verify-timeout S` bounds it. Verify output is appended
to `--evidence` so the reviewer sees it. This is an independent check in addition
to the reviewer, not a replacement for it.

`--version-plan FILE` accepts a JSON object (`{"1": "acceptance ...", "2": ...}`)
or an array of `{"version": N, "acceptance": "..."}` entries. The selected text is
exported as `AUTO_ITERATE_VERSION_ACCEPTANCE` and appended to the evidence file for
the reviewer. `AUTO_ITERATE_ACCEPTANCE` remains the task's final acceptance.

`--fallback-cmd CMD` is an explicit opt-in for the case where `review run` exits 3
(no external AI). It runs a fresh reviewer command in the repository with
`AUTO_ITERATE_DIFF`, `AUTO_ITERATE_REVIEW_FILE`, goal, acceptance, evidence and task
in the environment. The command must write a review containing a
`VERDICT: APPROVED|REJECTED` line and must not modify the repository; the driver
submits it with `review submit --channel` (`claude-subagent` by default, or
`self-review`). The gate still applies its reviewer-strength gate, so this never
silently downgrades a task. Without the flag, exit 3 is unchanged.

`--stop-on-reject` stops as soon as a review is not approved (exit 5), leaving the
task running so a human can inspect the feedback instead of burning more rounds.

`--check` validates the token/task/repo and the log/report/events paths, prints one
`check ok:` line and exits without running the agent or changing the repository. It
does not require `--agent-cmd` and does not truncate existing output files.

`--log FILE` appends one tab-separated line per event (`start`, `round`, `review`,
`approved`, `record`, `stop`). `test-driver.sh` runs the driver against a stub
`review-gate` in a temporary repo; it needs only `git`, `python3` and `sh`:

```bash
tools/auto-iterate/test-driver.sh
```

## Notes

- Identity comes from the gate token, never from `--implementer-family`; see
  `crates/review-gate/README.md`.
- The installed git hook (`review-gate guard --install`) is a convenience, not the
  security boundary; the gate record and CI `verify-range` are.
- Reviewer substitution is only allowed with explicit user authorization and must be
  recorded; see the SKILL's last sections.
