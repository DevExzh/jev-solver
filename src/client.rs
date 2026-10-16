use std::{
    collections::BTreeMap,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, bail, ensure};
use reqwest::{
    Client, StatusCode,
    header::{AUTHORIZATION, HeaderValue},
    multipart::{Form, Part},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::{Instant, sleep};

use crate::config::Credentials;

#[derive(Debug, Serialize)]
pub struct NoulQuestion {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub instructions: Value,
    pub criteria: Value,
}

#[derive(Debug, Serialize)]
pub struct ImageAsset {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub part: String,
}

#[derive(Debug, Serialize)]
pub struct EvaluationRequest {
    pub model: String,
    pub state: Value,
    pub questions: BTreeMap<String, NoulQuestion>,
    pub assets: BTreeMap<String, ImageAsset>,
}

pub struct Upload {
    pub part: String,
    pub png: Vec<u8>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Noul {
        noul: f64,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, other: &Self) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
    }
}

#[derive(Debug, Deserialize)]
pub struct EvaluationResponse {
    pub model: String,
    pub answers: BTreeMap<String, Answer>,
    pub usage: Usage,
}

#[derive(Clone)]
pub struct JevClient {
    http: Client,
    endpoint: reqwest::Url,
    timeout: Duration,
    retries: usize,
}

impl JevClient {
    pub fn new(credentials: Credentials, timeout: Duration, retries: usize) -> Result<Self> {
        ensure!(!timeout.is_zero(), "Request timeout must be positive");
        ensure!(retries <= 8, "At most 8 retries are supported");
        let mut authorization = HeaderValue::from_str(&format!("Bearer {}", credentials.api_key))
            .map_err(|_| anyhow::anyhow!("Invalid characters in API key"))?;
        authorization.set_sensitive(true);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(AUTHORIZATION, authorization);
        headers.insert(
            "TypeSafe-Danger-Zone-Images",
            HeaderValue::from_static("true"),
        );
        let http = Client::builder()
            .default_headers(headers)
            .user_agent(concat!("jev-solver/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .connect_timeout(Duration::from_secs(10).min(timeout))
            .build()
            .context("Could not create the TypeSafe HTTP client")?;
        Ok(Self {
            http,
            endpoint: credentials.endpoint,
            timeout,
            retries,
        })
    }

    /// Retry only explicit overload/rate-limit/service responses. A transport
    /// error may have occurred after billing, so it is not retried blindly.
    pub async fn evaluate(
        &self,
        request: &EvaluationRequest,
        uploads: &[Upload],
    ) -> Result<EvaluationResponse> {
        let json = serde_json::to_string(request)?;
        let size: usize = json.len() + uploads.iter().map(|u| u.png.len() + 1024).sum::<usize>();
        ensure!(
            size < 32 * 1024 * 1024 - 1024,
            "Request exceeds TypeSafe's 32 MiB body limit"
        );
        let deadline = Instant::now() + self.timeout;
        for attempt in 0..=self.retries {
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !remaining.is_zero(),
                "TypeSafe request timed out, including retries"
            );
            // Multipart streams are consumed by send; rebuild for each attempt.
            let mut form = Form::new().part(
                "request",
                Part::text(json.clone()).mime_str("application/json")?,
            );
            for upload in uploads {
                form = form.part(
                    upload.part.clone(),
                    Part::bytes(upload.png.clone())
                        .file_name(format!("{}.png", upload.part))
                        .mime_str("image/png")?,
                );
            }
            let response = self
                .http
                .post(self.endpoint.clone())
                .timeout(remaining)
                .multipart(form)
                .send()
                .await
                .context("TypeSafe request failed")?;
            let status = response.status();
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|h| h.to_str().ok())
                .and_then(parse_retry_after);
            if status.is_success() {
                let response: EvaluationResponse = response
                    .json()
                    .await
                    .context("Invalid TypeSafe response JSON")?;
                ensure!(
                    !response.model.trim().is_empty(),
                    "TypeSafe response omitted the resolved model"
                );
                return Ok(response);
            }
            if retryable(status) && attempt < self.retries {
                let delay =
                    retry_after.unwrap_or(Duration::from_millis(500 * 2_u64.pow(attempt as u32)));
                ensure!(
                    delay < deadline.saturating_duration_since(Instant::now()),
                    "TypeSafe returned {status}; Retry-After exceeds the remaining request timeout"
                );
                eprintln!(
                    "TypeSafe returned {status}; retrying in {:.1}s",
                    delay.as_secs_f64()
                );
                sleep(delay).await;
                continue;
            }
            // Do not print response bodies: providers can echo inputs or headers.
            let advice = match status.as_u16() {
                400 | 422 => {
                    "request or image validation failed; check model support and image limits"
                }
                401 | 403 => "check the API key and access to the requested model",
                413 => "reduce image sizes or batch fewer tiles",
                429 | 529 => "rate limit or overload; try again later",
                _ => "provider request failed",
            };
            bail!("TypeSafe returned {status}: {advice}");
        }
        unreachable!("the final attempt always returns")
    }
}

fn retryable(status: StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504 | 529)
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    value
        .parse::<u64>()
        .map(Duration::from_secs)
        .ok()
        .or_else(|| {
            httpdate::parse_http_date(value)
                .ok()
                .map(|date| date.duration_since(SystemTime::now()).unwrap_or_default())
        })
}
