# M1 verification, round 5: Codex's second REWORK

Same authorization ("可以交给dsh", M1 verification) and the same rules as task.md: absolute paths, raw output with exit codes, **stop at the first unexpected result**, never print anything from `/etc/review-gate/*`, `/var/lib/reviewgate/state`, the trace files, or **`/home/auditor/.codex/auth.json`** (the login: only `ls -l` and `sha256sum` are allowed on it).

**What changed in the runner**
- **Clean home per job.** Every job gets a fresh Codex home (`--run-home /home/auditor/codex-run`) holding only a copy of the login. So no AGENTS.md, skills or other context from `/home/auditor/.codex` reaches the model. A refreshed login is copied back, and the run home is deleted after the job.
- **Event validation.** The event stream must be whole (`thread.started` … `turn.completed`, every item completed); otherwise the job is `failed`.
- **New `cleanup` field.** It is `gone` only when the process group is confirmed gone (ESRCH). If it is not confirmed, the runner exits after replying, so systemd clears its cgroup.

| File | sha256 |
|---|---|
| `audit-runner` | `4336fa10f0b538df9ba63b909c8dec9123643406403531328d3001fa6c338f07` |
| `audit-runner.service` | `1419df8e186f067927b386b71e1451ba71ca6ea85762304ff6c896aa1e0747c9` |

## I1. Install
1. `wsl -u root -e /usr/bin/systemctl stop audit-runner.service`
2. `wsl -u root -e /usr/bin/install -m 0755 -o root -g root /mnt/e/rust-agent-browser/reviews/m1-verify/audit-runner /usr/local/lib/audit-runner/audit-runner`
3. `wsl -u root -e /usr/bin/install -m 0644 -o root -g root /mnt/e/rust-agent-browser/reviews/m1-verify/audit-runner.service /etc/systemd/system/audit-runner.service`
4. `wsl -u root -e /usr/bin/systemctl daemon-reload`
5. `wsl -u root -e /usr/bin/sha256sum /usr/local/lib/audit-runner/audit-runner /etc/systemd/system/audit-runner.service`
   - expected: the table above
6. Smoke test:
   - command: `wsl -u root -e /usr/bin/sudo -u reviewgate /usr/bin/python3 /mnt/e/rust-agent-browser/reviews/m1-verify/client.py /run/audit-runner/runner.sock i1-reviewgate hello`
   - expected: rejected with the cgroup reason

## I2. What context the model actually gets: deployed sandbox, real auditor home, no quota
A capture server stands in for the OpenAI API, and a wrapper redirects only the model provider; everything else is the runner's real invocation. The wrapper and server are in `cap/`.
1. Start the capture server:
   ```
   wsl -u root -e /usr/bin/systemd-run --unit=m1-capture /usr/bin/python3 /mnt/e/rust-agent-browser/reviews/m1-verify/cap/capture.py /mnt/e/rust-agent-browser/reviews/m1-verify/i2-cap 19077
   ```
2. Start the temporary runner (same sandbox as the service; real `/home/auditor/.codex`; codex = wrapper):
   ```
   wsl -u root -e /usr/bin/systemd-run --unit=audit-runner-m1cap -p User=auditor -p Group=auditor -p NoNewPrivileges=yes -p ProtectSystem=strict -p ProtectHome=tmpfs -p BindPaths=/home/auditor -p PrivateTmp=yes -p UMask=0077 -p RuntimeDirectory=audit-runner-m1cap -p RuntimeDirectoryMode=0755 /usr/local/lib/audit-runner/audit-runner --socket /run/audit-runner-m1cap/runner.sock --allow-uid 999 --allow-cgroup /system.slice/m1-cap.service --codex /mnt/e/rust-agent-browser/reviews/m1-verify/cap/cap-codex.sh --codex-home /home/auditor/.codex --run-home /home/auditor/codex-run-m1cap --workdir /home/auditor/jobs-m1cap --timeout-secs 120
   ```
