//! McpBackend against a real chrome-devtools-mcp. Needs Node.js (npx) and a
//! Chrome, so ignored by default: `cargo test -p agent -- --ignored`.

use std::path::PathBuf;

use agent::{BrowserBackend, Coverage, McpBackend};
use mcp_client::BrowserConfig;

/// chrome-devtools-mcp silently starts a fresh Chrome when its browser dies,
/// and says so only on the next tool call. Diagnostics read afterwards cover
/// only the new browser, so they must not pass for the run.
#[tokio::test]
#[ignore = "needs Chrome + npx"]
async fn a_browser_crash_reads_as_failed_diagnostics() {
    let profile = std::env::temp_dir().join(format!("webtest-crash-{}", std::process::id()));
    // Same Chrome lookup as the CLI: $WEBTEST_CHROME, else ~/.local/bin/chrome-wsl.
    let chrome = std::env::var_os("WEBTEST_CHROME")
        .map(PathBuf::from)
        .or_else(|| {
            let p = PathBuf::from(std::env::var_os("HOME")?).join(".local/bin/chrome-wsl");
            p.exists().then_some(p)
        });
    let browser = McpBackend::launch(&BrowserConfig {
        chrome_path: chrome,
        user_data_dir: Some(profile.clone()),
        ..Default::default()
    })
    .await
    .unwrap();
    let page = r#"data:text/html,<script>console.error("before-crash")</script>"#;
    let nav = BrowserBackend::navigate(&browser, page).await.unwrap();
    assert!(!nav.is_error, "{}", nav.text);
    let live = browser.diagnostics().await;
    assert_eq!(
        live.console_errors,
        vec!["[error] before-crash"],
        "{live:?}"
    );
    assert!(live.gaps().is_empty(), "{live:?}");

    // Kill Chrome only (`--user-data-dir=<profile>`); the MCP server lives on.
    let killed = std::process::Command::new("pkill")
        .args(["-9", "-f", &format!("user-data-dir={}", profile.display())])
        .status()
        .unwrap();
    assert!(killed.success(), "no Chrome to kill");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Another call hears of the restart first and consumes the one-time notice.
    let _ = BrowserBackend::snapshot(&browser).await;
    assert!(browser.restarted());

    let after = browser.diagnostics().await;
    assert!(
        !after
            .console_errors
            .iter()
            .any(|e| e.contains("before-crash")),
        "the fresh browser cannot have the old error: {after:?}"
    );
    assert!(
        matches!(after.console, Coverage::Failed { .. }),
        "{after:?}"
    );
    assert!(
        matches!(after.network, Coverage::Failed { .. }),
        "{after:?}"
    );
    assert_eq!(after.gaps().len(), 2);

    BrowserBackend::close(Box::new(browser)).await.unwrap();
    let _ = std::fs::remove_dir_all(&profile);
}
