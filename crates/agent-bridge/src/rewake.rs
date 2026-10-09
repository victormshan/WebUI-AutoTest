//! `agent-bridge rewake`: the push channel of a Claude Code session.
//!
//! The service pushes to DSH, whose host takes injected prompts; a Claude Code session has no
//! such inlet. What it has are async hooks: a hook with `asyncRewake: true` runs in the
//! background and wakes the session when it exits 2. `rewake` is that hook: it waits on the
//! bridge and exits 2 when a message arrives that has not woken the session yet.
//!
//! - One waiter per state directory (an exclusive lock): the hook fires at every stop, and a
//!   second waiter would wake the session twice. A second `rewake` exits 0 at once.
//! - Messages that already woke the session are remembered, so a message left unacked does not
//!   wake it in a loop. The record holds only the keys that are still unread, so it stays small.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::client::Client;

/// `<task>#<n>`: what identifies a message.
pub fn key(m: &Value) -> String {
    format!(
        "{}#{}",
        m["task"].as_str().unwrap_or("?"),
        m["n"].as_u64().unwrap_or(0)
    )
}

/// Unread messages `msgs` against the keys that already woke the session: the messages that
/// have not, and the record to keep (the keys of every message now unread — an acked message
/// leaves the inbox and so leaves the record).
pub fn fresh<'a>(msgs: &'a [Value], seen: &BTreeSet<String>) -> (Vec<&'a Value>, BTreeSet<String>) {
    let new = msgs.iter().filter(|m| !seen.contains(&key(m))).collect();
    (new, msgs.iter().map(key).collect())
}

/// One line per message for the woken session: what arrived, not the message itself.
pub fn line(m: &Value) -> String {
    let title = m["meta"]["title"].as_str().unwrap_or("");
    format!(
        "{} n={} {} 来自 {}{}{}",
        m["task"].as_str().unwrap_or("?"),
        m["n"],
        m["kind"].as_str().unwrap_or("?"),
        m["from"].as_str().unwrap_or("?"),
        if title.is_empty() { "" } else { " " },
        title
    )
}

fn read_seen(p: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(p)
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

fn write_seen(p: &Path, seen: &BTreeSet<String>) -> Result<()> {
    let tmp = p.with_extension("tmp");
    let body: Vec<&str> = seen.iter().map(String::as_str).collect();
    std::fs::write(&tmp, body.join("\n"))?;
    std::fs::rename(&tmp, p)?;
    Ok(())
}

/// The exclusive lock, or None when another waiter holds it.
pub fn lock(dir: &Path) -> Result<Option<File>> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("rewake.lock"))?;
    Ok(f.try_lock().is_ok().then_some(f))
}

/// Wait until a message that has not woken the session arrives; return the lines to show.
/// Returns None when another waiter already runs.
pub async fn run(client: &Client, dir: PathBuf) -> Result<Option<Vec<String>>> {
    let Some(_lock) = lock(&dir)? else {
        return Ok(None);
    };
    let seen_path = dir.join("rewake-seen");
    loop {
        match client.wait(Duration::from_secs(3000), 60).await {
            Ok(msgs) if msgs.is_empty() => {} // long-poll timeout: keep waiting
            Ok(msgs) => {
                let (new, keep) = fresh(&msgs, &read_seen(&seen_path));
                write_seen(&seen_path, &keep)?;
                if !new.is_empty() {
                    return Ok(Some(new.into_iter().map(line).collect()));
                }
                // Only messages that already woke the session are unread: do not spin.
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
            Err(e) => {
                eprintln!("agent-bridge rewake: {e:#}; retrying in 30s");
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn m(task: &str, n: u64) -> Value {
        json!({ "task": task, "n": n, "kind": "note", "from": "dsh", "meta": {} })
    }

    #[test]
    fn a_message_wakes_once_and_the_record_holds_only_what_is_unread() {
        let first = [m("t", 1)];
        let (new, seen) = fresh(&first, &BTreeSet::new());
        assert_eq!(new.len(), 1, "a new message wakes");
        let (new, seen) = fresh(&first, &seen);
        assert!(
            new.is_empty(),
            "the same unread message does not wake again"
        );
        let more = [m("t", 1), m("u", 4)];
        let (new, seen) = fresh(&more, &seen);
        assert_eq!(new.iter().map(|x| key(x)).collect::<Vec<_>>(), ["u#4"]);
        // t#1 acked: it leaves the inbox and the record.
        let (_, seen) = fresh(&[m("u", 4)], &seen);
        assert_eq!(seen.into_iter().collect::<Vec<_>>(), ["u#4"]);
    }

    #[test]
    fn the_line_names_task_message_and_sender() {
        let mut x = m("webgemini-newchat-click", 33);
        assert_eq!(line(&x), "webgemini-newchat-click n=33 note 来自 dsh");
        x["meta"]["title"] = json!("送审");
        assert!(line(&x).ends_with("来自 dsh 送审"));
    }

    #[test]
    fn only_one_waiter_holds_the_lock() {
        let d = tempfile::tempdir().unwrap();
        let a = lock(d.path()).unwrap();
        assert!(a.is_some());
        assert!(
            lock(d.path()).unwrap().is_none(),
            "a second waiter steps aside"
        );
        drop(a);
        assert!(
            lock(d.path()).unwrap().is_some(),
            "the lock is free once the first exits"
        );
    }

    #[test]
    fn the_record_survives_a_restart_of_the_waiter() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("rewake-seen");
        assert!(read_seen(&p).is_empty(), "no record yet");
        let (_, seen) = fresh(&[m("t", 2)], &BTreeSet::new());
        write_seen(&p, &seen).unwrap();
        let again = [m("t", 2)];
        let (new, _) = fresh(&again, &read_seen(&p));
        assert!(new.is_empty());
    }
}
