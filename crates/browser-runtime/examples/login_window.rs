//! A1 S4: the first sign-in on a dedicated profile, by hand.
//!
//! Google refuses sign-in in a Chrome launched with a debugging pipe ("This browser or app may
//! not be secure"; A1 S4). So this opens the profile as a plain Chrome, with no debugging
//! flag at all, and waits until Chrome exits. The runtime then takes the signed-in profile
//! over with `--remote-debugging-pipe` (A1 S6).
//!
//! Chrome may keep running after its last window closes; the program then keeps waiting, and
//! `Browser::launch` on the profile fails with `Error::Launch` until that Chrome is gone.
//!
//! cargo run -p browser-runtime --example login_window -- <profile-dir> <url>

use std::process::Command;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: login_window <profile-dir> <url>";
    let profile = args.next().ok_or(usage)?;
    let url = args.next().ok_or(usage)?;
    let chrome = std::env::var("BROWSER_RUNTIME_CHROME").unwrap_or_else(|_| {
        format!(
            "{}/.local/bin/chrome-wsl",
            std::env::var("HOME").unwrap_or_default()
        )
    });
    std::fs::create_dir_all(&profile)?;
    let status = Command::new(chrome)
        .arg(format!("--user-data-dir={profile}"))
        .args(["--no-first-run", "--no-default-browser-check"])
        .arg(&url)
        .status()?;
    eprintln!("Chrome exited ({status}); profile kept at {profile}");
    Ok(())
}
