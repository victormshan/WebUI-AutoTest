//! Launch configuration. Arguments are explicit: nothing is added that the caller did not
//! choose, in particular no `--enable-automation` (it can get Google sign-in refused) and no
//! unconditional `--no-sandbox` / `--disable-dev-shm-usage` (baseline §5, P1).

use std::path::PathBuf;
use std::time::Duration;

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct LaunchConfig {
    /// Chrome executable (or a wrapper script that execs it).
    pub chrome: PathBuf,
    /// Dedicated profile directory. Required: Chrome ≥136 refuses debugging on the default one,
    /// and the review channel needs a profile only its own user can read.
    pub user_data_dir: PathBuf,
    /// Headless (`--headless=new`) or a visible window (e.g. on WSLg).
    pub headless: bool,
    /// Further arguments, appended as given.
    pub extra_args: Vec<String>,
    /// Extra environment for the Chrome process.
    pub env: Vec<(String, String)>,
    /// Per-command response timeout.
    pub command_timeout: Duration,
    /// How long `close` waits for a graceful exit before killing the process group.
    pub close_timeout: Duration,
}

impl LaunchConfig {
    pub fn new(chrome: impl Into<PathBuf>, user_data_dir: impl Into<PathBuf>) -> Self {
        Self {
            chrome: chrome.into(),
            user_data_dir: user_data_dir.into(),
            headless: true,
            extra_args: Vec::new(),
            env: Vec::new(),
            command_timeout: Duration::from_secs(30),
            close_timeout: Duration::from_secs(10),
        }
    }

    /// Checks what can be checked before launching, with errors a person can act on.
    pub fn validate(&self) -> Result<()> {
        if !self.chrome.is_file() {
            return Err(Error::Config(format!(
                "Chrome executable not found: {}",
                self.chrome.display()
            )));
        }
        if self.user_data_dir.as_os_str().is_empty() {
            return Err(Error::Config("user_data_dir is required".into()));
        }
        if self.user_data_dir.exists() && !self.user_data_dir.is_dir() {
            return Err(Error::Config(format!(
                "user_data_dir is not a directory: {}",
                self.user_data_dir.display()
            )));
        }
        if self
            .extra_args
            .iter()
            .any(|a| a.starts_with("--remote-debugging-port"))
        {
            return Err(Error::Config(
                "--remote-debugging-port is not allowed: this runtime talks CDP over a pipe only"
                    .into(),
            ));
        }
        Ok(())
    }

    /// The full argument list, in order.
    pub fn args(&self) -> Vec<String> {
        let mut a = vec![
            "--remote-debugging-pipe".to_string(),
            format!("--user-data-dir={}", self.user_data_dir.display()),
            "--no-first-run".to_string(),
            "--no-default-browser-check".to_string(),
        ];
        if self.headless {
            a.push("--headless=new".into());
        }
        a.extend(self.extra_args.iter().cloned());
        a.push("about:blank".into());
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_explicit_and_never_open_a_port() {
        let c = LaunchConfig::new("/bin/true", "/tmp/p");
        let a = c.args();
        assert!(a.contains(&"--remote-debugging-pipe".to_string()));
        assert!(a.iter().all(|x| !x.contains("remote-debugging-port")));
        assert!(a.iter().all(|x| x != "--enable-automation"));
        assert!(a.iter().all(|x| x != "--no-sandbox"));
        assert!(a.contains(&"--user-data-dir=/tmp/p".to_string()));
    }

    #[test]
    fn a_port_or_a_missing_binary_is_a_config_error() {
        let mut c = LaunchConfig::new("/nonexistent/chrome", "/tmp/p");
        assert!(matches!(c.validate(), Err(Error::Config(m)) if m.contains("not found")));
        c.chrome = "/bin/true".into();
        c.extra_args.push("--remote-debugging-port=9222".into());
        assert!(matches!(c.validate(), Err(Error::Config(m)) if m.contains("not allowed")));
    }
}
