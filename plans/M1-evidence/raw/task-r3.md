# M1 verification, round 3: install the fd fix and two remaining checks

Same authorization, rules and stop-on-unexpected as task.md. No Codex quota is used.

**What changed:** Claude found that systemd's listening socket (fd 3) was passed to Codex without close-on-exec, so a Codex process could accept the gate's next connection. The runner now sets FD_CLOEXEC on it.
- A test using systemd-socket-activate checks this.
- That test fails without the fix.

New binary sha256 is `77467be6db7651ec36456f93c05adc7ad55fb5bba6c0d20be4af389de0fae853`; it is in this folder. The unit files are unchanged.

## G1. Install
1. `wsl -u root -e /usr/bin/systemctl stop audit-runner.service`
2. `wsl -u root -e /usr/bin/install -m 0755 -o root -g root /mnt/e/rust-agent-browser/reviews/m1-verify/audit-runner /usr/local/lib/audit-runner/audit-runner`
3. `wsl -u root -e /usr/bin/sha256sum /usr/local/lib/audit-runner/audit-runner`
   - expected: the hash above

## G2. The new binary refuses correctly (smoke test)
1. `wsl -u root -e /usr/bin/sudo -u reviewgate /usr/bin/python3 /mnt/e/rust-agent-browser/reviews/m1-verify/client.py /run/audit-runner/runner.sock g2-reviewgate hello`
   - expected: rejected, `peer cgroup /non-systemd is not /system.slice/review-gate.service`
2. `wsl -u root -e /usr/bin/journalctl -u audit-runner.service --no-pager -n 5`

## G3. The gate's logs, as auditor
1. `wsl -u root -e /usr/bin/sudo -u auditor /usr/bin/journalctl -u review-gate.service --no-pager -n 3`
   - expected: no gate log lines; a message about insufficient permissions or "No entries" instead
   - **If gate log lines appear, report only how many lines there were, not their text.**
2. `wsl -u root -e /usr/bin/id -nG auditor`
   - expected: only auditor; no adm or systemd-journal
3. `wsl -u root -e /usr/bin/sudo -u auditor /bin/sh -c 'test -r /var/lib/reviewgate/relay/traces/2026-10-05_10-02-40.gate.md && echo READABLE || echo not-readable'`
   - expected: **READABLE**. The gate's relay trace dir is world-readable by design (install.sh step 3), so this is recorded, not a failure.
   - Do not print the file.

## G4. Restore
1. `wsl -u root -e /usr/bin/systemctl stop audit-runner.service`
2. `wsl -u root -e /usr/bin/systemctl is-active audit-runner.socket audit-runner.service`
   - expected: active / inactive

Write the raw output to `dsh-raw-r3.txt` and post the result table.
