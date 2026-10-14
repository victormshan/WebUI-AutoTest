//! audit-runner command line. Under systemd it takes its socket from socket activation
//! (`audit-runner.socket`: owner auditor, group reviewgate, mode 0660), so the runner itself
//! needs no privileges; without it, `--socket` binds one (tests, manual runs).

use std::os::fd::FromRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use audit_runner::{Config, serve};
use clap::Parser;
use tokio::net::UnixListener;

#[derive(Parser)]
#[command(
    name = "audit-runner",
    about = "Runs milestone audit jobs for review-gate; holds no keys"
)]
struct Cli {
    /// Socket to bind when not socket-activated.
    #[arg(long)]
    socket: Option<PathBuf>,
    /// The only uid allowed to submit jobs (review-gate's).
    #[arg(long)]
    allow_uid: u32,
    /// cgroup the submitting process must belong to (e.g. /system.slice/review-gate.service).
    #[arg(long)]
    allow_cgroup: Option<String>,
    /// The auditor's Codex CLI.
    #[arg(long)]
    codex: PathBuf,
    /// The auditor's Codex home; only its login (auth.json) is used.
    #[arg(long)]
    codex_home: PathBuf,
    /// Codex's home during a job, recreated empty (plus the login) for every job.
    #[arg(long)]
    run_home: PathBuf,
    /// Per-job directories (input, events, answer).
    #[arg(long)]
    workdir: PathBuf,
    /// The runner runs in its own systemd service cgroup: after every job, end every other
    /// process in it and confirm it is empty. Refuses to start outside a `.service` cgroup.
    #[arg(long)]
    own_cgroup: bool,
    #[arg(long, default_value_t = 1800)]
    timeout_secs: u64,
    #[arg(long, default_value = "high")]
    reasoning_effort: String,
}

/// The listening socket systemd passed (LISTEN_FDS=1, fd 3), if this process was
/// socket-activated. An error when it was, but fd 3 cannot be made close-on-exec: the runner
/// then stops rather than run with a socket every Codex process would inherit.
fn activated() -> anyhow::Result<Option<std::os::unix::net::UnixListener>> {
    let pid_ok = std::env::var("LISTEN_PID")
        .ok()
        .and_then(|p| p.parse::<u32>().ok())
        == Some(std::process::id());
    let one = std::env::var("LISTEN_FDS").ok().as_deref() == Some("1");
    if !(pid_ok && one) {
        return Ok(None);
    }
    // systemd passes fd 3 without close-on-exec; set it, or every Codex process would inherit
    // the listening socket and could accept the gate's next connection.
    // SAFETY: fcntl on fd 3 only changes its flags; systemd hands that fd to this process.
    unsafe {
        let flags = libc::fcntl(3, libc::F_GETFD);
        if flags < 0 || libc::fcntl(3, libc::F_SETFD, flags | libc::FD_CLOEXEC) < 0 {
            anyhow::bail!(
                "cannot set close-on-exec on the activated socket: {}",
                std::io::Error::last_os_error()
            );
        }
    }
    // SAFETY: systemd hands over fd 3 for exactly this process when both variables match.
    Ok(Some(unsafe {
        std::os::unix::net::UnixListener::from_raw_fd(3)
    }))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if cli.own_cgroup {
        let path = audit_runner::cgroup_of(std::process::id() as i32)?;
        anyhow::ensure!(
            path.ends_with(".service"),
            "--own-cgroup: not in a systemd service cgroup ({path})"
        );
        audit_runner::own_cgroup_procs()?;
    }
    let listener = match activated()? {
        Some(l) => {
            l.set_nonblocking(true)?;
            UnixListener::from_std(l)?
        }
        None => {
            let path = cli
                .socket
                .clone()
                .ok_or_else(|| anyhow::anyhow!("not socket-activated: --socket is required"))?;
            let _ = std::fs::remove_file(&path);
            let l = UnixListener::bind(&path)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660))?;
            l
        }
    };
    std::fs::create_dir_all(&cli.workdir)?;
    let cfg = Config {
        allow_uid: cli.allow_uid,
        allow_cgroup: cli.allow_cgroup,
        codex: cli.codex,
        codex_home: cli.codex_home,
        run_home: cli.run_home,
        workdir: cli.workdir,
        default_timeout: Duration::from_secs(cli.timeout_secs),
        reply_timeout: Duration::from_secs(30),
        own_cgroup: cli.own_cgroup,
        reasoning_effort: cli.reasoning_effort,
    };
    eprintln!("audit-runner {} ready", env!("CARGO_PKG_VERSION"));
    serve(cfg, listener).await?;
    Ok(())
}
