use std::{fs, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use thirtyfour::{common::capabilities::chromium::ChromiumLikeCapabilities, prelude::*};
use tokio::time::{Instant, sleep};

use crate::{
    client::Usage,
    solver::{Grid, Solution, Solver, annotate},
};

const ANCHOR_FRAME: &str = "iframe[src*='/recaptcha/'][src*='/anchor']";
const CHALLENGE_FRAME: &str = "iframe[src*='/recaptcha/'][src*='/bframe']";
const TABLE: &str = ".rc-imageselect-table-33, .rc-imageselect-table-44";
const CELLS: &str = ".rc-imageselect-table-33 td, .rc-imageselect-table-44 td";
const POLL: Duration = Duration::from_millis(150);
const STABLE_FOR: Duration = Duration::from_millis(450);
const READ_CHALLENGE: &str = include_str!("scripts/read_challenge.js");

#[derive(Debug, Clone)]
pub struct BrowserOptions {
    pub url: String,
    pub webdriver_url: Option<String>,
    pub chrome_binary: Option<String>,
    pub headless: bool,
    pub widget_index: usize,
    pub max_rounds: usize,
    pub max_reloads: usize,
    pub wait_timeout: Duration,
    pub artifact_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserStatus {
    Solved,
    NeedsReview,
    Failed,
    LimitReached,
    Blocked,
    Unsupported,
    Cancelled,
}

#[derive(Debug, Serialize)]
pub struct Round {
    pub number: usize,
    pub dynamic: bool,
    pub action: String,
    pub solution: Solution,
}

#[derive(Debug, Serialize)]
pub struct BrowserReport {
    pub page_url: String,
    pub status: BrowserStatus,
    pub reason: String,
    pub token: Option<String>,
    pub rounds: Vec<Round>,
    pub reloads: usize,
    pub usage: Usage,
    pub elapsed_ms: u128,
    pub artifact_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct ChallengeMeta {
    prompt: String,
    rows: u32,
    cols: u32,
    dynamic: bool,
    ready: bool,
    geometry: serde_json::Value,
    moving: bool,
    identities: Vec<String>,
    selected: Vec<bool>,
    error: String,
    blocked: bool,
    unsupported: bool,
    visual_state: serde_json::Value,
}

impl ChallengeMeta {
    fn same_images(&self, other: &Self) -> bool {
        self.prompt == other.prompt
            && self.rows == other.rows
            && self.cols == other.cols
            && self.identities == other.identities
    }
}

/// Uses a fresh Chrome session and manages ChromeDriver unless a remote URL is
/// supplied. Chrome itself must already be installed.
pub async fn run(solver: &Solver, options: &BrowserOptions) -> Result<BrowserReport> {
    ensure!(
        options.max_rounds > 0 && options.max_rounds <= 100,
        "max-rounds must be between 1 and 100"
    );
    ensure!(
        options.max_reloads <= 20,
        "At most 20 challenge reloads are supported"
    );
    ensure!(
        !options.wait_timeout.is_zero(),
        "Browser wait timeout must be positive"
    );
    let url = reqwest::Url::parse(&options.url).context("Invalid browser URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "Browser URL must use HTTP or HTTPS"
    );
    fs::create_dir_all(&options.artifact_dir)
        .context("Cannot create browser artifact directory")?;
    let mut caps = DesiredCapabilities::chrome();
    caps.add_arg("--window-size=1280,1000")?;
    caps.add_arg("--lang=en-US")?;
    if options.headless {
        caps.add_arg("--headless=new")?;
    }
    if let Some(binary) = &options.chrome_binary {
        caps.set_binary(binary)?;
    }
    let driver = match &options.webdriver_url {
        Some(url) => WebDriver::new(url, caps).await.context("Cannot connect to WebDriver; start chromedriver --port=9515 or omit --webdriver-url")?,
        None => WebDriver::managed(caps).await.context("Cannot start Chrome; install Chrome or supply --webdriver-url for an existing WebDriver server")?,
    };
    let started = Instant::now();
    let mut report = BrowserReport {
        page_url: options.url.clone(),
        status: BrowserStatus::Failed,
        reason: String::new(),
        token: None,
        rounds: Vec::new(),
        reloads: 0,
        usage: Usage::default(),
        elapsed_ms: 0,
        artifact_dir: options.artifact_dir.clone(),
    };
    let outcome = tokio::select! {
        result = solve_session(&driver, solver, options, &mut report) => result,
        result = tokio::signal::ctrl_c() => {
            result.context("Could not listen for Ctrl-C")
                .map(|()| (BrowserStatus::Cancelled, "Interrupted by Ctrl-C".to_owned()))
        }
    };
    match outcome {
        Ok((status, reason)) => {
            report.status = status;
            report.reason = reason;
        }
        Err(error) => {
            report.status = BrowserStatus::Failed;
            report.reason = format!("{error:#}");
        }
    }
    // A checked anchor can precede the asynchronous response callback. Only a
    // nonempty response value satisfies the CLI's token-returning contract.
    if report.status == BrowserStatus::Solved && report.token.is_none() {
        match wait_for_token(&driver, options.widget_index, options.wait_timeout).await {
            Ok(Some(token)) => report.token = Some(token),
            Ok(None) => {
                report.status = BrowserStatus::Failed;
                report.reason =
                    "Checkbox completed, but no response token was exposed before the timeout"
                        .to_owned();
            }
            Err(error) => {
                report.status = BrowserStatus::Failed;
                report.reason = format!("Could not capture response token: {error:#}");
            }
        }
    }
    // Always tear down the session, including failure and cancellation paths.
    let observation = if enter_challenge(&driver).await.unwrap_or(false) {
        read_challenge(&driver).await.ok()
    } else {
        None
    };
    let screenshot = driver.screenshot_as_png().await.ok();
    if let Err(error) = driver.quit().await {
        eprintln!("Could not close WebDriver session: {error}");
    }
    if let Some(png) = screenshot {
        fs::write(options.artifact_dir.join("final.png"), png)?;
    }
    if let Some(observation) = observation {
        fs::write(
            options.artifact_dir.join("final-observation.json"),
            serde_json::to_vec_pretty(&observation)?,
        )?;
    }
    report.elapsed_ms = started.elapsed().as_millis();
    fs::write(
        options.artifact_dir.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(report)
}

async fn solve_session(
    driver: &WebDriver,
    solver: &Solver,
    options: &BrowserOptions,
    report: &mut BrowserReport,
) -> Result<(BrowserStatus, String)> {
    driver.set_implicit_wait_timeout(Duration::ZERO).await?;
    driver.set_page_load_timeout(options.wait_timeout).await?;
    driver.goto(&options.url).await?;
    let deadline = Instant::now() + options.wait_timeout;
    loop {
        if enter_anchor(driver, options.widget_index).await? {
            let checkbox = driver.find(By::Id("recaptcha-anchor")).await?;
            if checkbox.attr("aria-checked").await?.as_deref() != Some("true") {
                checkbox.click().await?;
            }
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "No reCAPTCHA v2 checkbox iframe found; v3 scores and invisible widgets are unsupported"
        );
        sleep(POLL).await;
    }

    let mut pending_replacements: Vec<(usize, String)> = Vec::new();
    let mut pending_since: Option<Instant> = None;
    let mut last_verified: Option<ChallengeMeta> = None;
    for number in 1..=options.max_rounds {
        if let Some(token) = wait_for_outcome(driver, options).await? {
            report.token = (!token.is_empty()).then_some(token);
            return Ok((
                BrowserStatus::Solved,
                "reCAPTCHA checkbox is checked or a response token is present".to_owned(),
            ));
        }
        let mut meta = stable_challenge(driver, options.wait_timeout).await?;
        if meta.blocked {
            return Ok((
                BrowserStatus::Blocked,
                "reCAPTCHA reported an automated-query or access block".to_owned(),
            ));
        }
        if meta.unsupported {
            return Ok((
                BrowserStatus::Unsupported,
                "Only reCAPTCHA v2 3x3 and 4x4 image grids are supported".to_owned(),
            ));
        }
        if let Some(previous) = &last_verified
            && meta.same_images(previous)
        {
            let reason = if meta.error.is_empty() {
                "Verification did not advance the challenge".to_owned()
            } else {
                format!("reCAPTCHA rejected the selection: {}", meta.error)
            };
            if report.reloads < options.max_reloads
                && reload_challenge(driver, options, &meta).await?
            {
                report.reloads += 1;
                eprintln!(
                    "  {reason}; requesting a fresh challenge ({}/{})",
                    report.reloads, options.max_reloads
                );
                if let Some(round) = report.rounds.last_mut() {
                    round.action = "select_verify_reload_rejected".to_owned();
                }
                last_verified = None;
                pending_replacements.clear();
                pending_since = None;
                continue;
            }
            return Ok((BrowserStatus::Failed, reason));
        }
        last_verified = None;

        // CSS animation can finish before the replacement image arrives. Wait
        // for every clicked image identity to change, not just its selected CSS.
        if !pending_replacements.is_empty() {
            let deadline = pending_since.unwrap_or_else(Instant::now) + options.wait_timeout;
            let previous_prompt = meta.prompt.clone();
            while pending_replacements
                .iter()
                .any(|(index, identity)| meta.identities.get(index - 1) == Some(identity))
            {
                ensure!(
                    Instant::now() < deadline,
                    "Timed out waiting for replacement tiles; no duplicate clicks were sent"
                );
                sleep(POLL).await;
                meta = stable_challenge(driver, deadline.saturating_duration_since(Instant::now()))
                    .await?;
                if meta.prompt != previous_prompt {
                    break;
                }
            }
            pending_replacements.clear();
            pending_since = None;
        }

        let table = driver.find(By::Css(TABLE)).await?;
        let png = table.screenshot_as_png().await?;
        let after_capture = read_challenge(driver).await?;
        if !meta.same_images(&after_capture)
            || !after_capture.ready
            || meta.geometry != after_capture.geometry
        {
            continue;
        }
        let grid = Grid::new(meta.rows, meta.cols)?;
        eprintln!(
            "Round {number}: {} ({}x{}, {})",
            meta.prompt.replace('\n', " "),
            grid.rows,
            grid.cols,
            if meta.dynamic {
                "replacement tiles"
            } else {
                "static grid"
            }
        );
        fs::write(
            options.artifact_dir.join(format!("round-{number:02}.png")),
            &png,
        )?;
        fs::write(
            options
                .artifact_dir
                .join(format!("round-{number:02}-observation.json")),
            serde_json::to_vec_pretty(&meta)?,
        )?;
        let solution = solver.solve(&png, &meta.prompt, grid).await?;
        report.usage.add(&solution.usage);
        fs::write(
            options.artifact_dir.join(format!("round-{number:02}.json")),
            serde_json::to_vec_pretty(&solution)?,
        )?;
        fs::write(
            options
                .artifact_dir
                .join(format!("round-{number:02}-annotated.png")),
            annotate(&png, &solution)?,
        )?;

        if let Some(token) = solved(driver, options.widget_index).await? {
            report.token = (!token.is_empty()).then_some(token);
            report.rounds.push(Round {
                number,
                dynamic: meta.dynamic,
                action: "widget_completed_during_inference".to_owned(),
                solution,
            });
            return Ok((
                BrowserStatus::Solved,
                "Widget completed during inference".to_owned(),
            ));
        }
        if !enter_challenge(driver).await? {
            eprintln!("  Challenge closed during inference; re-observing the checkbox");
            report.rounds.push(Round {
                number,
                dynamic: meta.dynamic,
                action: "discarded_hidden_challenge".to_owned(),
                solution,
            });
            continue;
        }
        let current = read_challenge(driver).await?;
        if !meta.same_images(&current) || !current.ready || meta.selected != current.selected {
            eprintln!(
                "  Discarding stale result: images_changed={}, ready={}, moving={}",
                !meta.same_images(&current),
                current.ready,
                current.moving
            );
            fs::write(
                options
                    .artifact_dir
                    .join(format!("round-{number:02}-stale-observation.json")),
                serde_json::to_vec_pretty(&current)?,
            )?;
            report.rounds.push(Round {
                number,
                dynamic: meta.dynamic,
                action: "discarded_stale_result".to_owned(),
                solution,
            });
            continue;
        }
        if solution.needs_review() {
            let reason = format!(
                "Tiles {:?} fall between the selection and rejection thresholds; review the saved annotated grid",
                solution.uncertain
            );
            let reloaded = report.reloads < options.max_reloads
                && reload_challenge(driver, options, &meta).await?;
            report.rounds.push(Round {
                number,
                dynamic: meta.dynamic,
                action: if reloaded {
                    "reload_uncertain"
                } else {
                    "needs_review"
                }
                .to_owned(),
                solution,
            });
            if reloaded {
                report.reloads += 1;
                eprintln!(
                    "  Still uncertain after refinement; requesting a fresh challenge ({}/{})",
                    report.reloads, options.max_reloads
                );
                continue;
            }
            return Ok((BrowserStatus::NeedsReview, reason));
        }
        let selected = solution.selected.clone();
        eprintln!(
            "  {}: select {:?}, {} input tokens, {}ms",
            solution.model, selected, solution.usage.input_tokens, solution.elapsed_ms
        );
        report.rounds.push(Round {
            number,
            dynamic: meta.dynamic,
            action: if meta.dynamic && !selected.is_empty() {
                "click_and_wait_for_replacements"
            } else {
                "select_and_verify"
            }
            .to_owned(),
            solution,
        });

        for index in &selected {
            let live = read_challenge(driver).await?;
            ensure!(
                live.prompt == meta.prompt
                    && live.rows == meta.rows
                    && live.cols == meta.cols
                    && live.identities.get(index - 1) == meta.identities.get(index - 1),
                "Grid changed before clicking tile {index}; stopped to avoid a stale click"
            );
            let cells = driver.find_all(By::Css(CELLS)).await?;
            let cell = cells
                .get(index - 1)
                .context("Tile disappeared before click")?;
            if live.selected.get(index - 1) == Some(&true) {
                continue;
            }
            cell.click().await?;
            if meta.dynamic {
                pending_replacements.push((*index, meta.identities[index - 1].clone()));
            }
        }
        if meta.dynamic && !selected.is_empty() {
            pending_since = Some(Instant::now());
        } else {
            driver
                .find(By::Id("recaptcha-verify-button"))
                .await?
                .click()
                .await?;
            last_verified = Some(meta);
            wait_after_verify(driver, options, last_verified.as_ref().unwrap()).await?;
        }
    }
    if let Some(token) = solved(driver, options.widget_index).await? {
        report.token = (!token.is_empty()).then_some(token);
        return Ok((
            BrowserStatus::Solved,
            "reCAPTCHA completed on the last allowed round".to_owned(),
        ));
    }
    Ok((
        BrowserStatus::LimitReached,
        format!(
            "Stopped after {} bounded browser rounds",
            options.max_rounds
        ),
    ))
}

async fn enter_anchor(driver: &WebDriver, index: usize) -> Result<bool> {
    driver.enter_default_frame().await?;
    let frames = driver.find_all(By::Css(ANCHOR_FRAME)).await?;
    if let Some(frame) = frames.get(index) {
        frame.clone().enter_frame().await?;
        Ok(true)
    } else {
        Ok(false)
    }
}

async fn enter_challenge(driver: &WebDriver) -> Result<bool> {
    driver.enter_default_frame().await?;
    for frame in driver.find_all(By::Css(CHALLENGE_FRAME)).await? {
        let rect = frame.rect().await?;
        // Closed reCAPTCHA dialogs can be parked at -10000px while WebDriver
        // still considers them displayed.
        if frame.is_displayed().await? && rect.x + rect.width > 0.0 && rect.y + rect.height > 0.0 {
            frame.enter_frame().await?;
            return Ok(true);
        }
    }
    Ok(false)
}

async fn solved(driver: &WebDriver, widget_index: usize) -> Result<Option<String>> {
    driver.enter_default_frame().await?;
    let result = driver.execute(
        "const nodes = document.querySelectorAll('textarea[name=\"g-recaptcha-response\"]'); return nodes[arguments[0]]?.value?.trim() || '';",
        vec![json!(widget_index)]).await?;
    let token: String = result.convert()?;
    if !token.is_empty() {
        return Ok(Some(token));
    }
    if enter_anchor(driver, widget_index).await? {
        let checkbox = driver.find(By::Id("recaptcha-anchor")).await?;
        if checkbox.attr("aria-checked").await?.as_deref() == Some("true") {
            return Ok(Some(String::new()));
        }
    }
    Ok(None)
}

async fn wait_for_token(
    driver: &WebDriver,
    widget_index: usize,
    timeout: Duration,
) -> Result<Option<String>> {
    let deadline = Instant::now() + timeout;
    loop {
        driver.enter_default_frame().await?;
        let token: String = driver
            .execute(
                r#"
            const index = arguments[0];
            const field = document.querySelectorAll('textarea[name="g-recaptcha-response"]')[index];
            if (field?.value?.trim()) return field.value.trim();
            try {
                if (index === 0 && typeof window.grecaptcha?.getResponse === 'function')
                    return window.grecaptcha.getResponse()?.trim() || '';
            } catch (_) {}
            return '';
        "#,
                vec![json!(widget_index)],
            )
            .await?
            .convert()?;
        if !token.is_empty() {
            return Ok(Some(token));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        sleep(POLL).await;
    }
}

async fn reload_challenge(
    driver: &WebDriver,
    options: &BrowserOptions,
    previous: &ChallengeMeta,
) -> Result<bool> {
    let buttons = driver.find_all(By::Id("recaptcha-reload-button")).await?;
    let Some(button) = buttons.first() else {
        return Ok(false);
    };
    if !button.is_displayed().await? || !button.is_enabled().await? {
        return Ok(false);
    }
    button.click().await?;
    let deadline = Instant::now() + options.wait_timeout;
    loop {
        let meta = read_challenge(driver).await?;
        if meta.blocked || meta.unsupported || (meta.ready && !meta.same_images(previous)) {
            return Ok(true);
        }
        ensure!(
            Instant::now() < deadline,
            "Reload did not produce a fresh challenge within the browser timeout"
        );
        sleep(POLL).await;
    }
}

async fn wait_for_outcome(driver: &WebDriver, options: &BrowserOptions) -> Result<Option<String>> {
    let deadline = Instant::now() + options.wait_timeout;
    let reopen_after = Instant::now() + Duration::from_secs(1);
    let mut reopened = false;
    loop {
        if let Some(token) = solved(driver, options.widget_index).await? {
            return Ok(Some(token));
        }
        if enter_challenge(driver).await? {
            return Ok(None);
        }
        if !reopened
            && Instant::now() >= reopen_after
            && enter_anchor(driver, options.widget_index).await?
        {
            let checkbox = driver.find(By::Id("recaptcha-anchor")).await?;
            let classes = checkbox.attr("class").await?.unwrap_or_default();
            if !classes.contains("loading")
                && checkbox.attr("aria-busy").await?.as_deref() != Some("true")
            {
                eprintln!("  Reopening a closed or expired challenge");
                checkbox.click().await?;
                reopened = true;
            }
        }
        ensure!(
            Instant::now() < deadline,
            "Timed out waiting for a challenge or completed checkbox"
        );
        sleep(POLL).await;
    }
}

async fn read_challenge(driver: &WebDriver) -> Result<ChallengeMeta> {
    let result = driver.execute(READ_CHALLENGE, Vec::new()).await?;
    result
        .convert()
        .context("Cannot read reCAPTCHA challenge metadata")
}

async fn stable_challenge(driver: &WebDriver, timeout: Duration) -> Result<ChallengeMeta> {
    let deadline = Instant::now() + timeout;
    let mut previous: Option<ChallengeMeta> = None;
    let mut stable_since = Instant::now();
    loop {
        let meta = read_challenge(driver).await?;
        if meta.blocked || meta.unsupported {
            return Ok(meta);
        }
        let unchanged = meta.ready
            && previous.as_ref().is_some_and(|old| {
                old.ready
                    && old.same_images(&meta)
                    && old.selected == meta.selected
                    && old.geometry == meta.geometry
            });
        if !unchanged {
            stable_since = Instant::now();
        }
        if unchanged && stable_since.elapsed() >= STABLE_FOR {
            return Ok(meta);
        }
        previous = Some(meta);
        if Instant::now() >= deadline {
            bail!("Timed out waiting for a stable, fully loaded image grid");
        }
        sleep(POLL).await;
    }
}

async fn wait_after_verify(
    driver: &WebDriver,
    options: &BrowserOptions,
    previous: &ChallengeMeta,
) -> Result<()> {
    let deadline = Instant::now() + options.wait_timeout;
    loop {
        if solved(driver, options.widget_index).await?.is_some() {
            return Ok(());
        }
        if enter_challenge(driver).await? {
            let meta = read_challenge(driver).await?;
            if meta.blocked
                || meta.unsupported
                || !meta.error.is_empty()
                || (meta.ready && !meta.same_images(previous))
            {
                return Ok(());
            }
        } else {
            // The main outcome loop waits for callbacks or reopens an expired
            // widget instead of clicking a hidden challenge iframe.
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "Verification did not produce a result within the browser timeout"
        );
        sleep(POLL).await;
    }
}
