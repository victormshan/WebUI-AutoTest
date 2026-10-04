//! review-gate command line.

use std::io::Read;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use review_gate::reviewer::{self, AskError, ReviewerConfig};

#[derive(Parser)]
#[command(
    name = "review-gate",
    version,
    about = "Independent review gate for auto-iterate (three-party protocol)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Ask an external AI (another vendor): prompt on stdin, answer on stdout.
    /// Usable as webtest's `--review-command "review-gate ask"`.
    Ask {
        /// auto | gemini-api | openai | web-gemini
        #[arg(long, default_value = "auto")]
        provider: String,
        /// Print {provider, model, family, channel, answer} as JSON
        #[arg(long)]
        json: bool,
        /// Only report which providers are available
        #[arg(long)]
        probe: bool,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Ask {
            provider,
            json,
            probe,
        } => ask(&provider, json, probe).await,
    }
}

/// Exit codes: 0 answered · 3 no external AI available · 1 failed · 2 usage error.
async fn ask(provider: &str, json: bool, probe: bool) -> ExitCode {
    let cfg = ReviewerConfig::from_env();
    if probe {
        let map: serde_json::Map<String, serde_json::Value> = reviewer::probe(&cfg)
            .await
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.into()))
            .collect();
        println!("{}", serde_json::Value::Object(map));
        return ExitCode::SUCCESS;
    }
    let mut prompt = String::new();
    if std::io::stdin().read_to_string(&mut prompt).is_err() || prompt.trim().is_empty() {
        eprintln!("empty prompt on stdin");
        return ExitCode::from(2);
    }
    match reviewer::ask(&cfg, prompt.trim(), provider).await {
        Ok(r) if json => {
            println!("{}", serde_json::to_string(&r).expect("serializable"));
            ExitCode::SUCCESS
        }
        Ok(r) => {
            println!("{}", r.answer);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(if matches!(e, AskError::Unavailable(_)) {
                3
            } else {
                1
            })
        }
    }
}
