# M1 verification, round 6: Codex's third REWORK (stop rule) and an escape I found

Same authorization ("可以交给dsh", M1 verification) and rules as before: absolute paths, raw output with exit codes, **stop at the first unexpected result**, and never print `/etc/review-gate/*`, `/var/lib/reviewgate/state`, trace files or `auth.json`. No Codex quota is used in this round.

**What changed in the runner**
1. **Stop rule (Codex's blocking point).** When a job's cleanup is not confirmed, the runner stops after that connection whether its reply is delivered, fails or times out (30 s).
2. **Escape (found by Claude).** A process that leaves Codex's process group (`setsid`) survived the per-job kill, and the job still said `cleanup: gone`. The service now runs with `--own-cgroup`: after every job the runner kills every other process in its own service cgroup and reports `gone` only when it is the sole process left. It refuses to start outside a `.service` cgroup.
3. **Unit:** `KillMode=control-group` is now explicit (systemd's default).

| File | sha256 |
|---|---|
| `audit-runner` (production; contains no fault code) | `69983378df3bfc3f9fb7bd93c32c54ce5539a19879b88d6b42ba478fee79b9c4` |
| `audit-runner.service` | `6150c5123a81adb4b99ebc67977a3c42433993289fb06c01a3732025ecdb4dcf` |
| `audit-runner-fault` (**test build only**: with `AUDIT_RUNNER_FAULT=cleanup-unconfirmed` every job's cleanup is "not confirmed"; never install it) | `ceb4013bfcd7b142a3ff92c36d36dfdce2188e44f83e8587ba591a5b5fa9ee63` |
| `fault-codex.sh` (fake Codex that leaves `sleep 4790` behind in its own session and reports its pid as `left-pid=`) | `b9c44d1c5b8218e30382800c6f9494d1373a4941e74de97f917299fab523be78` |

## K1. Install the production runner
1. `wsl -u root -e /usr/bin/systemctl stop audit-runner.service`
2. `wsl -u root -e /usr/bin/install -m 0755 -o root -g root /mnt/e/rust-agent-browser/reviews/m1-verify/audit-runner /usr/local/lib/audit-runner/audit-runner`
3. `wsl -u root -e /usr/bin/install -m 0644 -o root -g root /mnt/e/rust-agent-browser/reviews/m1-verify/audit-runner.service /etc/systemd/system/audit-runner.service`
4. `wsl -u root -e /usr/bin/systemctl daemon-reload`
5. `wsl -u root -e /usr/bin/sha256sum /usr/local/lib/audit-runner/audit-runner /etc/systemd/system/audit-runner.service`
   - expected: the table above
6. `wsl -u root -e /usr/bin/grep -c AUDIT_RUNNER_FAULT /usr/local/lib/audit-runner/audit-runner`
   - expected: `0` (exit code 1 is fine)
7. Smoke test:
   - command: `wsl -u root -e /usr/bin/sudo -u reviewgate /usr/bin/python3 /mnt/e/rust-agent-browser/reviews/m1-verify/client.py /run/audit-runner/runner.sock k1-reviewgate hello`
   - expected: rejected with the cgroup reason. This also shows the service started with `--own-cgroup` inside its own cgroup.
8. `wsl -u root -e /usr/bin/systemctl show -p KillMode -p ActiveState -p ExecMainStatus audit-runner.service`
   - expected: `KillMode=control-group`, `ActiveState=active`

## K2. The cgroup sweep ends an escaped process (production binary, deployed sandbox)
1. Start a transient socket plus service, the same as the installed unit, with fake Codex `fault-codex.sh`:
   ```
   wsl -u root -e /usr/bin/systemd-run --unit=audit-runner-m1esc --socket-property=ListenStream=/run/audit-runner-m1esc.sock --socket-property=SocketUser=auditor --socket-property=SocketMode=0666 -p User=auditor -p Group=auditor -p NoNewPrivileges=yes -p ProtectSystem=strict -p ProtectHome=tmpfs -p BindPaths=/home/auditor -p PrivateTmp=yes -p UMask=0077 -p KillMode=control-group /usr/local/lib/audit-runner/audit-runner --allow-uid 999 --allow-cgroup /system.slice/m1-esc.service --codex /mnt/e/rust-agent-browser/reviews/m1-verify/fault-codex.sh --codex-home /home/auditor/.codex --run-home /home/auditor/codex-run-m1esc --workdir /home/auditor/jobs-m1esc --own-cgroup
   ```
2. Submit:
   ```
   wsl -u root -e /usr/bin/systemd-run --unit=m1-esc --wait --pipe --collect -p User=reviewgate -p Group=reviewgate /usr/bin/python3 /mnt/e/rust-agent-browser/reviews/m1-verify/client.py /run/audit-runner-m1esc.sock k2-esc x
   ```
   - expected: `completed`, `cleanup: "gone"`; note the `left-pid=N` in the answer.
3. `wsl -u root -e /bin/cat /proc/N/cmdline`
   - expected: **No such file** (the escaped `sleep 4790` was ended)
4. `wsl -u root -e /usr/bin/systemctl show -p ActiveState audit-runner-m1esc.service`
   - expected: `active` (the runner keeps serving after a confirmed cleanup)
5. Clean up:
   - `wsl -u root -e /usr/bin/systemctl stop audit-runner-m1esc.socket audit-runner-m1esc.service`
   - `wsl -u root -e /bin/rm -rf /home/auditor/jobs-m1esc`
   - `wsl -u root -e /bin/rm -f /run/audit-runner-m1esc.sock`

## K3. Stop rule: an unconfirmed cleanup makes the runner exit and systemd clears its cgroup (fault build, no --own-cgroup)
Here the runner itself leaves the escaped process alone (no `--own-cgroup`), and the fault makes the cleanup "not confirmed". So clearing up the leftover is up to systemd once the runner exits.
1. Start the transient socket plus service:
   ```
   wsl -u root -e /usr/bin/systemd-run --unit=audit-runner-m1stop --socket-property=ListenStream=/run/audit-runner-m1stop.sock --socket-property=SocketUser=auditor --socket-property=SocketMode=0666 -p User=auditor -p Group=auditor -p NoNewPrivileges=yes -p ProtectSystem=strict -p ProtectHome=tmpfs -p BindPaths=/home/auditor -p PrivateTmp=yes -p UMask=0077 -p KillMode=control-group -p Environment=AUDIT_RUNNER_FAULT=cleanup-unconfirmed /mnt/e/rust-agent-browser/reviews/m1-verify/audit-runner-fault --allow-uid 999 --allow-cgroup /system.slice/m1-stop.service --codex /mnt/e/rust-agent-browser/reviews/m1-verify/fault-codex.sh --codex-home /home/auditor/.codex --run-home /home/auditor/codex-run-m1stop --workdir /home/auditor/jobs-m1stop
   ```
2. Submit:
   ```
   wsl -u root -e /usr/bin/systemd-run --unit=m1-stop --wait --pipe --collect -p User=reviewgate -p Group=reviewgate /usr/bin/python3 /mnt/e/rust-agent-browser/reviews/m1-verify/client.py /run/audit-runner-m1stop.sock k3-a x
   ```
   - expected: `status: "failed"`, `cleanup: "not confirmed: injected fault"`; note `left-pid=N`.
3. Wait 2 s, then:
   - `wsl -u root -e /bin/cat /proc/N/cmdline`
     - expected: **No such file** (systemd ended the service and its cgroup)
   - `wsl -u root -e /usr/bin/systemctl show -p ActiveState -p Result -p ExecMainStatus audit-runner-m1stop.service`
     - expected: not active; `Result=exit-code`; status 1
   - `wsl -u root -e /usr/bin/journalctl -u audit-runner-m1stop.service --no-pager -n 15`
4. A new connection starts a fresh runner. The same submit with job id `k3-b`:
   - expected: a reply again (`failed` / `not confirmed: injected fault`), so socket activation recovers.
5. Clean up:
   - `wsl -u root -e /usr/bin/systemctl stop audit-runner-m1stop.socket audit-runner-m1stop.service`
   - `wsl -u root -e /usr/bin/systemctl reset-failed audit-runner-m1stop.service audit-runner-m1stop.socket`
   - `wsl -u root -e /bin/rm -rf /home/auditor/jobs-m1stop /home/auditor/codex-run-m1stop`
   - `wsl -u root -e /bin/rm -f /run/audit-runner-m1stop.sock`
   - Then make sure no `sleep 4790` is left; if one is, report it before killing it.

## K4. Restore
1. `wsl -u root -e /usr/bin/systemctl stop audit-runner.service`
2. `wsl -u root -e /usr/bin/systemctl is-active audit-runner.socket audit-runner.service audit-runner-m1esc.service audit-runner-m1stop.service`
   - expected: active / inactive ×3

Write the raw output to `dsh-raw-r6.txt` and post the result table.
