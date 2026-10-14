# M1 verification of audit-runner (plan §5, model A) — run by DSH

The user authorized this ("可以交给dsh"), including root via `wsl -u root`, **only for the commands listed here**.

Files are in `E:\rust-agent-browser\reviews\m1-verify\` (= `/mnt/e/rust-agent-browser/reviews/m1-verify/`).

**Rules**
- Use absolute paths everywhere (`wsl -u root -e` has no /usr/sbin in PATH).
- For every command, record the exact command, its full output, and its exit code.
- **Stop at the first unexpected result.** Don't try to fix it or work around it; report it.
- **Never print the contents** of `/etc/review-gate/*` or of anything under `/var/lib/reviewgate/state`. Only `test -r`, `ls -ld`, or an attempt whose expected result is "Permission denied" is allowed. If a read unexpectedly succeeds, the output goes to `/dev/null`. In that case report only "READABLE" and stop.
- Don't delete anything except what's named below: the stale runtime directory in A, and the temporary probe unit, socket and job directory in D.
- Write raw output to `/mnt/e/rust-agent-browser/reviews/m1-verify/dsh-raw.txt`, then post a result on this task.

## A. Reinstall the fixed socket unit (DirectoryMode 0750 → 0755)
1. `wsl -u root -e /usr/bin/install -m 0644 -o root -g root /mnt/e/rust-agent-browser/reviews/m1-verify/audit-runner.socket /etc/systemd/system/audit-runner.socket`
2. `wsl -u root -e /usr/bin/systemctl stop audit-runner.service audit-runner.socket`
3. `wsl -u root -e /bin/rm -f /run/audit-runner/runner.sock`, then `wsl -u root -e /bin/rmdir /run/audit-runner`. DirectoryMode only applies when systemd creates the directory.
4. `wsl -u root -e /usr/bin/systemctl daemon-reload`, then `wsl -u root -e /usr/bin/systemctl start audit-runner.socket`
5. `wsl -u root -e /bin/ls -ld /run/audit-runner /run/audit-runner/runner.sock`
   - expected: `drwxr-xr-x root root` and `srw-rw---- auditor reviewgate`
6. `wsl -u root -e /usr/bin/systemctl is-active audit-runner.socket audit-runner.service`
   - expected: `active` / `inactive`
7. `wsl -u root -e /usr/bin/sha256sum /etc/systemd/system/audit-runner.socket /mnt/e/rust-agent-browser/reviews/m1-verify/audit-runner.socket`
   - expected: identical

## B. auditor cannot read the gate's secrets
Run each as auditor and expect "Permission denied" or "not-readable":
- `wsl -u root -e /usr/bin/sudo -u auditor /bin/sh -c 'test -r /etc/review-gate/env && echo READABLE || echo not-readable'`
- the same with `/etc/review-gate/client.token`
- `wsl -u root -e /usr/bin/sudo -u auditor /bin/ls /var/lib/reviewgate/state`
  - expected: Permission denied
- `wsl -u root -e /usr/bin/sudo -u auditor /bin/sh -c 'test -r /var/lib/reviewgate/state/keys/gate.ed25519 && echo READABLE || echo not-readable'`
- `wsl -u root -e /usr/bin/id auditor`
  - expected: no reviewgate group

## C. Impostors are refused (real installed runner)
The client is `C=/mnt/e/rust-agent-browser/reviews/m1-verify/client.py`.
1. As auditor:
   - command: `wsl -u root -e /usr/bin/sudo -u auditor /usr/bin/python3 /mnt/e/rust-agent-browser/reviews/m1-verify/client.py /run/audit-runner/runner.sock c1-auditor hello`
   - expected: it connects, since auditor owns the socket, and the reply is `"status": "rejected"` with an error mentioning uid 997. A `"status": "completed"` reply is a failure.
2. As reviewgate:
   - command: `wsl -u root -e /usr/bin/sudo -u reviewgate /usr/bin/python3 /mnt/e/rust-agent-browser/reviews/m1-verify/client.py /run/audit-runner/runner.sock c2-reviewgate hello`
   - expected: `"status": "rejected"`, because the peer executable is python3, not /usr/local/bin/review-gate.
3. Afterwards:
   - `wsl -u root -e /bin/ls -ld /home/auditor/jobs`
     - expected: exists, owned by auditor, created by the socket-activated runner
   - `wsl -u root -e /bin/ls -la /home/auditor/jobs`
     - expected: **no** c1-auditor or c2-reviewgate job directories
   - `wsl -u root -e /usr/bin/journalctl -u audit-runner.service --no-pager -n 30`

## D. A real Codex job in the same sandbox, plus interop probes
This uses a little of the user's ChatGPT quota; one job only. It is a temporary runner with the same sandbox as the service, but without `--allow-exe`, so reviewgate's python3 client may submit.
1. Start it:
   ```
   wsl -u root -e /usr/bin/systemd-run --unit=audit-runner-m1probe -p User=auditor -p Group=auditor -p NoNewPrivileges=yes -p ProtectSystem=strict -p ProtectHome=tmpfs -p BindPaths=/home/auditor -p PrivateTmp=yes -p UMask=0077 -p RuntimeDirectory=audit-runner-m1probe -p RuntimeDirectoryMode=0755 /usr/local/lib/audit-runner/audit-runner --socket /run/audit-runner-m1probe/runner.sock --allow-uid 999 --codex /home/auditor/.local/node_modules/.bin/codex --codex-home /home/auditor/.codex --workdir /home/auditor/jobs-m1probe --timeout-secs 900
   ```
2. Open the socket: `wsl -u root -e /bin/chmod 0666 /run/audit-runner-m1probe/runner.sock`. The runner's peer check (uid 999) still applies.
3. Submit the job as reviewgate (wait up to 15 minutes):
   ```
   wsl -u root -e /usr/bin/sudo -u reviewgate /bin/sh -c '/usr/bin/python3 /mnt/e/rust-agent-browser/reviews/m1-verify/client.py /run/audit-runner-m1probe/runner.sock d1-probe "$(cat /mnt/e/rust-agent-browser/reviews/m1-verify/probe-prompt.txt)" 900'
   ```
   Record the full JSON reply: status, commands, item_types, and answer.
4. Check that the probe left no marker:
   - `wsl -u root -e /bin/ls -la /home/auditor/jobs-m1probe /home/auditor/jobs-m1probe/d1-probe`
     - expected: no `MARKER-should-not-exist`
   - `wsl -u root -e /usr/bin/grep -o '"type":"command_execution"[^}]*' /home/auditor/jobs-m1probe/d1-probe/events.jsonl`
     - if the file name differs, list the directory and grep the events file that is there
5. Run the same probes directly in an identical sandbox, without Codex (this is the boundary itself, independent of Codex's own sandbox):
   ```
   wsl -u root -e /usr/bin/systemd-run --wait --pipe --collect -p User=auditor -p Group=auditor -p NoNewPrivileges=yes -p ProtectSystem=strict -p ProtectHome=tmpfs -p BindPaths=/home/auditor -p PrivateTmp=yes /bin/sh -c 'id; ls /mnt/c >/dev/null && echo mnt-c-readable || echo mnt-c-not-readable; /mnt/c/Windows/System32/cmd.exe /c ver; echo cmd-exit=$?; /mnt/c/Windows/System32/wsl.exe -u root -e /usr/bin/id; echo wsl-exit=$?; test -r /etc/review-gate/env && echo READABLE || echo env-not-readable; touch /usr/local/lib/audit-runner/x; echo touch-exit=$?'
   ```
   Whatever comes out is the result; it isn't an error. In particular, **if `wsl.exe -u root -e /usr/bin/id` prints uid=0 from inside the sandbox, record it plainly.** That is the known model-A limit, not a reason to stop.
6. Clean up:
   - `wsl -u root -e /usr/bin/systemctl stop audit-runner-m1probe`
   - `wsl -u root -e /bin/rm -rf /home/auditor/jobs-m1probe` (only after step 4 is recorded)

## E. Not replaceable by the gate or auditor through normal paths
- `wsl -u root -e /usr/bin/sudo -u auditor /bin/sh -c 'for p in /usr/local/lib/audit-runner/audit-runner /usr/local/lib/audit-runner /etc/systemd/system/audit-runner.service /etc/systemd/system/audit-runner.socket /etc/systemd/system /usr/local/bin/review-gate; do test -w $p && echo "WRITABLE $p" || echo "not-writable $p"; done'`
- the same loop with `sudo -u reviewgate`
- `wsl -u root -e /bin/ls -l /home/auditor/.local/node_modules/.bin/codex /home/auditor/.local/node_modules/@openai/codex/package.json`
  - This is a note, not a failure: the Codex CLI is auditor-owned, so auditor can replace its own Codex. I record that as a model-A limit.
- `wsl -u root -e /usr/bin/sha256sum /usr/local/lib/audit-runner/audit-runner`
  - expected: starts with f49bcb3cda797db8a03e

Post the result with a short table: step, expected, actual, pass/fail. Attach the raw output path. Thanks!
