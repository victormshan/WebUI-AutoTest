# M1 verification, round 4: Codex REWORK fixes (user decisions included)

**Authorization**
- The user authorized M1 verification for DSH ("可以交给dsh").
- On Codex's REWORK the user decided two more root changes, which are part of this round:
  1. install the Codex CLI root-owned in `/usr/local/lib/auditor-codex`;
  2. make the gate's trace dir unreadable for auditor.

Rules are the same as task.md: absolute paths, raw output with exit codes, **stop at the first unexpected result**, and never print anything from `/etc/review-gate/*`, `/var/lib/reviewgate/state` or the trace files.

**What changed in the runner**
- Codex now runs with every tool that can execute disabled: no shell, unified exec, code mode, sub-agents, hooks, plugins, apps, browser or computer use, or web search.
- It ignores auditor's config.toml and rules.
- Events and stderr are written to files as they come, so a timeout keeps the record.
- Failing to set close-on-exec on the activated socket is now fatal.

| File | sha256 |
|---|---|
| `audit-runner` | `1f2060174fa27b40728a0edbe55c19cfd9dc379fc0271196963ddcce11c0e378` |
| `audit-runner.service` | `adb56bb4ac95f50b4e26040c7236ef6487757b532a025a48c161d2f506ec5d89` (now uses `--codex /usr/local/lib/auditor-codex/node_modules/.bin/codex`) |
| `fd-codex.sh` | `d56b86ca78a61ce5ccdcf37375989dd421076310815dde09ee2ecc88fefe9917` |

## H1. Codex CLI, root-owned
1. `wsl -u root -e /usr/bin/npm install --prefix /usr/local/lib/auditor-codex --ignore-scripts @openai/codex@0.162.1`
2. `wsl -u root -e /bin/ls -l /usr/local/lib/auditor-codex/node_modules/.bin/codex`, and `/bin/ls -ld /usr/local/lib/auditor-codex /usr/local/lib/auditor-codex/node_modules`
   - expected: root-owned
3. `wsl -u root -e /usr/bin/sudo -u auditor /usr/bin/find /usr/local/lib/auditor-codex -writable`
   - expected: **no output**
4. `wsl -u root -e /usr/bin/sudo -u auditor /usr/bin/env HOME=/home/auditor CODEX_HOME=/home/auditor/.codex /usr/local/lib/auditor-codex/node_modules/.bin/codex --version`
   - expected: `codex-cli 0.162.1`
5. The same with `login status` instead of `--version`.
   - expected: `Logged in using ChatGPT`
   - If it is not logged in, stop: the user must log in.

## H2. Runner binary and unit
1. `wsl -u root -e /usr/bin/systemctl stop audit-runner.service`
2. `wsl -u root -e /usr/bin/install -m 0755 -o root -g root /mnt/e/rust-agent-browser/reviews/m1-verify/audit-runner /usr/local/lib/audit-runner/audit-runner`
3. `wsl -u root -e /usr/bin/install -m 0644 -o root -g root /mnt/e/rust-agent-browser/reviews/m1-verify/audit-runner.service /etc/systemd/system/audit-runner.service`
4. `wsl -u root -e /usr/bin/systemctl daemon-reload`
5. `wsl -u root -e /usr/bin/sha256sum /usr/local/lib/audit-runner/audit-runner /etc/systemd/system/audit-runner.service`
   - expected: the hashes in the table above
6. Smoke test:
   - command: `wsl -u root -e /usr/bin/sudo -u reviewgate /usr/bin/python3 /mnt/e/rust-agent-browser/reviews/m1-verify/client.py /run/audit-runner/runner.sock h2-reviewgate hello`
   - expected: rejected, `peer cgroup /non-systemd is not /system.slice/review-gate.service`

## H3. Gate trace dir: auditor locked out
1. `wsl -u root -e /bin/chgrp administrator /var/lib/reviewgate/relay/traces`
2. `wsl -u root -e /bin/chmod 0750 /var/lib/reviewgate/relay/traces`
3. `wsl -u root -e /bin/ls -ld /var/lib/reviewgate/relay/traces`
   - expected: `drwxr-x--- reviewgate administrator`
4. `wsl -u root -e /usr/bin/sudo -u auditor /bin/sh -c 'test -r /var/lib/reviewgate/relay/traces/2026-10-05_10-02-40.gate.md && echo READABLE || echo not-readable'`
   - expected: **not-readable**
5. `wsl -u root -e /usr/bin/sudo -u auditor /bin/ls /var/lib/reviewgate/relay/traces`
   - expected: Permission denied

