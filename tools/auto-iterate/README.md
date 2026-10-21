# Auto-iterate tooling

`SKILL.md` is the authoritative, vendor-neutral workflow (v2). The gate is still
`crates/review-gate`; this directory only holds the protocol text and the optional driver.

## Install (one source of truth)

Copy or link the same file into each host's skill directory; do not fork the text.

```bash
# Claude Code (this machine)
ln -sfn <repo>/tools/auto-iterate/SKILL.md ~/.claude/skills/auto-iterate/SKILL.md

# opencode / other skill-aware hosts: use their skill directory, pointing at the same file.
# Hosts without a skill mechanism: use the driver below or reference the file from AGENTS.md.
```

Check the installed version with `head -5 tools/auto-iterate/SKILL.md` (`version: 2`).

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

The agent sees `AUTO_ITERATE_REPO`, `AUTO_ITERATE_ITERATION` and
`AUTO_ITERATE_FEEDBACK` (previous review text, empty on round 1). Exit codes:
`0` done/finalized, `3` no external AI, `4` paused by the gate, `1` error.

## Run log, timeout and tests

`--agent-timeout S` bounds a hung agent command (0 disables it); the driver exits 1
and leaves the task running so the round can be retried.

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