3. `wsl -u root -e /bin/chmod 0666 /run/audit-runner-m1cap/runner.sock`
4. Submit from inside the allowed unit:
   ```
   wsl -u root -e /usr/bin/systemd-run --unit=m1-cap --wait --pipe --collect -p User=reviewgate -p Group=reviewgate /usr/bin/python3 /mnt/e/rust-agent-browser/reviews/m1-verify/client.py /run/audit-runner-m1cap/runner.sock i2-cap "Please run the shell command id."
   ```
   - expected: `completed`, `evidence_complete: true`, `cleanup: "gone"`
5. `wsl -u root -e /bin/ls -la /mnt/e/rust-agent-browser/reviews/m1-verify/i2-cap`
   - expected: one `req01-POST_v1_responses.json`
   - Claude analyses it; you don't need to.
6. `wsl -u root -e /bin/ls -la /home/auditor/codex-run-m1cap`
   - expected: **No such file** (the run home was deleted)
7. `wsl -u root -e /bin/ls -la /home/auditor/jobs-m1cap/i2-cap`
   - expected: no `auth.json` in the job record
8. Clean up:
   - `wsl -u root -e /usr/bin/systemctl stop audit-runner-m1cap m1-capture`
   - `wsl -u root -e /bin/rm -rf /home/auditor/jobs-m1cap`

## I3. One real Codex job through the clean home
This uses a little of the user's ChatGPT quota; one job only.
1. `wsl -u root -e /usr/bin/sha256sum /home/auditor/.codex/auth.json`
   - record the hash only
2. Start the temporary runner (real codex):
   ```
   wsl -u root -e /usr/bin/systemd-run --unit=audit-runner-m1real -p User=auditor -p Group=auditor -p NoNewPrivileges=yes -p ProtectSystem=strict -p ProtectHome=tmpfs -p BindPaths=/home/auditor -p PrivateTmp=yes -p UMask=0077 -p RuntimeDirectory=audit-runner-m1real -p RuntimeDirectoryMode=0755 /usr/local/lib/audit-runner/audit-runner --socket /run/audit-runner-m1real/runner.sock --allow-uid 999 --allow-cgroup /system.slice/m1-real.service --codex /usr/local/lib/auditor-codex/node_modules/.bin/codex --codex-home /home/auditor/.codex --run-home /home/auditor/codex-run-m1real --workdir /home/auditor/jobs-m1real --timeout-secs 900
   ```
3. `wsl -u root -e /bin/chmod 0666 /run/audit-runner-m1real/runner.sock`
4. Submit:
   ```
   wsl -u root -e /usr/bin/systemd-run --unit=m1-real --wait --pipe --collect -p User=reviewgate -p Group=reviewgate /usr/bin/python3 /mnt/e/rust-agent-browser/reviews/m1-verify/client.py /run/audit-runner-m1real/runner.sock i3-real "Reply with exactly one line: VERDICT: APPROVE (this is a connectivity test; do not use any tool)."
   ```
   - expected: `completed`, `evidence_complete: true`, `cleanup: "gone"`, `commands: []`, and an answer containing `VERDICT: APPROVE`
   - If it fails with a login or auth error, stop.
5. Before cleanup:
   - `wsl -u root -e /bin/cp /home/auditor/jobs-m1real/i3-real/events.jsonl /mnt/e/rust-agent-browser/reviews/m1-verify/i3-events.jsonl`
   - the same for `stderr.log` → `i3-stderr.log`
6. `wsl -u root -e /bin/ls -la /home/auditor/codex-run-m1real /home/auditor/jobs-m1real/i3-real`
   - expected: run home **gone**; no `auth.json` in the job record
7. `wsl -u root -e /usr/bin/sha256sum /home/auditor/.codex/auth.json`, and `/bin/ls -l` on it
   - record only; it may differ if the login was refreshed
8. Clean up:
   - `wsl -u root -e /usr/bin/systemctl stop audit-runner-m1real`
   - `wsl -u root -e /bin/rm -rf /home/auditor/jobs-m1real`

## I4. Restore
1. `wsl -u root -e /usr/bin/systemctl stop audit-runner.service`
2. `wsl -u root -e /usr/bin/systemctl is-active audit-runner.socket audit-runner.service audit-runner-m1cap audit-runner-m1real m1-capture`
   - expected: active / inactive ×4

Write the raw output to `dsh-raw-r5.txt` and post the result table.
