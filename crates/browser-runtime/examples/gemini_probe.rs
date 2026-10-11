//! A1 S6/S7: on a signed-in profile, open a new Gemini chat, type a one-time nonce prompt with
//! trusted input, send it, sample the page every 500 ms and read the answer. Prints a JSON
//! report: the signal timeline (counts and lengths, no page text except the final answer)
//! and whether the answer matches the nonce. No `Runtime.enable` (a known detection vector).
//!
//! cargo run -p browser-runtime --example gemini_probe -- <profile-dir> [--headless]

use std::time::{Duration, Instant};

use browser_runtime::{Browser, LaunchConfig};
use serde_json::{Value, json};

const OBSERVE: &str = r#"(() => {
  const q = (s) => Array.from(document.querySelectorAll(s));
  const responses = q('model-response');
  const last = responses[responses.length - 1];
  const content = last ? (last.querySelector('message-content') || last) : null;
  const composer = document.querySelector('rich-textarea [contenteditable="true"], [contenteditable="true"][aria-label*="prompt" i]');
  const buttons = q('button');
  const label = (b) => (b.getAttribute('aria-label') || '').toLowerCase();
  return {
    url: location.pathname,
    signin: !!document.querySelector('a[href*="accounts.google.com/ServiceLogin"], a[href*="accounts.google.com/v3/signin"]'),
    composer: !!composer,
    composerLen: composer ? composer.innerText.trim().length : -1,
    responses: responses.length,
    userQueries: q('user-query').length,
    lastLen: content ? content.innerText.trim().length : 0,
    lastText: content ? content.innerText.trim().slice(0, 64) : '',
    stop: buttons.some((b) => label(b).includes('stop') || label(b).includes('停止')),
    send: buttons.some((b) => b.getAttribute('data-test-id') === 'send-button' || label(b).includes('send')),
  };
})()"#;

const DIGITS: [&str; 10] = ["零", "一", "二", "三", "四", "五", "六", "七", "八", "九"];

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let profile = args
        .first()
        .ok_or("usage: gemini_probe <profile-dir> [--headless]")?;
    let chrome = std::env::var("BROWSER_RUNTIME_CHROME").unwrap_or_else(|_| {
        format!(
            "{}/.local/bin/chrome-wsl",
            std::env::var("HOME").unwrap_or_default()
        )
    });
    let mut cfg = LaunchConfig::new(chrome, profile);
    cfg.headless = args.iter().any(|a| a == "--headless");
    let b = Browser::launch(cfg).await?;
    let t0 = Instant::now();
    let t = b
        .call(
            "Target.createTarget",
            json!({ "url": "https://gemini.google.com/app" }),
        )
        .await?;
    let s = b
        .call(
            "Target.attachToTarget",
            json!({ "targetId": t["targetId"], "flatten": true }),
        )
        .await?;
    let sid = s["sessionId"].as_str().unwrap().to_string();
    let eval = |expr: &'static str| {
        let b = &b;
        let sid = sid.clone();
        async move {
            let r = b
                .call_in(
                    Some(&sid),
                    "Runtime.evaluate",
                    json!({ "expression": expr, "returnByValue": true }),
                )
                .await?;
            Ok::<Value, browser_runtime::Error>(r["result"]["value"].clone())
        }
    };
    let mut timeline = Vec::new();
    let ms = |t: Instant| t.duration_since(t0).as_millis();

    // 1. Page ready: composer present, or a sign-in prompt (then stop: S4/S6 blocked).
    let mut ready = Value::Null;
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let o = eval(OBSERVE).await.unwrap_or(Value::Null);
        if o["composer"] == true || o["signin"] == true {
            ready = o;
            break;
        }
    }
    timeline.push(json!({ "ms": ms(Instant::now()), "phase": "ready", "obs": ready }));
    if ready["composer"] != true {
        println!(
            "{}",
            json!({ "outcome": "no-composer", "timeline": timeline })
        );
        b.close().await?;
        return Ok(());
    }

    // 2. Nonce prompt, typed with trusted input.
    let nonce = 100 + (std::process::id() as u64 * 7919 + t0.elapsed().as_nanos() as u64) % 900;
    let expect: String = nonce
        .to_string()
        .chars()
        .map(|c| DIGITS[c.to_digit(10).unwrap() as usize])
        .collect();
    let prompt =
        format!("请把数字 {nonce} 的每一位写成中文数字，只输出这三个汉字，不要任何其他内容。");
    eval(r#"(() => { const c = document.querySelector('rich-textarea [contenteditable="true"], [contenteditable="true"][aria-label*="prompt" i]'); c.focus(); return true; })()"#).await?;
    b.call_in(Some(&sid), "Input.insertText", json!({ "text": prompt }))
        .await?;
    let before = eval(OBSERVE).await?;
    timeline.push(json!({ "ms": ms(Instant::now()), "phase": "typed", "obs": before }));
    for (kind, text) in [("keyDown", Some("\r")), ("keyUp", None)] {
        let mut p =
            json!({ "type": kind, "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13 });
        if let Some(t) = text {
            p["text"] = json!(t);
        }
        b.call_in(Some(&sid), "Input.dispatchKeyEvent", p).await?;
    }
    let sent_at = Instant::now();
    timeline.push(json!({ "ms": ms(sent_at), "phase": "enter-sent" }));

    // 3. Sample until the answer is stable: a new response, no stop button, same text 3 times.
    let base = before["responses"].as_u64().unwrap_or(0);
    let mut stable = 0;
    let mut prev = String::new();
    let mut answer = String::new();
    let mut outcome = "timeout";
    while sent_at.elapsed() < Duration::from_secs(120) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let o = eval(OBSERVE).await.unwrap_or(Value::Null);
        let text = o["lastText"].as_str().unwrap_or("").to_string();
        timeline.push(json!({ "ms": ms(Instant::now()), "obs": {
            "url": o["url"], "responses": o["responses"], "userQueries": o["userQueries"],
            "lastLen": o["lastLen"], "stop": o["stop"], "composerLen": o["composerLen"] } }));
        let new_response = o["responses"].as_u64().unwrap_or(0) > base;
        if new_response && o["stop"] == false && !text.is_empty() && text == prev {
            stable += 1;
            if stable >= 3 {
                answer = text;
                outcome = "done";
                break;
            }
        } else {
            stable = 0;
        }
        prev = text;
    }
    let report = json!({
        "outcome": outcome,
        "nonce": nonce,
        "expect": expect,
        "answer": answer,
        "match": answer == expect,
        "elapsedMs": sent_at.elapsed().as_millis(),
        "samples": timeline.len(),
        "timeline": timeline,
    });
    println!("{report}");
    b.close().await?;
    Ok(())
}
