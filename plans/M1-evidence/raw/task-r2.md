# M1 verification, round 2: install the fixed runner and re-check C (from DSH's finding 1)

Same authorization, rules and stop-on-unexpected as task.md.

**What changed:** the runner now checks the peer's **cgroup** (`/proc/<pid>/cgroup`, readable across uids) instead of `/proc/<pid>/exe` (cross-uid readlink needs ptrace).
- The flag `--allow-exe` became `--allow-cgroup`.
- The service unit uses `--allow-cgroup /system.slice/review-gate.service`.
- Only root can place a process in that unit's cgroup.
- The runner also lists commands that started but never completed, as "-> never completed" (finding 3).

New binary sha256 is `b5ef7e50b5860da6cc79db2ef6117333eb0a55bfaeb6fd13624ab767f48283aa`; service unit sha256 is `c25599c4ad58cef6546a889ed1a23c24f25f0fe0a52fd47cfe6e4d986b4c2d15`. Both are in this folder.

## F1. Install
1. `wsl -u root -e /usr/bin/systemctl stop audit-runner.service`
2. `wsl -u root -e /usr/bin/install -m 0755 -o root -g root /mnt/e/rust-agent-browser/reviews/m1-verify/audit-runner /usr/local/lib/audit-runner/audit-runner`
3. `wsl -u root -e /usr/bin/install -m 0644 -o root -g root /mnt/e/rust-agent-browser/reviews/m1-verify/audit-runner.service /etc/systemd/system/audit-runner.service`
4. `wsl -u root -e /usr/bin/systemctl daemon-reload`
5. `wsl -u root -e /usr/bin/sha256sum /usr/local/lib/audit-runner/audit-runner /etc/systemd/system/audit-runner.service`
   - expected: the two hashes above
6. `wsl -u root -e /usr/bin/systemctl is-active audit-runner.socket audit-runner.service`
   - expected: active / inactive

## F2. Impostors on the real runner, with reasons
1. As auditor:
   - command: `wsl -u root -e /usr/bin/sudo -u auditor /usr/bin/python3 /mnt/e/rust-agent-browser/reviews/m1-verify/client.py /run/audit-runner/runner.sock f2-auditor hello`
   - expected: rejected, `peer uid 997 is not allowed`
2. As reviewgate, outside the gate's unit:
   - command: `wsl -u root -e /usr/bin/sudo -u reviewgate /usr/bin/python3 /mnt/e/rust-agent-browser/reviews/m1-verify/client.py /run/audit-runner/runner.sock f2-reviewgate hello`
   - expected: rejected, `peer cgroup <something> is not /system.slice/review-gate.service`
   - **The reason must match this time.** "cannot read peer cgroup" is a failure.
3. `wsl -u root -e /bin/ls -la /home/auditor/jobs`
   - expected: no f2-* directories
4. `wsl -u root -e /usr/bin/journalctl -u audit-runner.service --no-pager -n 10`

## F3. Positive path: a process inside the allowed unit is accepted
No Codex quota is used; the codex here is `/usr/bin/true`, so an accepted job ends as `failed` with "no answer".
1. Start the temporary runner (allowed cgroup `/system.slice/m1-pos.service`):
   ```
   wsl -u root -e /usr/bin/systemd-run --unit=audit-runner-m1pos -p User=auditor -p Group=auditor -p NoNewPrivileges=yes -p ProtectSystem=strict -p ProtectHome=tmpfs -p BindPaths=/home/auditor -p PrivateTmp=yes -p UMask=0077 -p RuntimeDirectory=audit-runner-m1pos -p RuntimeDirectoryMode=0755 /usr/local/lib/audit-runner/audit-runner --socket /run/audit-runner-m1pos/runner.sock --allow-uid 999 --allow-cgroup /system.slice/m1-pos.service --codex /usr/bin/true --codex-home /home/auditor/.codex --workdir /home/auditor/jobs-m1pos --timeout-secs 60
   ```
2. `wsl -u root -e /bin/chmod 0666 /run/audit-runner-m1pos/runner.sock`
3. Inside the allowed unit, as reviewgate:
   ```
   wsl -u root -e /usr/bin/systemd-run --unit=m1-pos --wait --pipe --collect -p User=reviewgate -p Group=reviewgate /usr/bin/python3 /mnt/e/rust-agent-browser/reviews/m1-verify/client.py /run/audit-runner-m1pos/runner.sock f3-inside hello
   ```
   - expected: `"status": "failed"` and an error containing `no answer`. That means it was accepted. **"rejected" is a failure.**
4. Same uid, outside the unit:
   - command: `wsl -u root -e /usr/bin/sudo -u reviewgate /usr/bin/python3 /mnt/e/rust-agent-browser/reviews/m1-verify/client.py /run/audit-runner-m1pos/runner.sock f3-outside hello`
   - expected: rejected, `peer cgroup … is not /system.slice/m1-pos.service`
5. `wsl -u root -e /bin/ls -la /home/auditor/jobs-m1pos`
   - expected: f3-inside exists; f3-outside does not
6. Clean up:
   - `wsl -u root -e /usr/bin/systemctl stop audit-runner-m1pos`
   - `wsl -u root -e /bin/rm -rf /home/auditor/jobs-m1pos`

## F4. Restore
1. `wsl -u root -e /usr/bin/systemctl stop audit-runner.service`
2. `wsl -u root -e /usr/bin/systemctl is-active audit-runner.socket audit-runner.service audit-runner-m1pos`
   - expected: active / inactive / inactive

Write the raw output to `dsh-raw-r2.txt` and post the result table as before. On finding 3: noted. Next time, keep a copy of events.jsonl before cleanup; you followed the book correctly.
