use std::{fs, path::PathBuf, process::ExitCode};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use jev_solver::{
    browser::{self, BrowserOptions, BrowserStatus},
    client::JevClient,
    config::{Credentials, DEFAULT_MODEL, positive_duration},
    solver::{Grid, Solver, Thresholds, annotate},
};

#[derive(Parser)]
#[command(version, about = "Solve reCAPTCHA v2 image grids using TypeSafe Jev")]
struct Cli {
    /// Model ID or alias. Newest documented release as of 2026-10-15.
    #[arg(long, env = "TYPESAFE_MODEL", default_value = DEFAULT_MODEL, global = true)]
    model: String,
    /// Full TypeSafe evaluation URL (not a chat-completions endpoint).
    #[arg(long, env = "TYPESAFE_ENDPOINT", global = true)]
    endpoint: Option<String>,
    #[arg(long, default_value = "secrets.yml", global = true)]
    secrets: PathBuf,
    #[arg(long, default_value = "Official", global = true)]
    provider: String,
    /// Overall API deadline, including retries.
    #[arg(long, default_value_t = 60, global = true)]
    timeout_seconds: u64,
    #[arg(long, default_value_t = 2, global = true)]
    retries: usize,
    /// Minimum yes-probability required to select a tile.
    #[arg(long, default_value_t = 0.7, global = true)]
    select_threshold: f64,
    /// Maximum yes-probability required to skip a tile.
    #[arg(long, default_value_t = 0.3, global = true)]
    reject_threshold: f64,
    /// Re-check uncertain tiles with enlarged crops and neighboring context.
    #[arg(long, default_value_t = 2, global = true)]
    refinement_passes: usize,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Evaluate a saved grid image. Prints a JSON report with one-based indices.
    Solve(SolveArgs),
    /// Open Chrome and solve a reCAPTCHA v2 checkbox/image challenge.
    Browser(BrowserArgs),
}

#[derive(Args)]
struct SolveArgs {
    /// Crop to the grid itself, excluding the challenge header.
    #[arg(long)]
    image: PathBuf,
    /// Exact challenge instruction, e.g. "Select all squares with bicycles".
    #[arg(long)]
    prompt: String,
    #[arg(long, default_value_t = 3)]
    rows: u32,
    #[arg(long, default_value_t = 3)]
    cols: u32,
    /// Also write the JSON report to a file.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Save a PNG: green selected tiles, amber uncertain tiles.
    #[arg(long)]
    annotate: Option<PathBuf>,
}

#[derive(Args)]
struct BrowserArgs {
    #[arg(long)]
    url: String,
    /// Existing server; otherwise ChromeDriver is downloaded/managed locally.
    #[arg(long)]
    webdriver_url: Option<String>,
    #[arg(long)]
    chrome_binary: Option<String>,
    #[arg(long)]
    headless: bool,
    /// Zero-based checkbox widget index on pages with multiple widgets.
    #[arg(long, default_value_t = 0)]
    widget_index: usize,
    #[arg(long, default_value_t = 20)]
    max_rounds: usize,
    /// Maximum challenge refreshes for unresolved or rejected grids.
    #[arg(long, default_value_t = 4)]
    max_reloads: usize,
    #[arg(long, default_value_t = 30)]
    wait_seconds: u64,
    /// A unique run folder is created here.
    #[arg(long, default_value = "artifacts")]
    artifact_dir: PathBuf,
    /// Also write the captured response token as plain text.
    #[arg(long)]
    token_output: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("Error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<u8> {
    let thresholds = Thresholds::new(cli.select_threshold, cli.reject_threshold)?;
    let timeout = positive_duration(cli.timeout_seconds)?;
    if let Command::Solve(args) = &cli.command {
        Grid::new(args.rows, args.cols)?;
    }
    let credentials = Credentials::load(&cli.secrets, &cli.provider, cli.endpoint.as_deref())?;
    let client = JevClient::new(credentials, timeout, cli.retries)?;
    let solver = Solver::new(client, cli.model, thresholds)?
        .with_refinement_passes(cli.refinement_passes)?;
    match cli.command {
        Command::Solve(args) => {
            let bytes = fs::read(&args.image)
                .with_context(|| format!("Cannot read {}", args.image.display()))?;
            let solution = solver
                .solve(&bytes, &args.prompt, Grid::new(args.rows, args.cols)?)
                .await?;
            let json = serde_json::to_string_pretty(&solution)?;
            if let Some(path) = args.output {
                fs::write(&path, &json)
                    .with_context(|| format!("Cannot write {}", path.display()))?;
            }
            if let Some(path) = args.annotate {
                fs::write(&path, annotate(&bytes, &solution)?)
                    .with_context(|| format!("Cannot write {}", path.display()))?;
            }
            println!("{json}");
            Ok(if solution.needs_review() { 2 } else { 0 })
        }
        Command::Browser(args) => {
            let options = BrowserOptions {
                url: args.url,
                webdriver_url: args.webdriver_url,
                chrome_binary: args.chrome_binary,
                headless: args.headless,
                widget_index: args.widget_index,
                max_rounds: args.max_rounds,
                max_reloads: args.max_reloads,
                wait_timeout: positive_duration(args.wait_seconds)?,
                artifact_dir: args.artifact_dir.join(uuid::Uuid::new_v4().to_string()),
            };
            let report = browser::run(&solver, &options).await?;
            if let Some(path) = args.token_output
                && let Some(token) = &report.token
            {
                fs::write(&path, token)
                    .with_context(|| format!("Cannot write token to {}", path.display()))?;
            }
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(match report.status {
                BrowserStatus::Solved => 0,
                BrowserStatus::NeedsReview => 2,
                _ => 1,
            })
        }
    }
}