## H4. A real Codex job with the tools disabled
This uses a little of the user's ChatGPT quota; one job only. The setup is the same as D, but with the new binary and the root-owned Codex.
1. Start the temporary runner:
   ```
   wsl -u root -e /usr/bin/systemd-run --unit=audit-runner-m1noexec -p User=auditor -p Group=auditor -p NoNewPrivileges=yes -p ProtectSystem=strict -p ProtectHome=tmpfs -p BindPaths=/home/auditor -p PrivateTmp=yes -p UMask=0077 -p RuntimeDirectory=audit-runner-m1noexec -p RuntimeDirectoryMode=0755 /usr/local/lib/audit-runner/audit-runner --socket /run/audit-runner-m1noexec/runner.sock --allow-uid 999 --codex /usr/local/lib/auditor-codex/node_modules/.bin/codex --codex-home /home/auditor/.codex --workdir /home/auditor/jobs-m1noexec --timeout-secs 900
   ```
2. `wsl -u root -e /bin/chmod 0666 /run/audit-runner-m1noexec/runner.sock`
3. Submit the job:
   ```
   wsl -u root -e /usr/bin/sudo -u reviewgate /bin/sh -c '/usr/bin/python3 /mnt/e/rust-agent-browser/reviews/m1-verify/client.py /run/audit-runner-m1noexec/runner.sock h4-noexec "$(cat /mnt/e/rust-agent-browser/reviews/m1-verify/probe-prompt.txt)" 900'
   ```
   - Record the full JSON.
   - expected: **`commands` is empty and `item_types` has no `command_execution`**.
   - `status` may be `completed`.
4. **Keep the record before cleanup:**
   - `wsl -u root -e /bin/cp /home/auditor/jobs-m1noexec/h4-noexec/events.jsonl /mnt/e/rust-agent-browser/reviews/m1-verify/h4-events.jsonl`
   - the same for `stderr.log` → `h4-stderr.log`
5. `wsl -u root -e /bin/ls -la /home/auditor/jobs-m1noexec/h4-noexec`
   - expected: no `MARKER-should-not-exist`
6. `wsl -u root -e /usr/bin/grep -c command_execution /home/auditor/jobs-m1noexec/h4-noexec/events.jsonl`
   - expected: `0`
7. Clean up:
   - `wsl -u root -e /usr/bin/systemctl stop audit-runner-m1noexec`
   - `wsl -u root -e /bin/rm -rf /home/auditor/jobs-m1noexec`

## H5. Descriptors in the deployed configuration (socket activation by systemd, auditor sandbox)
No quota is used.
1. Start a transient socket plus service. The sandbox properties are the same as the installed unit; the codex is a script that lists its descriptors:
   ```
   wsl -u root -e /usr/bin/systemd-run --unit=audit-runner-m1fd --socket-property=ListenStream=/run/audit-runner-m1fd.sock --socket-property=SocketUser=auditor --socket-property=SocketMode=0666 -p User=auditor -p Group=auditor -p NoNewPrivileges=yes -p ProtectSystem=strict -p ProtectHome=tmpfs -p BindPaths=/home/auditor -p PrivateTmp=yes -p UMask=0077 /usr/local/lib/audit-runner/audit-runner --allow-uid 999 --allow-cgroup /system.slice/m1-fd.service --codex /mnt/e/rust-agent-browser/reviews/m1-verify/fd-codex.sh --codex-home /home/auditor/.codex --workdir /home/auditor/jobs-m1fd
   ```
2. Submit from inside the allowed unit:
   ```
   wsl -u root -e /usr/bin/systemd-run --unit=m1-fd --wait --pipe --collect -p User=reviewgate -p Group=reviewgate /usr/bin/python3 /mnt/e/rust-agent-browser/reviews/m1-verify/client.py /run/audit-runner-m1fd.sock h5-fd x
   ```
   - expected: `completed`. The `answer` lists exactly:
     - `0 pipe:[…]`
     - `1 /home/auditor/jobs-m1fd/h5-fd/events.jsonl`
     - `2 /home/auditor/jobs-m1fd/h5-fd/stderr.log`
     - `10 /mnt/e/rust-agent-browser/reviews/m1-verify/fd-codex.sh`
   - **Any other line, especially `socket:`, is a failure.**
3. `wsl -u root -e /usr/bin/systemctl show -p Listen audit-runner-m1fd.socket`
   - this records that the activation came from systemd
4. Clean up:
   - `wsl -u root -e /usr/bin/systemctl stop audit-runner-m1fd.socket audit-runner-m1fd.service`
   - `wsl -u root -e /bin/rm -rf /home/auditor/jobs-m1fd`
   - `wsl -u root -e /bin/rm -f /run/audit-runner-m1fd.sock`

## H6. Restore
1. `wsl -u root -e /usr/bin/systemctl stop audit-runner.service`
2. `wsl -u root -e /usr/bin/systemctl is-active audit-runner.socket audit-runner.service audit-runner-m1noexec audit-runner-m1fd.socket`
   - expected: active / inactive / inactive / inactive

Write the raw output to `dsh-raw-r4.txt` and post the result table.
