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
    /// Executable the submitting process must run (e.g. /usr/local/bin/review-gate).
    #[arg(long)]
    allow_exe: Option<PathBuf>,
    /// The auditor's Codex CLI.
    #[arg(long)]
    codex: PathBuf,
    #[arg(long)]
    codex_home: PathBuf,
    /// Per-job directories (input, events, answer).
    #[arg(long)]
    workdir: PathBuf,
    #[arg(long, default_value_t = 1800)]
    timeout_secs: u64,
    #[arg(long, default_value = "high")]
    reasoning_effort: String,
}

/// The listening socket systemd passed (LISTEN_FDS=1, fd 3), if any.
fn activated() -> Option<std::os::unix::net::UnixListener> {
    let pid_ok = std::env::var("LISTEN_PID").ok()?.parse::<u32>().ok()? == std::process::id();
    let one = std::env::var("LISTEN_FDS").ok()? == "1";
    // SAFETY: systemd hands over fd 3 for exactly this process when both variables match.
    (pid_ok && one).then(|| unsafe { std::os::unix::net::UnixListener::from_raw_fd(3) })
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let listener = match activated() {
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
        allow_exe: cli.allow_exe,
        codex: cli.codex,
        codex_home: cli.codex_home,
        workdir: cli.workdir,
        default_timeout: Duration::from_secs(cli.timeout_secs),
        reasoning_effort: cli.reasoning_effort,
    };
    eprintln!("audit-runner {} ready", env!("CARGO_PKG_VERSION"));
    serve(cfg, listener).await?;
    Ok(())
}
