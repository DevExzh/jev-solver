use std::{env, fs, path::Path, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use reqwest::Url;
use serde::Deserialize;

pub const DEFAULT_MODEL: &str = "jev-1.14.0";
pub const DEFAULT_ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";

/// Never derive Debug or Serialize for a structure holding credentials.
pub struct Credentials {
    pub endpoint: Url,
    pub api_key: String,
}

#[derive(Deserialize)]
struct Secrets {
    #[serde(rename = "system-one")]
    providers: Vec<Provider>,
}

#[derive(Deserialize)]
struct Provider {
    name: String,
    endpoint: String,
    #[serde(rename = "api-key")]
    api_key: String,
}

impl Credentials {
    /// Environment credentials take priority. YAML is used only when needed.
    pub fn load(path: &Path, provider: &str, endpoint: Option<&str>) -> Result<Self> {
        let key = env::var("TYPESAFE_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty());
        if let Some(api_key) = key {
            return Self::new(endpoint.unwrap_or(DEFAULT_ENDPOINT), api_key);
        }
        let contents = fs::read_to_string(path).with_context(|| {
            format!(
                "Set TYPESAFE_API_KEY or configure provider {provider:?} in {}",
                path.display()
            )
        })?;
        // A YAML parse error can contain source snippets, including secrets.
        let secrets: Secrets = serde_yaml_ng::from_str(&contents).map_err(|_| {
            anyhow::anyhow!(
                "Invalid secrets YAML; expected system-one entries with name, endpoint, and api-key"
            )
        })?;
        let provider = secrets
            .providers
            .into_iter()
            .find(|entry| entry.name.eq_ignore_ascii_case(provider))
            .with_context(|| {
                format!("Provider {provider:?} was not found in {}", path.display())
            })?;
        Self::new(endpoint.unwrap_or(&provider.endpoint), provider.api_key)
    }

    pub fn new(endpoint: &str, api_key: String) -> Result<Self> {
        ensure!(!api_key.trim().is_empty(), "The TypeSafe API key is empty");
        let endpoint = Url::parse(endpoint).context("Invalid TypeSafe endpoint URL")?;
        let loopback = matches!(
            endpoint.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]")
        );
        ensure!(
            endpoint.scheme() == "https" || (endpoint.scheme() == "http" && loopback),
            "TypeSafe endpoints must use HTTPS (HTTP is accepted on loopback for tests)"
        );
        ensure!(
            endpoint.username().is_empty()
                && endpoint.password().is_none()
                && endpoint.query().is_none()
                && endpoint.fragment().is_none(),
            "Put credentials in TYPESAFE_API_KEY, not in the endpoint URL"
        );
        if !endpoint
            .path()
            .trim_end_matches('/')
            .ends_with("/systemone")
        {
            bail!(
                "Expected a TypeSafe /v1/systemone evaluation endpoint; OpenAI/OpenRouter chat endpoints use a different API"
            );
        }
        Ok(Self { endpoint, api_key })
    }
}

pub fn positive_duration(seconds: u64) -> Result<Duration> {
    ensure!(seconds > 0, "Timeout must be at least one second");
    Ok(Duration::from_secs(seconds))
}
